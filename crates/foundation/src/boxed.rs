use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use corefoundation::base::CFTypeRef;

#[repr(C)]
pub struct Body<T> {
    isa: *const Class,
    pub(crate) contents: *mut T,
}

pub fn register<T: 'static>(
    name: &'static std::ffi::CStr,
    superclass: Option<&'static Class>,
) -> &'static Class {
    let class = corefoundation::base::register(name, superclass, size_of::<Body<T>>(), false);
    unsafe extern "C" fn dealloc<T>(receiver: Id, _sel: Sel) {
        // SAFETY: `-dealloc` is reached with the last reference gone, and the
        // receiver is a `Body<T>` whose `contents` came from `Box::into_raw`.
        unsafe {
            let body = receiver as *mut Body<T>;
            if !(*body).contents.is_null() {
                drop(Box::from_raw((*body).contents));
                (*body).contents = std::ptr::null_mut();
            }
        }
        objc::arc::finalize(receiver);
    }
    class.replace_method(
        objc::sel::get("dealloc"),
        dealloc::<T> as *const std::ffi::c_void,
        c"v16@0:8",
    );
    class
}

pub fn make<T>(class: &'static Class, contents: T) -> Id {
    // SAFETY: `Body<T>` begins with `isa` and is the layout `class` was
    // registered with.
    let body: *mut Body<T> = unsafe { corefoundation::base::allocate(class) };
    if body.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe { (*body).contents = Box::into_raw(Box::new(contents)) };
    body as Id
}

/// Give an already-allocated object its contents, for a class whose `-init` is
/// where the state is made. Does nothing to an object that has some already, so
/// a subclass calling `[super init]` twice does not leak the first box.
///
/// # Safety
///
/// `object` must be null or an instance of a class [`register`]ed for `T`.
pub unsafe fn fill<T>(object: Id, contents: T) {
    if object.is_null() {
        return;
    }
    unsafe {
        let body = object as *mut Body<T>;
        if (*body).contents.is_null() {
            (*body).contents = Box::into_raw(Box::new(contents));
        }
    }
}

/// # Safety
///
/// `object` must be null or an object this runtime allocated.
pub unsafe fn contents<'a, T>(object: Id, class: &'static Class) -> Option<&'a mut T> {
    if !unsafe { corefoundation::base::is_kind_of(object as CFTypeRef, class) } {
        return None;
    }
    // SAFETY: an instance of `class` is a `Body<T>` with a live `contents`.
    unsafe {
        let body = object as *mut Body<T>;
        (!(*body).contents.is_null()).then(|| &mut *(*body).contents)
    }
}
