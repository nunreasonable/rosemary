//! A minimal `CFAttributedString`: a string and one attributes dictionary
//! over its whole length - all `CFAttributedStringCreate` itself is ever asked
//! to build. Apple's own type additionally supports per-range attribute runs
//! through `CFAttributedStringSetAttributes` and friends; none of those are
//! implemented here, so this is only ever the one uniform run
//! `CFAttributedStringCreate`'s own signature describes.
//!
//! An opaque handle with no Objective-C `isa`, as `crate::runloop`'s sources
//! and `crate::stream`'s pipes are, and for the same reason: this crate's own
//! `NSAttributedString` lives one crate up, in `foundation`, which this one
//! cannot depend on without a cycle - so there is no single class the two
//! could share, and registering a second one under the same name would
//! collide with the first.

use std::sync::Arc;

use crate::base::CFTypeRef;

/// The uniform run: one string, one dictionary of attributes over all of it.
/// Both retained; `attributes` may be null, which `CFAttributedStringCreate`
/// documents as "no attributes" rather than refusing.
pub struct AttributedString {
    pub string: CFTypeRef,
    pub attributes: CFTypeRef,
}

impl Drop for AttributedString {
    fn drop(&mut self) {
        // SAFETY: both retained by `CFAttributedStringCreate`, or null.
        unsafe {
            crate::api::CFRelease(self.string);
            crate::api::CFRelease(self.attributes);
        }
    }
}

/// `CFAttributedStringRef CFAttributedStringCreate(CFAllocatorRef,
/// CFStringRef str, CFDictionaryRef attributes)`.
///
/// # Safety
///
/// `string` is null or a `CFString`; `attributes` is null or a `CFDictionary`.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CFAttributedStringCreate(
    _allocator: CFTypeRef,
    string: CFTypeRef,
    attributes: CFTypeRef,
) -> CFTypeRef {
    let value = Arc::new(AttributedString {
        // SAFETY: the caller's obligation.
        string: unsafe { crate::api::CFRetain(string) },
        attributes: unsafe { crate::api::CFRetain(attributes) },
    });
    crate::runloop::note(Arc::as_ptr(&value));
    Arc::into_raw(value) as CFTypeRef
}

exports::exports! {
    functions {
        "CFAttributedStringCreate" => CFAttributedStringCreate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_retains_both_the_string_and_the_attributes() {
        crate::bootstrap();
        let string = crate::string::from_bytes(b"hi", false);
        let attributes = crate::collections::new_dictionary(
            &[],
            crate::callbacks::Policy::from_key_callbacks(),
            crate::callbacks::Policy::from_type_callbacks(),
            false,
        );
        let before_string = unsafe { crate::api::CFGetRetainCount(string) };
        let before_attrs = unsafe { crate::api::CFGetRetainCount(attributes) };
        // SAFETY: two live objects.
        let value = unsafe { CFAttributedStringCreate(std::ptr::null(), string, attributes) };
        assert!(!value.is_null());
        // SAFETY: the create rule - this retained both.
        unsafe {
            assert_eq!(crate::api::CFGetRetainCount(string), before_string + 1);
            assert_eq!(crate::api::CFGetRetainCount(attributes), before_attrs + 1);
            crate::api::CFRelease(string);
            crate::api::CFRelease(attributes);
        }
    }
}
