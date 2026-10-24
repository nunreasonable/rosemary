//! `CMCopyDictionaryOfAttachments`: the attachments of whatever CoreMedia or
//! CoreVideo object carries them.
//!
//! A sample buffer's own attachments would come from `CMSetAttachment`, which
//! is not implemented, so it has none. A CoreVideo buffer's are CoreVideo's.

use corefoundation::base::CFTypeRef;

/// `CFDictionaryRef CMCopyDictionaryOfAttachments(CFAllocatorRef,
/// CMAttachmentBearerRef target, CMAttachmentMode)`: retained, or null when
/// there are none.
pub unsafe extern "C" fn CMCopyDictionaryOfAttachments(
    _allocator: CFTypeRef,
    target: CFTypeRef,
    mode: u32,
) -> CFTypeRef {
    if crate::sample::is_sample_buffer(target) {
        return std::ptr::null();
    }
    corevideo::buffer::copy_attachments(target, mode)
}

exports::exports! {
    functions {
        "CMCopyDictionaryOfAttachments" => CMCopyDictionaryOfAttachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use corevideo::buffer::{SHOULD_NOT_PROPAGATE, SHOULD_PROPAGATE, set_attachment};

    fn key(text: &str) -> CFTypeRef {
        corefoundation::base::interned_name(text) as usize as CFTypeRef
    }

    /// A name answered twice is one a later row shadows without a word.
    #[test]
    fn no_name_is_exported_twice() {
        let mut names = crate::names();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
        let symbols = crate::data_symbols();
        let mut data: Vec<&str> = symbols.iter().map(|(n, _)| n.as_str()).collect();
        data.sort_unstable();
        data.dedup();
        assert_eq!(data.len(), symbols.len());
    }

    #[test]
    fn sample_buffer_without_attachments_copies_null() {
        let mut sample = std::ptr::null();
        // SAFETY: a live out-parameter, and the sample buffer it receives.
        unsafe {
            crate::sample::CMSampleBufferCreate(
                std::ptr::null(),
                std::ptr::null(),
                1,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut sample,
            );
            assert!(
                CMCopyDictionaryOfAttachments(std::ptr::null(), sample, SHOULD_PROPAGATE).is_null()
            );
            assert!(CMCopyDictionaryOfAttachments(std::ptr::null(), std::ptr::null(), 1).is_null());
            corefoundation::api::CFRelease(sample);
        }
    }

    #[test]
    fn pixel_buffer_propagated_attachments_are_copied_by_mode() {
        let buffer = corevideo::pixelbuffer::create(
            corevideo::format::Y420,
            16,
            16,
            corevideo::pixelbuffer::Options::default(),
        )
        .unwrap();
        let matrix = key("CVImageBufferYCbCrMatrix");
        let primaries = key("CVImageBufferColorPrimaries");
        let note = key("Note");
        set_attachment(buffer, matrix, key("ITU_R_601_4"), SHOULD_PROPAGATE);
        set_attachment(buffer, matrix, key("ITU_R_709_2"), SHOULD_PROPAGATE);
        set_attachment(buffer, primaries, key("ITU_R_709_2"), SHOULD_PROPAGATE);
        set_attachment(buffer, note, key("kept"), SHOULD_NOT_PROPAGATE);
        // SAFETY: a buffer this test made, and dictionaries the call returns.
        unsafe {
            let propagated =
                CMCopyDictionaryOfAttachments(std::ptr::null(), buffer, SHOULD_PROPAGATE);
            assert_eq!(corefoundation::api::CFDictionaryGetCount(propagated), 2);
            let value = corefoundation::api::CFDictionaryGetValue(propagated, matrix);
            assert_ne!(
                corefoundation::api::CFEqual(value, key("ITU_R_709_2")),
                0,
                "the later value replaces the earlier"
            );
            assert!(corefoundation::api::CFDictionaryGetValue(propagated, note).is_null());
            corefoundation::api::CFRelease(propagated);
            let kept =
                CMCopyDictionaryOfAttachments(std::ptr::null(), buffer, SHOULD_NOT_PROPAGATE);
            assert_eq!(corefoundation::api::CFDictionaryGetCount(kept), 1);
            corefoundation::api::CFRelease(kept);
            // Setting a key in the other mode moves it.
            set_attachment(buffer, note, key("moved"), SHOULD_PROPAGATE);
            assert!(
                CMCopyDictionaryOfAttachments(std::ptr::null(), buffer, SHOULD_NOT_PROPAGATE)
                    .is_null()
            );
            corefoundation::api::CFRelease(buffer);
        }
    }
}
