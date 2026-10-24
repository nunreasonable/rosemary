//! `CVPixelBuffer`: a picture in main memory, packed or planar.
//!
//! The memory is the heap's, or an IOSurface's when the attributes ask for
//! one with `kCVPixelBufferIOSurfacePropertiesKey`. Either way the layout is
//! [`crate::format::layout`]'s, so a planar buffer begins with its big-endian
//! plane info and the surface reports the same planes.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use corefoundation::base::{CFTypeID, CFTypeRef};
use objc::class::Class;

use crate::buffer;
use crate::format::{self, Layout};

/// `CVReturn` values.
pub const SUCCESS: i32 = 0;
pub const ERROR: i32 = -6660;
pub const INVALID_ARGUMENT: i32 = -6661;
pub const ALLOCATION_FAILED: i32 = -6662;
pub const INVALID_PIXEL_FORMAT: i32 = -6680;
pub const INVALID_SIZE: i32 = -6681;
pub const INVALID_PIXEL_BUFFER_ATTRIBUTES: i32 = -6682;

/// `kCVPixelBufferLock_ReadOnly`.
pub const LOCK_READ_ONLY: u64 = 1;

/// Pixel buffer attribute key texts, as `CVPixelBuffer.h` defines them.
pub const PIXEL_FORMAT_TYPE_KEY: &str = "PixelFormatType";
pub const WIDTH_KEY: &str = "Width";
pub const HEIGHT_KEY: &str = "Height";
pub const IOSURFACE_PROPERTIES_KEY: &str = "IOSurfaceProperties";
pub const BYTES_PER_ROW_ALIGNMENT_KEY: &str = "BytesPerRowAlignment";

/// How [`create`] should make a buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Back it with an IOSurface.
    pub iosurface: bool,
    /// Rows are aligned to this many bytes where it is above 64.
    pub row_alignment: usize,
}

enum Backing {
    Heap {
        ptr: *mut u8,
        layout: std::alloc::Layout,
    },
    /// A retained IOSurface.
    Surface(CFTypeRef),
}

struct State {
    format: u32,
    width: usize,
    height: usize,
    layout: Layout,
    backing: Backing,
    locks: AtomicI32,
}

impl Drop for State {
    fn drop(&mut self) {
        match self.backing {
            // SAFETY: allocated in `create` with exactly this layout.
            Backing::Heap { ptr, layout } => unsafe { std::alloc::dealloc(ptr, layout) },
            Backing::Surface(surface) => buffer::release(surface),
        }
    }
}

impl State {
    fn base(&self) -> *mut u8 {
        match self.backing {
            Backing::Heap { ptr, .. } => ptr,
            // SAFETY: the surface this buffer holds.
            Backing::Surface(surface) => {
                unsafe { iosurface::IOSurfaceGetBaseAddress(surface) }.cast()
            }
        }
    }

    fn plane(&self, index: usize) -> Option<&format::PlaneLayout> {
        match self.layout.is_planar() {
            true => self.layout.planes.get(index),
            false => None,
        }
    }
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        objc::class::ptr(buffer::register_subclass::<State>(c"CVPixelBuffer")) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

fn state<'a>(object: CFTypeRef) -> Option<&'a State> {
    buffer::state(object, class())
}

/// Whether `object` is a pixel buffer.
pub fn is_pixel_buffer(object: CFTypeRef) -> bool {
    state(object).is_some()
}

/// A new `width` by `height` pixel buffer of `format`, zeroed, with one
/// reference for the caller.
pub fn create(
    format: u32,
    width: usize,
    height: usize,
    options: Options,
) -> Result<CFTypeRef, i32> {
    if format::planes(format).is_none() {
        return Err(INVALID_PIXEL_FORMAT);
    }
    let Some(layout) = format::layout(format, width, height, options.row_alignment) else {
        return Err(INVALID_SIZE);
    };
    let backing = match options.iosurface {
        true => {
            let surface = match layout.is_planar() {
                true => {
                    let planes: Vec<iosurface::planes::PlaneSpec> = layout
                        .planes
                        .iter()
                        .map(|p| iosurface::planes::PlaneSpec {
                            offset: p.offset,
                            width: p.width,
                            height: p.height,
                            bytes_per_row: p.bytes_per_row,
                            bytes_per_element: p.bytes_per_element,
                        })
                        .collect();
                    iosurface::planes::create_planar(format, width, height, layout.size, &planes)
                }
                false => {
                    let plane = &layout.planes[0];
                    iosurface::planes::create_packed(
                        format,
                        width,
                        height,
                        plane.bytes_per_element,
                        plane.bytes_per_row,
                        layout.size,
                    )
                }
            };
            if surface.is_null() {
                return Err(ALLOCATION_FAILED);
            }
            Backing::Surface(surface)
        }
        false => {
            let Ok(heap) = std::alloc::Layout::from_size_align(layout.size, format::ALIGNMENT)
            else {
                return Err(INVALID_SIZE);
            };
            // SAFETY: a non-zero size, since the picture is not empty.
            let ptr = unsafe { std::alloc::alloc_zeroed(heap) };
            if ptr.is_null() {
                return Err(ALLOCATION_FAILED);
            }
            Backing::Heap { ptr, layout: heap }
        }
    };
    let state = State {
        format,
        width,
        height,
        layout,
        backing,
        locks: AtomicI32::new(0),
    };
    if state.layout.is_planar() {
        let header = state.layout.header_bytes();
        // SAFETY: the header fits in the first plane's offset, inside the
        // buffer's memory.
        unsafe { std::ptr::copy_nonoverlapping(header.as_ptr(), state.base(), header.len()) };
    }
    Ok(buffer::make(class(), state))
}

/// Each plane of `buffer` as its bytes and its row size: one for a packed
/// buffer. `None` for anything that is not a pixel buffer.
///
/// The slices are valid while the caller holds a reference to `buffer`, and
/// alias whatever else writes to it.
pub fn planes_mut<'a>(buffer: CFTypeRef) -> Option<Vec<(&'a mut [u8], usize)>> {
    let state = state(buffer)?;
    let base = state.base();
    Some(
        state
            .layout
            .planes
            .iter()
            .map(|p| {
                // SAFETY: the plane lies inside the buffer's memory, and the
                // planes do not overlap.
                let bytes = unsafe { std::slice::from_raw_parts_mut(base.add(p.offset), p.len()) };
                (bytes, p.bytes_per_row)
            })
            .collect(),
    )
}

/// The value of `name` in `attributes`, if it is there.
fn attribute(attributes: CFTypeRef, name: &str) -> CFTypeRef {
    if attributes.is_null() {
        return std::ptr::null();
    }
    let key = corefoundation::base::interned_name(name) as usize as CFTypeRef;
    // SAFETY: a dictionary the caller passed, and a key this crate made.
    unsafe { corefoundation::api::CFDictionaryGetValue(attributes, key) }
}

/// `CVReturn CVPixelBufferCreate(CFAllocatorRef, size_t width, size_t height,
/// OSType pixelFormatType, CFDictionaryRef pixelBufferAttributes,
/// CVPixelBufferRef *)`.
///
/// Of the attributes, only the IOSurface properties (their presence, any
/// dictionary) and the row alignment change anything; the rest are ignored,
/// as Apple ignores keys it does not know.
pub unsafe extern "C" fn CVPixelBufferCreate(
    _allocator: CFTypeRef,
    width: usize,
    height: usize,
    pixel_format: u32,
    attributes: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    let row_alignment = attribute(attributes, BYTES_PER_ROW_ALIGNMENT_KEY);
    let row_alignment = match row_alignment.is_null() {
        true => 0,
        // SAFETY: a CF object from the caller's dictionary.
        false => match unsafe { corefoundation::collections::number(row_alignment) } {
            Some(n) if (0..=4096).contains(&n.as_i64()) => n.as_i64() as usize,
            _ => return INVALID_PIXEL_BUFFER_ATTRIBUTES,
        },
    };
    let options = Options {
        iosurface: !attribute(attributes, IOSURFACE_PROPERTIES_KEY).is_null(),
        row_alignment,
    };
    let (made, status) = match create(pixel_format, width, height, options) {
        Ok(made) => (made, SUCCESS),
        Err(status) => {
            if status == INVALID_PIXEL_FORMAT {
                eprintln!(
                    "rosemary: CVPixelBufferCreate: pixel format {} is not supported",
                    format::name(pixel_format)
                );
            }
            (std::ptr::null(), status)
        }
    };
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(made) };
    status
}

/// `CFTypeID CVPixelBufferGetTypeID(void)`.
pub unsafe extern "C" fn CVPixelBufferGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(class())
}

/// `CVPixelBufferRef CVPixelBufferRetain(CVPixelBufferRef)`: null-safe.
pub unsafe extern "C" fn CVPixelBufferRetain(buffer: CFTypeRef) -> CFTypeRef {
    buffer::retain(buffer)
}

/// `void CVPixelBufferRelease(CVPixelBufferRef)`: null-safe.
pub unsafe extern "C" fn CVPixelBufferRelease(buffer: CFTypeRef) {
    buffer::release(buffer)
}

/// `CVReturn CVPixelBufferLockBaseAddress(CVPixelBufferRef, CVPixelBufferLockFlags)`.
///
/// The lock is a coherency promise; the memory is readable without one.
pub unsafe extern "C" fn CVPixelBufferLockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32 {
    let Some(state) = state(buffer) else {
        return INVALID_ARGUMENT;
    };
    if let Backing::Surface(surface) = state.backing {
        // SAFETY: the surface this buffer holds.
        unsafe {
            iosurface::IOSurfaceLock(
                surface,
                (flags & LOCK_READ_ONLY) as u32,
                std::ptr::null_mut(),
            )
        };
    }
    state.locks.fetch_add(1, Ordering::AcqRel);
    SUCCESS
}

/// `CVReturn CVPixelBufferUnlockBaseAddress(CVPixelBufferRef, CVPixelBufferLockFlags)`:
/// `kCVReturnError` for a buffer that is not locked.
pub unsafe extern "C" fn CVPixelBufferUnlockBaseAddress(buffer: CFTypeRef, flags: u64) -> i32 {
    let Some(state) = state(buffer) else {
        return INVALID_ARGUMENT;
    };
    let held = state
        .locks
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n > 0).then(|| n - 1)
        });
    if held.is_err() {
        return ERROR;
    }
    if let Backing::Surface(surface) = state.backing {
        // SAFETY: the surface this buffer holds.
        unsafe {
            iosurface::IOSurfaceUnlock(
                surface,
                (flags & LOCK_READ_ONLY) as u32,
                std::ptr::null_mut(),
            )
        };
    }
    SUCCESS
}

/// `void *CVPixelBufferGetBaseAddress(CVPixelBufferRef)`: the planar header,
/// for a planar buffer.
pub unsafe extern "C" fn CVPixelBufferGetBaseAddress(buffer: CFTypeRef) -> *mut c_void {
    state(buffer).map_or(std::ptr::null_mut(), |s| s.base().cast())
}

/// `size_t CVPixelBufferGetBytesPerRow(CVPixelBufferRef)`.
///
/// FIXME (ACCURACY): a planar buffer answers its first plane's row size; what
/// macOS answers for one is not verified.
pub unsafe extern "C" fn CVPixelBufferGetBytesPerRow(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.layout.planes[0].bytes_per_row)
}

pub unsafe extern "C" fn CVPixelBufferGetWidth(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.width)
}

pub unsafe extern "C" fn CVPixelBufferGetHeight(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.height)
}

pub unsafe extern "C" fn CVPixelBufferGetPixelFormatType(buffer: CFTypeRef) -> u32 {
    state(buffer).map_or(0, |s| s.format)
}

pub unsafe extern "C" fn CVPixelBufferGetDataSize(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.layout.size)
}

/// `IOSurfaceRef CVPixelBufferGetIOSurface(CVPixelBufferRef)`: not retained,
/// and null for a buffer on the heap.
pub unsafe extern "C" fn CVPixelBufferGetIOSurface(buffer: CFTypeRef) -> CFTypeRef {
    match state(buffer).map(|s| &s.backing) {
        Some(Backing::Surface(surface)) => *surface,
        _ => std::ptr::null(),
    }
}

pub unsafe extern "C" fn CVPixelBufferIsPlanar(buffer: CFTypeRef) -> u8 {
    u8::from(state(buffer).is_some_and(|s| s.layout.is_planar()))
}

/// `size_t CVPixelBufferGetPlaneCount(CVPixelBufferRef)`: zero for a packed
/// buffer.
pub unsafe extern "C" fn CVPixelBufferGetPlaneCount(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| match s.layout.is_planar() {
        true => s.layout.planes.len(),
        false => 0,
    })
}

pub unsafe extern "C" fn CVPixelBufferGetBaseAddressOfPlane(
    buffer: CFTypeRef,
    index: usize,
) -> *mut c_void {
    let Some(state) = state(buffer) else {
        return std::ptr::null_mut();
    };
    match state.plane(index) {
        // SAFETY: the plane lies inside the buffer's memory.
        Some(plane) => unsafe { state.base().add(plane.offset) }.cast(),
        None => std::ptr::null_mut(),
    }
}

pub unsafe extern "C" fn CVPixelBufferGetBytesPerRowOfPlane(
    buffer: CFTypeRef,
    index: usize,
) -> usize {
    state(buffer)
        .and_then(|s| s.plane(index))
        .map_or(0, |p| p.bytes_per_row)
}

pub unsafe extern "C" fn CVPixelBufferGetWidthOfPlane(buffer: CFTypeRef, index: usize) -> usize {
    state(buffer)
        .and_then(|s| s.plane(index))
        .map_or(0, |p| p.width)
}

pub unsafe extern "C" fn CVPixelBufferGetHeightOfPlane(buffer: CFTypeRef, index: usize) -> usize {
    state(buffer)
        .and_then(|s| s.plane(index))
        .map_or(0, |p| p.height)
}

/// `CGSize`, which SysV returns in `xmm0` and `xmm1`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

/// `CGSize CVImageBufferGetEncodedSize(CVImageBufferRef)`.
pub unsafe extern "C" fn CVImageBufferGetEncodedSize(buffer: CFTypeRef) -> Size {
    state(buffer).map_or(Size::default(), |s| Size {
        width: s.width as f64,
        height: s.height as f64,
    })
}

/// `CVReturn CVPixelBufferPoolCreatePixelBuffer(CFAllocatorRef,
/// CVPixelBufferPoolRef, CVPixelBufferRef *)`: there are no pools.
pub unsafe extern "C" fn CVPixelBufferPoolCreatePixelBuffer(
    _allocator: CFTypeRef,
    _pool: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    if !out.is_null() {
        // SAFETY: the caller's out-parameter.
        unsafe { out.write(std::ptr::null()) };
    }
    INVALID_ARGUMENT
}

exports::exports! {
    functions {
        "CVPixelBufferCreate" => CVPixelBufferCreate,
        "CVPixelBufferGetTypeID" => CVPixelBufferGetTypeID,
        "CVPixelBufferRetain" => CVPixelBufferRetain,
        "CVPixelBufferRelease" => CVPixelBufferRelease,
        "CVPixelBufferLockBaseAddress" => CVPixelBufferLockBaseAddress,
        "CVPixelBufferUnlockBaseAddress" => CVPixelBufferUnlockBaseAddress,
        "CVPixelBufferGetBaseAddress" => CVPixelBufferGetBaseAddress,
        "CVPixelBufferGetBytesPerRow" => CVPixelBufferGetBytesPerRow,
        "CVPixelBufferGetWidth" => CVPixelBufferGetWidth,
        "CVPixelBufferGetHeight" => CVPixelBufferGetHeight,
        "CVPixelBufferGetPixelFormatType" => CVPixelBufferGetPixelFormatType,
        "CVPixelBufferGetDataSize" => CVPixelBufferGetDataSize,
        "CVPixelBufferGetIOSurface" => CVPixelBufferGetIOSurface,
        "CVPixelBufferIsPlanar" => CVPixelBufferIsPlanar,
        "CVPixelBufferGetPlaneCount" => CVPixelBufferGetPlaneCount,
        "CVPixelBufferGetBaseAddressOfPlane" => CVPixelBufferGetBaseAddressOfPlane,
        "CVPixelBufferGetBytesPerRowOfPlane" => CVPixelBufferGetBytesPerRowOfPlane,
        "CVPixelBufferGetWidthOfPlane" => CVPixelBufferGetWidthOfPlane,
        "CVPixelBufferGetHeightOfPlane" => CVPixelBufferGetHeightOfPlane,
        "CVImageBufferGetEncodedSize" => CVImageBufferGetEncodedSize,
        "CVPixelBufferPoolCreatePixelBuffer" => CVPixelBufferPoolCreatePixelBuffer,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use corefoundation::callbacks::Policy;
    use corefoundation::collections::{Number, new_dictionary, new_number};

    fn key(text: &str) -> CFTypeRef {
        corefoundation::base::interned_name(text) as usize as CFTypeRef
    }

    /// Attributes with an empty IOSurface properties dictionary, as Roblox
    /// passes them.
    pub(crate) fn surface_attributes() -> CFTypeRef {
        corefoundation::bootstrap();
        let empty = new_dictionary(
            &[],
            Policy::from_key_callbacks(),
            Policy::from_type_callbacks(),
            false,
        );
        let attributes = new_dictionary(
            &[(key(IOSURFACE_PROPERTIES_KEY), empty)],
            Policy::from_key_callbacks(),
            Policy::from_type_callbacks(),
            false,
        );
        buffer::release(empty);
        attributes
    }

    pub(crate) fn make(
        format: u32,
        width: usize,
        height: usize,
        attributes: CFTypeRef,
    ) -> CFTypeRef {
        corefoundation::bootstrap();
        let mut out = std::ptr::null();
        // SAFETY: a live out-parameter and a dictionary or null.
        let status = unsafe {
            CVPixelBufferCreate(
                std::ptr::null(),
                width,
                height,
                format,
                attributes,
                &mut out,
            )
        };
        assert_eq!(status, SUCCESS);
        assert!(!out.is_null());
        out
    }

    #[test]
    fn nv12_odd_size_has_half_rounded_up_chroma() {
        let buffer = make(format::NV12_VIDEO, 63, 33, std::ptr::null());
        // SAFETY: a buffer this test made.
        unsafe {
            assert_eq!(CVPixelBufferGetPlaneCount(buffer), 2);
            assert_eq!(CVPixelBufferGetWidthOfPlane(buffer, 0), 63);
            assert_eq!(CVPixelBufferGetHeightOfPlane(buffer, 0), 33);
            assert_eq!(CVPixelBufferGetWidthOfPlane(buffer, 1), 32);
            assert_eq!(CVPixelBufferGetHeightOfPlane(buffer, 1), 17);
            assert!(CVPixelBufferGetBytesPerRowOfPlane(buffer, 1) >= 64);
            assert_eq!(CVPixelBufferGetWidth(buffer), 63);
            assert_eq!(CVPixelBufferGetHeight(buffer), 33);
            assert_eq!(CVPixelBufferGetPixelFormatType(buffer), format::NV12_VIDEO);
            assert_eq!(
                corefoundation::api::CFGetTypeID(buffer),
                CVPixelBufferGetTypeID()
            );
            CVPixelBufferRelease(buffer);
        }
    }

    #[test]
    fn y420_has_three_64_aligned_non_overlapping_planes_within_data_size() {
        let buffer = make(format::Y420, 100, 51, std::ptr::null());
        // SAFETY: a buffer this test made.
        unsafe {
            assert_eq!(CVPixelBufferIsPlanar(buffer), 1);
            assert_eq!(CVPixelBufferGetPlaneCount(buffer), 3);
            let base = CVPixelBufferGetBaseAddress(buffer) as usize;
            let size = CVPixelBufferGetDataSize(buffer);
            let mut previous_end = base;
            for plane in 0..3 {
                let start = CVPixelBufferGetBaseAddressOfPlane(buffer, plane) as usize;
                let row = CVPixelBufferGetBytesPerRowOfPlane(buffer, plane);
                let height = CVPixelBufferGetHeightOfPlane(buffer, plane);
                assert_eq!(start % 64, 0, "plane {plane} starts aligned");
                assert_eq!(row % 64, 0, "plane {plane} rows are aligned");
                assert!(row >= CVPixelBufferGetWidthOfPlane(buffer, plane));
                assert!(start >= previous_end, "plane {plane} overlaps");
                previous_end = start + row * height;
            }
            assert!(previous_end <= base + size);
            assert_eq!(CVPixelBufferGetWidthOfPlane(buffer, 2), 50);
            assert_eq!(CVPixelBufferGetHeightOfPlane(buffer, 2), 26);
            assert!(CVPixelBufferGetBaseAddressOfPlane(buffer, 3).is_null());
            assert_eq!(CVPixelBufferGetWidthOfPlane(buffer, 3), 0);
            assert_eq!(
                CVPixelBufferGetBytesPerRow(buffer),
                CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
            );
            CVPixelBufferRelease(buffer);
        }
    }

    #[test]
    fn planar_base_address_is_big_endian_plane_info() {
        let buffer = make(format::Y420, 64, 64, std::ptr::null());
        // SAFETY: a buffer this test made; the header is three eight-byte
        // entries at its base.
        unsafe {
            let base = CVPixelBufferGetBaseAddress(buffer) as *const u8;
            for plane in 0..3 {
                let entry = std::slice::from_raw_parts(base.add(plane * 8), 8);
                let offset = i32::from_be_bytes(entry[0..4].try_into().unwrap());
                let row = u32::from_be_bytes(entry[4..8].try_into().unwrap());
                assert_eq!(
                    base.add(offset as usize) as *mut c_void,
                    CVPixelBufferGetBaseAddressOfPlane(buffer, plane)
                );
                assert_eq!(
                    row as usize,
                    CVPixelBufferGetBytesPerRowOfPlane(buffer, plane)
                );
            }
            CVPixelBufferRelease(buffer);
        }
        let nv12 = make(format::NV12_FULL, 64, 64, std::ptr::null());
        // SAFETY: as above, with two entries.
        unsafe {
            let base = CVPixelBufferGetBaseAddress(nv12) as *const u8;
            let entry = std::slice::from_raw_parts(base.add(8), 8);
            let offset = i32::from_be_bytes(entry[0..4].try_into().unwrap());
            assert_eq!(
                base.add(offset as usize) as *mut c_void,
                CVPixelBufferGetBaseAddressOfPlane(nv12, 1)
            );
            CVPixelBufferRelease(nv12);
        }
    }

    #[test]
    fn iosurface_backed_only_when_requested() {
        let heap = make(format::Y420, 64, 64, std::ptr::null());
        let attributes = surface_attributes();
        let backed = make(format::Y420, 64, 64, attributes);
        let packed = make(format::BGRA, 16, 8, attributes);
        // SAFETY: buffers this test made, and their surfaces.
        unsafe {
            assert!(CVPixelBufferGetIOSurface(heap).is_null());
            let surface = CVPixelBufferGetIOSurface(backed);
            assert!(iosurface::is_surface(surface));
            assert_eq!(iosurface::planes::IOSurfaceGetPlaneCount(surface), 3);
            for plane in 0..3 {
                assert_eq!(
                    CVPixelBufferGetBaseAddressOfPlane(backed, plane),
                    iosurface::planes::IOSurfaceGetBaseAddressOfPlane(surface, plane)
                );
                assert_eq!(
                    CVPixelBufferGetBytesPerRowOfPlane(backed, plane),
                    iosurface::planes::IOSurfaceGetBytesPerRowOfPlane(surface, plane)
                );
            }
            assert_eq!(
                CVPixelBufferGetBaseAddress(backed),
                iosurface::IOSurfaceGetBaseAddress(surface)
            );
            assert!(iosurface::pixels(surface).is_none());
            let packed_surface = CVPixelBufferGetIOSurface(packed);
            assert_eq!(
                iosurface::IOSurfaceGetBytesPerRow(packed_surface),
                CVPixelBufferGetBytesPerRow(packed)
            );
            assert_eq!(
                iosurface::IOSurfaceGetPixelFormat(packed_surface),
                format::BGRA
            );
            assert!(iosurface::pixels(packed_surface).is_some());
            // The buffer holds the surface.
            let count = corefoundation::api::CFGetRetainCount(surface);
            assert!(count >= 1);
            CVPixelBufferRelease(heap);
            CVPixelBufferRelease(backed);
            CVPixelBufferRelease(packed);
            buffer::release(attributes);
        }
    }

    #[test]
    fn unsupported_format_is_refused() {
        corefoundation::bootstrap();
        let mut out = 1 as CFTypeRef;
        // SAFETY: a live out-parameter.
        unsafe {
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    8,
                    8,
                    u32::from_be_bytes(*b"x420"),
                    std::ptr::null(),
                    &mut out
                ),
                INVALID_PIXEL_FORMAT
            );
            assert!(out.is_null());
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    0,
                    8,
                    format::BGRA,
                    std::ptr::null(),
                    &mut out
                ),
                INVALID_SIZE
            );
            assert_eq!(
                CVPixelBufferCreate(
                    std::ptr::null(),
                    8,
                    8,
                    format::BGRA,
                    std::ptr::null(),
                    std::ptr::null_mut()
                ),
                INVALID_ARGUMENT
            );
        }
    }

    #[test]
    fn row_alignment_attribute_raises_the_row_size() {
        let attributes = new_dictionary(
            &[(
                key(BYTES_PER_ROW_ALIGNMENT_KEY),
                new_number(Number::Integer(256)),
            )],
            Policy::from_key_callbacks(),
            Policy::from_type_callbacks(),
            false,
        );
        let buffer = make(format::BGRA, 10, 2, attributes);
        // SAFETY: a buffer this test made.
        unsafe {
            assert_eq!(CVPixelBufferGetBytesPerRow(buffer), 256);
            CVPixelBufferRelease(buffer);
        }
        buffer::release(attributes);
    }

    #[test]
    fn lock_unlock_balance_and_read_only_flag() {
        let attributes = surface_attributes();
        let buffer = make(format::NV12_VIDEO, 8, 8, attributes);
        // SAFETY: a buffer this test made, and its surface.
        unsafe {
            let surface = CVPixelBufferGetIOSurface(buffer);
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer, 0), ERROR);
            assert_eq!(
                CVPixelBufferLockBaseAddress(buffer, LOCK_READ_ONLY),
                SUCCESS
            );
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(buffer, LOCK_READ_ONLY),
                SUCCESS
            );
            assert_eq!(
                iosurface::IOSurfaceGetSeed(surface),
                0,
                "a read leaves the seed"
            );
            assert_eq!(CVPixelBufferLockBaseAddress(buffer, 0), SUCCESS);
            assert_eq!(CVPixelBufferLockBaseAddress(buffer, 0), SUCCESS);
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer, 0), SUCCESS);
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer, 0), SUCCESS);
            assert_eq!(iosurface::IOSurfaceGetSeed(surface), 2, "two writes");
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer, 0), ERROR);
            assert_eq!(
                CVPixelBufferLockBaseAddress(std::ptr::null(), 0),
                INVALID_ARGUMENT
            );
            CVPixelBufferRelease(buffer);
            buffer::release(attributes);
        }
    }

    #[test]
    fn cv_buffer_release_of_null_is_harmless() {
        // SAFETY: null, which both accept.
        unsafe {
            buffer::CVBufferRelease(std::ptr::null());
            CVPixelBufferRelease(std::ptr::null());
            assert!(buffer::CVBufferRetain(std::ptr::null()).is_null());
            assert!(CVPixelBufferRetain(std::ptr::null()).is_null());
        }
        let buffer = make(format::BGRA, 2, 2, std::ptr::null());
        // SAFETY: a buffer this test made.
        unsafe {
            let count = corefoundation::api::CFGetRetainCount(buffer);
            assert_eq!(buffer::CVBufferRetain(buffer), buffer);
            assert_eq!(corefoundation::api::CFGetRetainCount(buffer), count + 1);
            buffer::CVBufferRelease(buffer);
            CVPixelBufferRelease(buffer);
        }
    }

    #[test]
    fn encoded_size_is_returned_in_two_sse_registers() {
        let buffer = make(format::Y420, 640, 360, std::ptr::null());
        let get: unsafe extern "C" fn(CFTypeRef) -> Size =
            std::hint::black_box(CVImageBufferGetEncodedSize);
        // SAFETY: a buffer this test made, through the function's own type.
        let size = unsafe { get(buffer) };
        assert_eq!(
            size,
            Size {
                width: 640.0,
                height: 360.0
            }
        );
        // The same call as a guest's `CGSize (*)(CVImageBufferRef)` sees it:
        // the width in xmm0 and the height in xmm1.
        #[cfg(target_arch = "x86_64")]
        {
            let (width, height): (f64, f64);
            // SAFETY: a call to a SysV function of one pointer argument that
            // returns two doubles; every caller-saved register is clobbered.
            unsafe {
                std::arch::asm!(
                    "call {f}",
                    f = in(reg) get,
                    in("rdi") buffer,
                    out("xmm0") width,
                    out("xmm1") height,
                    clobber_abi("C"),
                );
            }
            assert_eq!((width, height), (640.0, 360.0));
        }
        // SAFETY: as above.
        unsafe {
            assert_eq!(
                CVImageBufferGetEncodedSize(std::ptr::null()),
                Size::default()
            );
            CVPixelBufferRelease(buffer);
        }
    }

    #[test]
    fn planes_mut_covers_each_plane() {
        let buffer = make(format::NV12_VIDEO, 4, 4, std::ptr::null());
        let planes = planes_mut(buffer).unwrap();
        assert_eq!(planes.len(), 2);
        assert_eq!(planes[0].0.len(), planes[0].1 * 4);
        assert_eq!(planes[1].0.len(), planes[1].1 * 2);
        planes.into_iter().for_each(|(bytes, _)| bytes.fill(7));
        // SAFETY: a buffer this test made.
        unsafe {
            let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1) as *const u8;
            assert_eq!(*chroma, 7);
            CVPixelBufferRelease(buffer);
        }
        assert!(planes_mut(std::ptr::null()).is_none());
    }

    #[test]
    fn a_pool_has_no_buffers() {
        let mut out = 1 as CFTypeRef;
        // SAFETY: a live out-parameter.
        let status = unsafe {
            CVPixelBufferPoolCreatePixelBuffer(std::ptr::null(), std::ptr::null(), &mut out)
        };
        assert_eq!(status, INVALID_ARGUMENT);
        assert!(out.is_null());
    }
}
