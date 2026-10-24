//! `NSSortDescriptor`: a key path, a direction, and the selector two values
//! for that key are compared with - `-compareObject:toObject:` is the one
//! thing this class is for, and it is real: both objects are sent
//! `-valueForKey:` and the two results are sent the comparison selector,
//! exactly as Apple's own does.

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

struct Body {
    /// Never read: it exists so the first word is a real `isa`, which is what
    /// makes this a valid Objective-C object at all.
    #[allow(dead_code)]
    isa: *const Class,
    contents: *mut Contents,
}

struct Contents {
    /// Retained.
    key: Id,
    ascending: bool,
    /// `None` means the default, `-compare:`.
    selector: Option<Sel>,
}

impl Drop for Contents {
    fn drop(&mut self) {
        objc::arc::release(self.key);
    }
}

fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        let class = corefoundation::base::register(
            c"NSSortDescriptor",
            None,
            std::mem::size_of::<Body>(),
            false,
        );
        unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
            // SAFETY: reached with the last reference gone.
            unsafe {
                let body = receiver as *mut Body;
                if !(*body).contents.is_null() {
                    drop(Box::from_raw((*body).contents));
                    (*body).contents = std::ptr::null_mut();
                }
                objc::arc::finalize(receiver);
            }
        }
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const std::ffi::c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(p as *const Class).expect("a sort descriptor class") }
}

/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
unsafe fn contents<'a>(receiver: Id) -> Option<&'a mut Contents> {
    if !unsafe { corefoundation::base::is_kind_of(receiver as CFTypeRef, class()) } {
        return None;
    }
    // SAFETY: an instance of `class()` is a `Body`.
    unsafe {
        let body = receiver as *mut Body;
        (!(*body).contents.is_null()).then(|| &mut *(*body).contents)
    }
}

fn make(key: Id, ascending: bool, selector: Option<Sel>) -> Id {
    // SAFETY: `class()`'s instances are `Body`.
    let body: *mut Body = unsafe { corefoundation::base::allocate(class()) };
    if body.is_null() {
        return body as Id;
    }
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe {
        (*body).contents = Box::into_raw(Box::new(Contents {
            key: objc::arc::retain(key),
            ascending,
            selector,
        }));
    }
    body as Id
}

unsafe extern "C" fn init_with_key_ascending(
    _class: Id,
    _sel: Sel,
    key: Id,
    ascending: bool,
) -> Id {
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(make(key, ascending, None) as CFTypeRef) as Id }
}

unsafe extern "C" fn init_with_key_ascending_selector(
    _class: Id,
    _sel: Sel,
    key: Id,
    ascending: bool,
    selector: Sel,
) -> Id {
    // SAFETY: a fresh object.
    unsafe {
        corefoundation::api::CFAutorelease(make(key, ascending, Some(selector)) as CFTypeRef) as Id
    }
}

unsafe extern "C" fn sort_descriptor_with_key(
    class_object: Id,
    sel: Sel,
    key: Id,
    ascending: bool,
) -> Id {
    unsafe { init_with_key_ascending(class_object, sel, key, ascending) }
}

unsafe extern "C" fn get_key(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `NSSortDescriptor`.
    unsafe { contents(receiver) }.map_or(std::ptr::null_mut(), |c| c.key)
}

unsafe extern "C" fn get_ascending(receiver: Id, _sel: Sel) -> bool {
    // SAFETY: an `NSSortDescriptor`.
    unsafe { contents(receiver) }.is_some_and(|c| c.ascending)
}

/// `-compare:` on the objects' values, the default when no selector was given.
unsafe fn default_compare(a: Id, b: Id) -> i64 {
    unsafe { send2_i64(a, objc::sel::get("compare:"), b) }
}

/// Send `sel` to `receiver` with `argument`, both objects. `0` (nil) for a
/// receiver with no such method - a key with no value compares equal to one.
unsafe fn send1_id(receiver: Id, sel: Sel, argument: Id) -> Id {
    let Some(method) = (unsafe { objc::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return std::ptr::null_mut();
    };
    let f: unsafe extern "C" fn(Id, Sel, Id) -> Id =
        unsafe { std::mem::transmute(method.implementation()) };
    unsafe { f(receiver, sel, argument) }
}

/// As [`send1_id`], for a method returning `NSComparisonResult`.
unsafe fn send2_i64(receiver: Id, sel: Sel, argument: Id) -> i64 {
    let Some(method) = (unsafe { objc::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return 0;
    };
    let f: unsafe extern "C" fn(Id, Sel, Id) -> i64 =
        unsafe { std::mem::transmute(method.implementation()) };
    unsafe { f(receiver, sel, argument) }
}

unsafe extern "C" fn compare_object_to_object(receiver: Id, _sel: Sel, a: Id, b: Id) -> i64 {
    // SAFETY: an `NSSortDescriptor`.
    let Some(descriptor) = (unsafe { contents(receiver) }) else {
        return 0;
    };
    let value_for_key = objc::sel::get("valueForKey:");
    // SAFETY: `a` and `b` are the caller's objects.
    let (va, vb) = unsafe {
        (
            send1_id(a, value_for_key, descriptor.key),
            send1_id(b, value_for_key, descriptor.key),
        )
    };
    if va.is_null() || vb.is_null() {
        return i64::from(!va.is_null()) - i64::from(!vb.is_null());
    }
    let order = match descriptor.selector {
        // SAFETY: `va`/`vb` are live objects; the selector is the caller's own
        // and is expected to answer `NSComparisonResult`.
        Some(sel) => unsafe { send2_i64(va, sel, vb) },
        None => unsafe { default_compare(va, vb) },
    };
    if descriptor.ascending { order } else { -order }
}

unsafe extern "C" fn reversed_sort_descriptor(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `NSSortDescriptor`.
    let Some(descriptor) = (unsafe { contents(receiver) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a fresh object.
    unsafe {
        corefoundation::api::CFAutorelease(make(
            descriptor.key,
            !descriptor.ascending,
            descriptor.selector,
        ) as CFTypeRef) as Id
    }
}

pub(crate) fn install() {
    let class = class();
    class_methods!(class, {
        "sortDescriptorWithKey:ascending:" => sort_descriptor_with_key, c"@32@0:8@16B24";
    });
    methods!(class, {
        "initWithKey:ascending:" => init_with_key_ascending, c"@32@0:8@16B24";
        "initWithKey:ascending:selector:" => init_with_key_ascending_selector,
            c"@40@0:8@16B24:32";
        "key" => get_key, c"@16@0:8";
        "ascending" => get_ascending, c"B16@0:8";
        "compareObject:toObject:" => compare_object_to_object, c"q32@0:8@16@24";
        "reversedSortDescriptor" => reversed_sort_descriptor, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(key: &str, ascending: bool) -> Id {
        let key = crate::string::from_str(key);
        // SAFETY: a fresh key string.
        let d = unsafe {
            init_with_key_ascending(std::ptr::null_mut(), objc::sel::get("x"), key, ascending)
        };
        objc::arc::release(key);
        d
    }

    #[test]
    fn compares_by_the_keys_value_and_honors_the_direction() {
        crate::bootstrap();
        let a = crate::number(1);
        let b = crate::number(2);
        // `-valueForKey:"self"` is answered by every object's default
        // `-valueForKey:`, so a plain `NSNumber` works as its own key here.
        let ascending = descriptor("self", true);
        let descending = descriptor("self", false);
        // SAFETY: live objects.
        unsafe {
            assert_eq!(
                compare_object_to_object(ascending, objc::sel::get("x"), a, b),
                -1
            );
            assert_eq!(
                compare_object_to_object(descending, objc::sel::get("x"), a, b),
                1
            );
        }
    }

    #[test]
    fn reversed_sort_descriptor_flips_ascending_and_keeps_the_key() {
        crate::bootstrap();
        let d = descriptor("self", true);
        // SAFETY: a live descriptor.
        let reversed = unsafe { reversed_sort_descriptor(d, objc::sel::get("x")) };
        unsafe {
            assert!(!get_ascending(reversed, objc::sel::get("x")));
            assert!(
                corefoundation::api::CFEqual(
                    get_key(d, objc::sel::get("x")) as CFTypeRef,
                    get_key(reversed, objc::sel::get("x")) as CFTypeRef
                ) != 0
            );
        }
    }
}
