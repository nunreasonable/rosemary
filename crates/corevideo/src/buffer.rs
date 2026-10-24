//! `CVBuffer`: what every CoreVideo buffer shares, which is its retain count
//! and its attachments.
//!
//! Every buffer class here descends from an abstract `CVBuffer` class, and
//! every buffer's body starts with the attachments, so `CVBufferRetain` and
//! the attachment calls work on a pixel buffer and a Metal texture alike.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use corefoundation::base::CFTypeRef;
use corefoundation::callbacks::Policy;
use objc::class::Class;
use objc::object::Id;

/// `kCVAttachmentMode_ShouldNotPropagate` and `_ShouldPropagate`.
pub const SHOULD_NOT_PROPAGATE: u32 = 0;
pub const SHOULD_PROPAGATE: u32 = 1;

/// A buffer's attachments by mode, as retained `(key, value)` pairs.
#[derive(Default)]
pub(crate) struct Attachments(Mutex<[Vec<(usize, usize)>; 2]>);

impl Drop for Attachments {
    fn drop(&mut self) {
        let modes = self.0.get_mut().unwrap_or_else(|e| e.into_inner());
        for (key, value) in modes.iter_mut().flat_map(|pairs| pairs.drain(..)) {
            release(key as CFTypeRef);
            release(value as CFTypeRef);
        }
    }
}

/// A CoreVideo buffer's body: the attachments first, at the same offset for
/// every buffer class, and then the class's own state.
#[repr(C)]
pub(crate) struct Body<T> {
    isa: *const c_void,
    attachments: Attachments,
    pub(crate) state: T,
}

pub(crate) fn release(object: CFTypeRef) {
    if !object.is_null() {
        // SAFETY: an object this crate holds a reference to.
        unsafe { corefoundation::api::CFRelease(object) };
    }
}

pub(crate) fn retain(object: CFTypeRef) -> CFTypeRef {
    match object.is_null() {
        true => object,
        // SAFETY: a live CF object.
        false => unsafe { corefoundation::api::CFRetain(object) },
    }
}

fn base_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class = corefoundation::base::register(c"CVBuffer", None, size_of::<Body<()>>(), false);
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

/// Register a buffer class whose state is `T`, with a `-dealloc` that drops
/// the attachments and the state.
pub(crate) fn register_subclass<T>(name: &'static std::ffi::CStr) -> &'static Class {
    unsafe extern "C" fn dealloc<T>(receiver: Id, _sel: objc::sel::Sel) {
        // SAFETY: `-dealloc` of an instance of a class registered with `T`,
        // whose body was written once by `make` and is dropped only here.
        unsafe {
            let body = receiver as *mut Body<T>;
            std::ptr::drop_in_place(&mut (*body).attachments);
            std::ptr::drop_in_place(&mut (*body).state);
        }
        objc::arc::finalize(receiver);
    }
    let class =
        corefoundation::base::register(name, Some(base_class()), size_of::<Body<T>>(), true);
    class.replace_method(
        objc::sel::get("dealloc"),
        dealloc::<T> as *const c_void,
        c"v16@0:8",
    );
    class
}

/// A new instance of `class`, which [`register_subclass`] made for `T`.
pub(crate) fn make<T>(class: &'static Class, state: T) -> CFTypeRef {
    // SAFETY: `Body<T>` is the layout `class` was registered with.
    let body = unsafe { corefoundation::base::allocate::<Body<T>>(class) };
    // SAFETY: a fresh, zeroed allocation whose fields are written once here.
    unsafe {
        std::ptr::write(&mut (*body).attachments, Attachments::default());
        std::ptr::write(&mut (*body).state, state);
    }
    body as CFTypeRef
}

/// The state of `object` if it is an instance of `class`, which
/// [`register_subclass`] made for `T`.
pub(crate) fn state<'a, T>(object: CFTypeRef, class: &'static Class) -> Option<&'a T> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_kind = !object.is_null() && unsafe { corefoundation::base::is_kind_of(object, class) };
    // SAFETY: checked to be an instance of `class`, whose body is `Body<T>`.
    is_kind.then(|| unsafe { &(*(object as *const Body<T>)).state })
}

fn attachments<'a>(object: CFTypeRef) -> Option<&'a Attachments> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_buffer =
        !object.is_null() && unsafe { corefoundation::base::is_kind_of(object, base_class()) };
    // SAFETY: every buffer class's body starts with `isa` and the attachments,
    // whatever its state, because `Body` is `repr(C)`.
    is_buffer.then(|| unsafe { &(*(object as *const Body<()>)).attachments })
}

/// Whether `object` is any CoreVideo buffer.
pub fn is_buffer(object: CFTypeRef) -> bool {
    attachments(object).is_some()
}

fn equal(a: usize, b: CFTypeRef) -> bool {
    // SAFETY: two live CF objects.
    unsafe { corefoundation::api::CFEqual(a as CFTypeRef, b) != 0 }
}

/// Set `key` to `value` in `mode`, taking it out of the other mode.
pub fn set_attachment(buffer: CFTypeRef, key: CFTypeRef, value: CFTypeRef, mode: u32) {
    let Some(attachments) = attachments(buffer) else {
        return;
    };
    if key.is_null() || value.is_null() || mode > SHOULD_PROPAGATE {
        return;
    }
    let pair = (retain(key) as usize, retain(value) as usize);
    let mut removed = Vec::new();
    {
        let mut modes = attachments.0.lock().unwrap_or_else(|e| e.into_inner());
        for pairs in modes.iter_mut() {
            if let Some(at) = pairs.iter().position(|(k, _)| equal(*k, key)) {
                removed.push(pairs.remove(at));
            }
        }
        modes[mode as usize].push(pair);
    }
    for (key, value) in removed {
        release(key as CFTypeRef);
        release(value as CFTypeRef);
    }
}

/// A new immutable dictionary of `buffer`'s attachments in `mode`, or null
/// when there are none.
pub fn copy_attachments(buffer: CFTypeRef, mode: u32) -> CFTypeRef {
    let Some(attachments) = attachments(buffer) else {
        return std::ptr::null();
    };
    if mode > SHOULD_PROPAGATE {
        return std::ptr::null();
    }
    let modes = attachments.0.lock().unwrap_or_else(|e| e.into_inner());
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = modes[mode as usize]
        .iter()
        .map(|(k, v)| (*k as CFTypeRef, *v as CFTypeRef))
        .collect();
    match pairs.is_empty() {
        true => std::ptr::null(),
        false => corefoundation::collections::new_dictionary(
            &pairs,
            Policy::from_key_callbacks(),
            Policy::from_type_callbacks(),
            false,
        ),
    }
}

/// `CVBufferRef CVBufferRetain(CVBufferRef)`: null-safe.
pub unsafe extern "C" fn CVBufferRetain(buffer: CFTypeRef) -> CFTypeRef {
    retain(buffer)
}

/// `void CVBufferRelease(CVBufferRef)`: null-safe.
pub unsafe extern "C" fn CVBufferRelease(buffer: CFTypeRef) {
    release(buffer)
}

exports::exports! {
    functions {
        "CVBufferRetain" => CVBufferRetain,
        "CVBufferRelease" => CVBufferRelease,
    }
}
