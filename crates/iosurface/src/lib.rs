//! IOSurface: a block of pixel memory with a size, a layout and a use count.
//!
//! On a Mac the memory is shared with the window server and the GPU. Here it is
//! ordinary process memory: the only consumers are this process's compositor
//! (`appkit`, through a layer's `contents`) and whatever the guest itself does
//! with the base address. What has to be exact is the layout a program reads
//! back and the use count, which is how a program such as Qt's layer backing
//! store decides whether a buffer is still on screen and it must draw into
//! another one.
//!
//! `IOSurfaceCreate` refuses planar surfaces (`kIOSurfacePlaneInfo`) rather
//! than laying them out as one plane. CoreVideo makes planar surfaces itself,
//! with the plane layout it worked out, through `planes::create_planar`; the
//! compositor does not show them.

#![allow(non_snake_case, clippy::missing_safety_doc)]

pub mod planes;

use std::alloc::Layout;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use corefoundation::base::{CFTypeID, CFTypeRef};
use objc::class::Class;
use objc::object::Id;

/// `IOReturn` values.
const SUCCESS: i32 = 0;
/// `kIOReturnBadArgument`.
const BAD_ARGUMENT: i32 = 0xe00002c2_u32 as i32;
/// `kIOReturnNotLocked`.
const NOT_LOCKED: i32 = 0xe00002cd_u32 as i32;

/// `kIOSurfaceLockReadOnly`: an unlock of this kind leaves the seed alone.
pub const LOCK_READ_ONLY: u32 = 1;

/// `kCVPixelFormatType_32BGRA`: premultiplied B, G, R, A in memory order.
pub const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// Row and allocation alignment. Any alignment is correct as long as
/// `IOSurfaceAlignProperty`, `IOSurfaceGetPropertyAlignment` and
/// `IOSurfaceCreate` agree on it; a cache line keeps rows friendly to SIMD
/// copies.
const ROW_ALIGNMENT: usize = 64;
const PAGE: usize = 4096;

/// The largest width or height `IOSurfaceGetPropertyMaximum` admits.
const MAX_EXTENT: usize = 16384;
const MAX_BYTES_PER_ELEMENT: usize = 16;

/// The property keys, as `(Mach-O name, text)`. The texts are Apple's: a
/// program may build a key with `CFSTR` instead of importing it, and the two
/// have to compare equal.
const KEYS: &[(&str, &str)] = &[
    ("_kIOSurfaceAllocSize", ALLOC_SIZE),
    ("_kIOSurfaceWidth", WIDTH),
    ("_kIOSurfaceHeight", HEIGHT),
    ("_kIOSurfaceBytesPerRow", BYTES_PER_ROW),
    ("_kIOSurfaceBytesPerElement", BYTES_PER_ELEMENT),
    ("_kIOSurfaceElementWidth", ELEMENT_WIDTH),
    ("_kIOSurfaceElementHeight", ELEMENT_HEIGHT),
    ("_kIOSurfaceOffset", OFFSET),
    ("_kIOSurfacePixelFormat", PIXEL_FORMAT),
    ("_kIOSurfacePlaneInfo", PLANE_INFO),
    ("_kIOSurfaceCacheMode", "IOSurfaceCacheMode"),
    ("_kIOSurfaceIsGlobal", "IOSurfaceIsGlobal"),
    (
        "_kIOSurfacePixelSizeCastingAllowed",
        "IOSurfacePixelSizeCastingAllowed",
    ),
];

const ALLOC_SIZE: &str = "IOSurfaceAllocSize";
const WIDTH: &str = "IOSurfaceWidth";
const HEIGHT: &str = "IOSurfaceHeight";
const BYTES_PER_ROW: &str = "IOSurfaceBytesPerRow";
const BYTES_PER_ELEMENT: &str = "IOSurfaceBytesPerElement";
const ELEMENT_WIDTH: &str = "IOSurfaceElementWidth";
const ELEMENT_HEIGHT: &str = "IOSurfaceElementHeight";
const OFFSET: &str = "IOSurfaceOffset";
const PIXEL_FORMAT: &str = "IOSurfacePixelFormat";
const PLANE_INFO: &str = "IOSurfacePlaneInfo";

/// The data symbols this crate publishes.
pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    corefoundation::base::string_globals(KEYS)
}

#[repr(C)]
struct Surface {
    isa: *const c_void,
    state: State,
}

struct State {
    id: u32,
    width: usize,
    height: usize,
    bytes_per_element: usize,
    bytes_per_row: usize,
    element_width: usize,
    element_height: usize,
    pixel_format: u32,
    offset: usize,
    alloc_size: usize,
    memory: *mut u8,
    layout: Layout,
    use_count: AtomicI32,
    seed: AtomicU32,
    locks: AtomicI32,
    /// `IOSurfaceSetValue`'s pairs, both retained.
    values: Mutex<Vec<(usize, usize)>>,
    /// Empty for a surface that is not planar.
    planes: Vec<planes::Plane>,
}

impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: allocated in `IOSurfaceCreate` with exactly this layout.
        unsafe { std::alloc::dealloc(self.memory, self.layout) };
        for (key, value) in self
            .values
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            release(key as CFTypeRef);
            release(value as CFTypeRef);
        }
    }
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        // Apple's `IOSurfaceRef` is toll-free bridged to the `IOSurface` class.
        let class = corefoundation::base::register(c"IOSurface", None, size_of::<Surface>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

/// Every live surface by `IOSurfaceID`, for `IOSurfaceLookup`.
static IDS: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` of an `IOSurface`, whose body is `Surface`.
    let surface = unsafe { &mut *(receiver as *mut Surface) };
    IDS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, object)| *object != receiver as usize);
    // SAFETY: initialized by `IOSurfaceCreate` and dropped only here.
    unsafe { std::ptr::drop_in_place(&mut surface.state) };
    objc::arc::finalize(receiver);
}

fn state<'a>(surface: CFTypeRef) -> Option<&'a State> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_surface =
        !surface.is_null() && unsafe { corefoundation::base::is_kind_of(surface, class()) };
    // SAFETY: checked to be an `IOSurface`, whose body is `Surface`.
    is_surface.then(|| unsafe { &(*(surface as *const Surface)).state })
}

fn release(object: CFTypeRef) {
    if !object.is_null() {
        // SAFETY: an object this crate retained.
        unsafe { corefoundation::api::CFRelease(object) };
    }
}

fn retain(object: CFTypeRef) -> CFTypeRef {
    match object.is_null() {
        true => object,
        // SAFETY: a live CF object.
        false => unsafe { corefoundation::api::CFRetain(object) },
    }
}

fn key(text: &str) -> CFTypeRef {
    corefoundation::base::interned_name(text) as usize as CFTypeRef
}

/// An integer property of `properties`, or `None` where it is absent or not a
/// number.
fn number(properties: CFTypeRef, name: &str) -> Option<i64> {
    // SAFETY: a dictionary the caller passed, and a key this crate made.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(properties, key(name)) };
    // SAFETY: null or a CF object from the dictionary.
    unsafe { corefoundation::collections::number(value) }.map(|n| n.as_i64())
}

fn has(properties: CFTypeRef, name: &str) -> bool {
    // SAFETY: as in `number`.
    !unsafe { corefoundation::api::CFDictionaryGetValue(properties, key(name)) }.is_null()
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|v| v / alignment * alignment)
}

fn alignment_of(name: &str) -> usize {
    match name {
        BYTES_PER_ROW => ROW_ALIGNMENT,
        ALLOC_SIZE => PAGE,
        _ => 1,
    }
}

/// The name of a property key, if it is one of Apple's.
fn key_name(key: CFTypeRef) -> Option<&'static str> {
    KEYS.iter().map(|(_, text)| *text).find(|text| {
        // SAFETY: null or a CF object the caller passed.
        unsafe { corefoundation::api::CFEqual(key, self::key(text)) != 0 }
    })
}

fn refuse(what: &str) -> CFTypeRef {
    eprintln!("rosemary: IOSurfaceCreate: {what}; no surface is made");
    std::ptr::null()
}

/// `IOSurfaceRef IOSurfaceCreate(CFDictionaryRef properties)`.
pub unsafe extern "C" fn IOSurfaceCreate(properties: CFTypeRef) -> CFTypeRef {
    if properties.is_null() {
        return std::ptr::null();
    }
    if has(properties, PLANE_INFO) {
        return refuse("planar surfaces (kIOSurfacePlaneInfo) are not supported");
    }
    let positive = |name, default: i64| {
        let value = number(properties, name).unwrap_or(default);
        usize::try_from(value).ok().filter(|v| *v > 0)
    };
    let (Some(width), Some(height)) = (positive(WIDTH, 0), positive(HEIGHT, 0)) else {
        return std::ptr::null();
    };
    // Apple's documented defaults: one byte per element, one-by-one elements.
    let (Some(bytes_per_element), Some(element_width), Some(element_height)) = (
        positive(BYTES_PER_ELEMENT, 1),
        positive(ELEMENT_WIDTH, 1),
        positive(ELEMENT_HEIGHT, 1),
    ) else {
        return std::ptr::null();
    };
    if width > MAX_EXTENT || height > MAX_EXTENT || bytes_per_element > MAX_BYTES_PER_ELEMENT {
        return refuse(&format!(
            "{width}x{height} at {bytes_per_element} bytes per element is past the maximum"
        ));
    }
    let Some(minimum_row) = width.div_ceil(element_width).checked_mul(bytes_per_element) else {
        return std::ptr::null();
    };
    let requested_row = number(properties, BYTES_PER_ROW)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let Some(bytes_per_row) = align_up(requested_row.max(minimum_row), ROW_ALIGNMENT) else {
        return std::ptr::null();
    };
    let offset = number(properties, OFFSET)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let rows = height.div_ceil(element_height);
    let Some(needed) = bytes_per_row
        .checked_mul(rows)
        .and_then(|bytes| bytes.checked_add(offset))
    else {
        return std::ptr::null();
    };
    let requested_size = number(properties, ALLOC_SIZE)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let Some(alloc_size) = align_up(requested_size.max(needed), PAGE) else {
        return std::ptr::null();
    };
    let Ok(layout) = Layout::from_size_align(alloc_size, PAGE) else {
        return std::ptr::null();
    };
    // SAFETY: a non-zero size, since width and height are positive.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    if memory.is_null() {
        return refuse(&format!("{alloc_size} bytes could not be allocated"));
    }
    let pixel_format = number(properties, PIXEL_FORMAT).map_or(0, |v| v as u32);
    publish(State {
        id: 0,
        width,
        height,
        bytes_per_element,
        bytes_per_row,
        element_width,
        element_height,
        pixel_format,
        offset,
        alloc_size,
        memory,
        layout,
        use_count: AtomicI32::new(0),
        seed: AtomicU32::new(0),
        locks: AtomicI32::new(0),
        values: Mutex::new(Vec::new()),
        planes: Vec::new(),
    })
}

/// A new surface holding `state`, under a fresh `IOSurfaceID`.
fn publish(mut state: State) -> CFTypeRef {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    state.id = id;
    // SAFETY: `Surface` is the registered layout.
    let surface = unsafe { corefoundation::base::allocate::<Surface>(class()) };
    // SAFETY: a fresh, zeroed allocation whose state is written once here.
    unsafe { std::ptr::write(&mut (*surface).state, state) };
    IDS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((id, surface as usize));
    surface as CFTypeRef
}

/// `IOSurfaceRef IOSurfaceLookup(IOSurfaceID)`: retained, as the name's
/// "create rule" sibling says.
pub unsafe extern "C" fn IOSurfaceLookup(id: u32) -> CFTypeRef {
    let ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
    ids.iter()
        .find(|(candidate, _)| *candidate == id)
        .map_or(std::ptr::null(), |(_, object)| retain(*object as CFTypeRef))
}

pub unsafe extern "C" fn IOSurfaceGetID(surface: CFTypeRef) -> u32 {
    state(surface).map_or(0, |s| s.id)
}

pub unsafe extern "C" fn IOSurfaceGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(class())
}

/// `IOReturn IOSurfaceLock(IOSurfaceRef, IOSurfaceLockOptions, uint32_t *seed)`.
pub unsafe extern "C" fn IOSurfaceLock(surface: CFTypeRef, _options: u32, seed: *mut u32) -> i32 {
    let Some(state) = state(surface) else {
        return BAD_ARGUMENT;
    };
    state.locks.fetch_add(1, Ordering::AcqRel);
    if !seed.is_null() {
        // SAFETY: the caller's out-pointer.
        unsafe { *seed = state.seed.load(Ordering::Acquire) };
    }
    SUCCESS
}

/// `IOSurfaceUnlock`: a write lock ending is a modification, which is what the
/// seed counts.
pub unsafe extern "C" fn IOSurfaceUnlock(surface: CFTypeRef, options: u32, seed: *mut u32) -> i32 {
    let Some(state) = state(surface) else {
        return BAD_ARGUMENT;
    };
    let held = state
        .locks
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n > 0).then(|| n - 1)
        });
    if held.is_err() {
        return NOT_LOCKED;
    }
    if options & LOCK_READ_ONLY == 0 {
        state.seed.fetch_add(1, Ordering::AcqRel);
    }
    if !seed.is_null() {
        // SAFETY: the caller's out-pointer.
        unsafe { *seed = state.seed.load(Ordering::Acquire) };
    }
    SUCCESS
}

pub unsafe extern "C" fn IOSurfaceGetSeed(surface: CFTypeRef) -> u32 {
    state(surface).map_or(0, |s| s.seed.load(Ordering::Acquire))
}

pub unsafe extern "C" fn IOSurfaceGetAllocSize(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.alloc_size)
}

pub unsafe extern "C" fn IOSurfaceGetWidth(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.width)
}

pub unsafe extern "C" fn IOSurfaceGetHeight(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.height)
}

pub unsafe extern "C" fn IOSurfaceGetBytesPerElement(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.bytes_per_element)
}

pub unsafe extern "C" fn IOSurfaceGetBytesPerRow(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.bytes_per_row)
}

pub unsafe extern "C" fn IOSurfaceGetElementWidth(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.element_width)
}

pub unsafe extern "C" fn IOSurfaceGetElementHeight(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.element_height)
}

pub unsafe extern "C" fn IOSurfaceGetPixelFormat(surface: CFTypeRef) -> u32 {
    state(surface).map_or(0, |s| s.pixel_format)
}

/// The base address, which a caller may use whether or not it holds a lock -
/// the lock is a coherency promise, not an access check.
pub unsafe extern "C" fn IOSurfaceGetBaseAddress(surface: CFTypeRef) -> *mut c_void {
    state(surface).map_or(std::ptr::null_mut(), |s| {
        // SAFETY: the offset lies inside the allocation, which covers it.
        unsafe { s.memory.add(s.offset) }.cast()
    })
}

pub unsafe extern "C" fn IOSurfaceIncrementUseCount(surface: CFTypeRef) {
    if let Some(state) = state(surface) {
        state.use_count.fetch_add(1, Ordering::AcqRel);
    }
}

pub unsafe extern "C" fn IOSurfaceDecrementUseCount(surface: CFTypeRef) {
    if let Some(state) = state(surface) {
        let _ = state
            .use_count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n > 0).then(|| n - 1)
            });
    }
}

pub unsafe extern "C" fn IOSurfaceGetUseCount(surface: CFTypeRef) -> i32 {
    state(surface).map_or(0, |s| s.use_count.load(Ordering::Acquire))
}

/// `Boolean IOSurfaceIsInUse(IOSurfaceRef)`: whether anything - here, a
/// layer showing it - holds a use count on it.
pub unsafe extern "C" fn IOSurfaceIsInUse(surface: CFTypeRef) -> u8 {
    u8::from(state(surface).is_some_and(|s| s.use_count.load(Ordering::Acquire) > 0))
}

pub unsafe extern "C" fn IOSurfaceSetValue(surface: CFTypeRef, key: CFTypeRef, value: CFTypeRef) {
    let Some(state) = state(surface) else {
        return;
    };
    if key.is_null() || value.is_null() {
        return;
    }
    let (key, value) = (retain(key) as usize, retain(value) as usize);
    let mut values = state.values.lock().unwrap_or_else(|e| e.into_inner());
    let found = values.iter_mut().find(|(k, _)| {
        // SAFETY: two live CF objects.
        unsafe { corefoundation::api::CFEqual(*k as CFTypeRef, key as CFTypeRef) != 0 }
    });
    match found {
        Some(entry) => {
            let old = std::mem::replace(entry, (key, value));
            drop(values);
            release(old.0 as CFTypeRef);
            release(old.1 as CFTypeRef);
        }
        None => values.push((key, value)),
    }
}

/// `CFTypeRef IOSurfaceCopyValue(IOSurfaceRef, CFStringRef)`, retained.
pub unsafe extern "C" fn IOSurfaceCopyValue(surface: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    let Some(state) = state(surface) else {
        return std::ptr::null();
    };
    let values = state.values.lock().unwrap_or_else(|e| e.into_inner());
    values
        .iter()
        // SAFETY: a live key and the caller's.
        .find(|(k, _)| unsafe { corefoundation::api::CFEqual(*k as CFTypeRef, key) != 0 })
        .map_or(std::ptr::null(), |(_, v)| retain(*v as CFTypeRef))
}

pub unsafe extern "C" fn IOSurfaceRemoveValue(surface: CFTypeRef, key: CFTypeRef) {
    let Some(state) = state(surface) else {
        return;
    };
    let removed: Vec<(usize, usize)> = {
        let mut values = state.values.lock().unwrap_or_else(|e| e.into_inner());
        let (gone, kept) = std::mem::take(&mut *values)
            .into_iter()
            .partition(|(k, _)| {
                // SAFETY: a live key and the caller's.
                unsafe { corefoundation::api::CFEqual(*k as CFTypeRef, key) != 0 }
            });
        *values = kept;
        gone
    };
    for (k, v) in removed {
        release(k as CFTypeRef);
        release(v as CFTypeRef);
    }
}

/// `IOSurfaceSetValues`: every pair of `dictionary`, over what is there.
pub unsafe extern "C" fn IOSurfaceSetValues(surface: CFTypeRef, dictionary: CFTypeRef) {
    // SAFETY: null or a dictionary the caller passed.
    let Some(pairs) =
        (unsafe { corefoundation::collections::dictionary(dictionary) }).map(|d| d.entries.clone())
    else {
        return;
    };
    for (key, value) in pairs {
        // SAFETY: entries of a live dictionary.
        unsafe { IOSurfaceSetValue(surface, key, value) };
    }
}

/// `CFDictionaryRef IOSurfaceCopyAllValues(IOSurfaceRef)`, retained.
pub unsafe extern "C" fn IOSurfaceCopyAllValues(surface: CFTypeRef) -> CFTypeRef {
    let Some(state) = state(surface) else {
        return std::ptr::null();
    };
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = state
        .values
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| (*k as CFTypeRef, *v as CFTypeRef))
        .collect();
    corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    )
}

pub unsafe extern "C" fn IOSurfaceRemoveAllValues(surface: CFTypeRef) {
    let Some(state) = state(surface) else {
        return;
    };
    let removed = std::mem::take(&mut *state.values.lock().unwrap_or_else(|e| e.into_inner()));
    for (k, v) in removed {
        release(k as CFTypeRef);
        release(v as CFTypeRef);
    }
}

/// `size_t IOSurfaceAlignProperty(CFStringRef, size_t)`: `value` rounded up to
/// what `IOSurfaceCreate` will make of it.
pub unsafe extern "C" fn IOSurfaceAlignProperty(property: CFTypeRef, value: usize) -> usize {
    let alignment = key_name(property).map_or(1, alignment_of);
    align_up(value, alignment).unwrap_or(value)
}

pub unsafe extern "C" fn IOSurfaceGetPropertyAlignment(property: CFTypeRef) -> usize {
    key_name(property).map_or(1, alignment_of)
}

pub unsafe extern "C" fn IOSurfaceGetPropertyMaximum(property: CFTypeRef) -> usize {
    let row = MAX_EXTENT * MAX_BYTES_PER_ELEMENT;
    match key_name(property) {
        Some(WIDTH | HEIGHT | ELEMENT_WIDTH | ELEMENT_HEIGHT) => MAX_EXTENT,
        Some(BYTES_PER_ELEMENT) => MAX_BYTES_PER_ELEMENT,
        Some(BYTES_PER_ROW) => row,
        Some(ALLOC_SIZE) => row * MAX_EXTENT,
        _ => 0,
    }
}

/// A surface's pixels as a compositor reads them.
#[derive(Clone, Copy, Debug)]
pub struct Pixels {
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
    pub bytes_per_element: usize,
    pub pixel_format: u32,
    pub data: *const u8,
}

/// `object`'s pixels, or `None` for something that is not a surface.
///
/// The pointer is valid for `bytes_per_row * height` bytes for as long as the
/// caller holds a reference to `object`.
///
/// `None` for a planar surface too, which the compositor does not show.
pub fn pixels(object: CFTypeRef) -> Option<Pixels> {
    let state = state(object).filter(|s| s.planes.is_empty())?;
    Some(Pixels {
        width: state.width,
        height: state.height,
        bytes_per_row: state.bytes_per_row,
        bytes_per_element: state.bytes_per_element,
        pixel_format: state.pixel_format,
        // SAFETY: the offset lies inside the allocation.
        data: unsafe { state.memory.add(state.offset) },
    })
}

/// Whether `object` is an `IOSurfaceRef`.
pub fn is_surface(object: CFTypeRef) -> bool {
    state(object).is_some()
}

exports::exports! {
    functions {
        "IOSurfaceCreate" => IOSurfaceCreate,
        "IOSurfaceLookup" => IOSurfaceLookup,
        "IOSurfaceGetID" => IOSurfaceGetID,
        "IOSurfaceGetTypeID" => IOSurfaceGetTypeID,
        "IOSurfaceLock" => IOSurfaceLock,
        "IOSurfaceUnlock" => IOSurfaceUnlock,
        "IOSurfaceGetSeed" => IOSurfaceGetSeed,
        "IOSurfaceGetAllocSize" => IOSurfaceGetAllocSize,
        "IOSurfaceGetWidth" => IOSurfaceGetWidth,
        "IOSurfaceGetHeight" => IOSurfaceGetHeight,
        "IOSurfaceGetBytesPerElement" => IOSurfaceGetBytesPerElement,
        "IOSurfaceGetBytesPerRow" => IOSurfaceGetBytesPerRow,
        "IOSurfaceGetElementWidth" => IOSurfaceGetElementWidth,
        "IOSurfaceGetElementHeight" => IOSurfaceGetElementHeight,
        "IOSurfaceGetPixelFormat" => IOSurfaceGetPixelFormat,
        "IOSurfaceGetBaseAddress" => IOSurfaceGetBaseAddress,
        "IOSurfaceIncrementUseCount" => IOSurfaceIncrementUseCount,
        "IOSurfaceDecrementUseCount" => IOSurfaceDecrementUseCount,
        "IOSurfaceGetUseCount" => IOSurfaceGetUseCount,
        "IOSurfaceIsInUse" => IOSurfaceIsInUse,
        "IOSurfaceSetValue" => IOSurfaceSetValue,
        "IOSurfaceCopyValue" => IOSurfaceCopyValue,
        "IOSurfaceRemoveValue" => IOSurfaceRemoveValue,
        "IOSurfaceSetValues" => IOSurfaceSetValues,
        "IOSurfaceCopyAllValues" => IOSurfaceCopyAllValues,
        "IOSurfaceRemoveAllValues" => IOSurfaceRemoveAllValues,
        "IOSurfaceAlignProperty" => IOSurfaceAlignProperty,
        "IOSurfaceGetPropertyAlignment" => IOSurfaceGetPropertyAlignment,
        "IOSurfaceGetPropertyMaximum" => IOSurfaceGetPropertyMaximum,
    }
    groups { planes }
}

#[cfg(test)]
mod tests;
