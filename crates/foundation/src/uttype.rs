//! UniformTypeIdentifiers: the `UTType` class, wrapping a UTI identifier.
//!
//! No Launch Services database of installed declarations exists here, so a
//! type this tree does not recognise answers with a dynamic identifier (see
//! `corefoundation::uti`) rather than failing outright - the same rule the
//! legacy `UTTypeCreatePreferredIdentifierForTag` follows.

#![allow(non_snake_case)]

use std::ffi::c_void;

use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use corefoundation::base::CFTypeRef;
use corefoundation::uti::{FILENAME_EXTENSION, MIME_TYPE, conforms, identifier_for, tag_for};

use crate::define::{class_methods, methods};

/// A `UTType` instance: the `isa`, and the identifier hanging off it.
#[repr(C)]
struct Body {
    isa: *const Class,
    contents: *mut String,
}

/// The one `UTType` class, registered on first use.
fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        let class = corefoundation::base::register(c"UTType", None, size_of::<Body>(), false);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: the cell holds a pointer to a class this function registered.
    unsafe { objc::class::from_ptr(ptr as *const Class).expect("a class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: `-dealloc` is reached with the last reference gone, and the
    // receiver is a `Body` whose `contents` came from `Box::into_raw`.
    unsafe {
        let body = receiver as *mut Body;
        if !(*body).contents.is_null() {
            drop(Box::from_raw((*body).contents));
            (*body).contents = std::ptr::null_mut();
        }
        objc::arc::finalize(receiver);
    }
}

/// A `UTType` for `identifier`, owned by the caller.
fn make(identifier: &str) -> Id {
    // SAFETY: `Body` begins with `isa` and is the layout `class()` was
    // registered with.
    let body: *mut Body = unsafe { corefoundation::base::allocate(class()) };
    if body.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe { (*body).contents = Box::into_raw(Box::new(identifier.to_string())) };
    body as Id
}

/// The identifier `receiver` holds, or `None` for anything else.
///
/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
pub(crate) unsafe fn identifier_of(receiver: Id) -> Option<String> {
    if !unsafe { corefoundation::base::is_kind_of(receiver as CFTypeRef, class()) } {
        return None;
    }
    // SAFETY: an instance of `class()` is a `Body` with a live `contents`.
    unsafe {
        let body = receiver as *const Body;
        (*body).contents.as_ref().cloned()
    }
}

fn text(value: Id) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: an `NSString`, by every caller's declared selector.
    String::from_utf8(unsafe { crate::string::bytes(value) }).ok()
}

fn autoreleased(object: Id) -> Id {
    match object.is_null() {
        true => object,
        false => objc::arc::autorelease(object),
    }
}

/// `+[UTType typeWithIdentifier:]`. Nil for an empty or non-string argument,
/// which is what Apple answers for a bad tag; every other identifier is
/// accepted, even one nothing has declared.
unsafe extern "C" fn type_with_identifier(_class: Id, _sel: Sel, identifier: Id) -> Id {
    let Some(text) = text(identifier) else {
        return std::ptr::null_mut();
    };
    if text.is_empty() {
        return std::ptr::null_mut();
    }
    autoreleased(make(&text))
}

/// `+[UTType typeWithFilenameExtension:]`.
unsafe extern "C" fn type_with_filename_extension(_class: Id, _sel: Sel, extension: Id) -> Id {
    let Some(extension) = text(extension) else {
        return std::ptr::null_mut();
    };
    match identifier_for(FILENAME_EXTENSION, &extension) {
        Some(id) => autoreleased(make(&id)),
        None => std::ptr::null_mut(),
    }
}

/// `+[UTType typeWithFilenameExtension:conformingToType:]`. The conformance
/// hint narrows nothing here - the same simplification
/// `UTTypeCreatePreferredIdentifierForTag` already makes for its hint.
unsafe extern "C" fn type_with_filename_extension_conforming_to(
    class: Id,
    sel: Sel,
    extension: Id,
    _conforming_to: Id,
) -> Id {
    // SAFETY: the same arguments the plain form takes.
    unsafe { type_with_filename_extension(class, sel, extension) }
}

/// `+[UTType typeWithMIMEType:]`.
unsafe extern "C" fn type_with_mime_type(_class: Id, _sel: Sel, mime_type: Id) -> Id {
    let Some(mime_type) = text(mime_type) else {
        return std::ptr::null_mut();
    };
    match identifier_for(MIME_TYPE, &mime_type) {
        Some(id) => autoreleased(make(&id)),
        None => std::ptr::null_mut(),
    }
}

/// `-[UTType identifier]`.
unsafe extern "C" fn identifier(receiver: Id, _sel: Sel) -> Id {
    match unsafe { identifier_of(receiver) } {
        Some(id) => autoreleased(crate::string::from_str(&id)),
        None => std::ptr::null_mut(),
    }
}

/// `-[UTType preferredFilenameExtension]`.
unsafe extern "C" fn preferred_filename_extension(receiver: Id, _sel: Sel) -> Id {
    let Some(id) = (unsafe { identifier_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    match tag_for(&id, FILENAME_EXTENSION) {
        Some(ext) => autoreleased(crate::string::from_str(&ext)),
        None => std::ptr::null_mut(),
    }
}

/// `-[UTType preferredMIMEType]`.
unsafe extern "C" fn preferred_mime_type(receiver: Id, _sel: Sel) -> Id {
    let Some(id) = (unsafe { identifier_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    match tag_for(&id, MIME_TYPE) {
        Some(mime) => autoreleased(crate::string::from_str(&mime)),
        None => std::ptr::null_mut(),
    }
}

/// `-[UTType conformsToType:]`.
unsafe extern "C" fn conforms_to_type(receiver: Id, _sel: Sel, other: Id) -> u8 {
    let (Some(a), Some(b)) = (unsafe { identifier_of(receiver) }, unsafe {
        identifier_of(other)
    }) else {
        return 0;
    };
    u8::from(conforms(&a, &b))
}

/// `-[UTType isEqual:]`. UTI comparisons are case-insensitive, like the table
/// in `corefoundation::uti` matches tags against.
unsafe extern "C" fn is_equal(receiver: Id, _sel: Sel, other: Id) -> u8 {
    let (Some(a), Some(b)) = (unsafe { identifier_of(receiver) }, unsafe {
        identifier_of(other)
    }) else {
        return 0;
    };
    u8::from(a.eq_ignore_ascii_case(&b))
}

/// `-[UTType hash]`, over the same case-folded identifier `-isEqual:` compares.
unsafe extern "C" fn hash(receiver: Id, _sel: Sel) -> usize {
    use std::hash::{Hash, Hasher};
    let Some(id) = (unsafe { identifier_of(receiver) }) else {
        return 0;
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.to_ascii_lowercase().hash(&mut hasher);
    hasher.finish() as usize
}

/// `-[UTType localizedDescription]`.
///
/// FIXME (ACCURACY): there is no Launch Services database of human-readable
/// type names here, so this answers the identifier itself rather than nil -
/// a usable label beats no label for UI text that reads it.
unsafe extern "C" fn localized_description(receiver: Id, sel: Sel) -> Id {
    unsafe { identifier(receiver, sel) }
}

pub(crate) fn install() {
    let class = class();
    class_methods!(class, {
        "typeWithIdentifier:" => type_with_identifier, c"@24@0:8@16";
        "typeWithFilenameExtension:" => type_with_filename_extension, c"@24@0:8@16";
        "typeWithFilenameExtension:conformingToType:" => type_with_filename_extension_conforming_to, c"@32@0:8@16@24";
        "typeWithMIMEType:" => type_with_mime_type, c"@24@0:8@16";
    });
    methods!(class, {
        "identifier" => identifier, c"@16@0:8";
        "preferredFilenameExtension" => preferred_filename_extension, c"@16@0:8";
        "preferredMIMEType" => preferred_mime_type, c"@16@0:8";
        "conformsToType:" => conforms_to_type, c"B24@0:8@16";
        "isEqual:" => is_equal, c"B24@0:8@16";
        "hash" => hash, c"Q16@0:8";
        "localizedDescription" => localized_description, c"@16@0:8";
    });
}

/// `UTType *const UTTypeX`: each a true global holding a cached instance, not
/// a method - that is how Apple exports these twelve (confirmed against the
/// framework's `.tbd`: they are plain `symbols`, not `objc-classes` members).
const CONSTANTS: &[(&str, &str)] = &[
    ("_UTTypeItem", "public.item"),
    ("_UTTypeData", "public.data"),
    ("_UTTypeContent", "public.content"),
    ("_UTTypeImage", "public.image"),
    ("_UTTypePNG", "public.png"),
    ("_UTTypeJPEG", "public.jpeg"),
    ("_UTTypeFolder", "public.folder"),
    ("_UTTypeDirectory", "public.directory"),
    ("_UTTypePlainText", "public.plain-text"),
    ("_UTTypePDF", "com.adobe.pdf"),
    ("_UTTypeMovie", "public.movie"),
    ("_UTTypeMPEG4Movie", "public.mpeg-4"),
];

/// A `UTType` for `path`'s resource type, used to answer `NSURLContentTypeKey`.
/// `None` when the resource cannot be examined at all.
///
/// FIXME (ACCURACY): extension-only, like the rest of `corefoundation::uti` -
/// no content sniffing, and no package-bundle distinction for a directory.
pub(crate) fn for_path(path: &std::path::Path) -> Option<Id> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let identifier = if metadata.is_dir() {
        "public.folder".to_string()
    } else {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| identifier_for(FILENAME_EXTENSION, ext))
            .unwrap_or_else(|| "public.data".to_string())
    };
    Some(make(&identifier))
}

/// Every `_UTTypeX` global this framework exports beside its class; the class
/// itself comes from `"UTType"` in `CLASSES`.
pub fn data_symbols() -> Vec<(String, u64)> {
    CONSTANTS
        .iter()
        .map(|(symbol, identifier)| {
            let object = make(identifier);
            // Immortal: a process-lifetime singleton, like the other shared
            // instances this tree makes once and never releases.
            objc::arc::retain(object);
            (
                (*symbol).to_string(),
                corefoundation::base::global_cell(symbol, object as usize as u64),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_with_identifier_round_trips_its_identifier() {
        crate::bootstrap();
        let id = crate::string::from_str("public.png");
        // SAFETY: a class method dispatched with its declared signature.
        let ut = unsafe {
            type_with_identifier(
                std::ptr::null_mut(),
                objc::sel::get("typeWithIdentifier:"),
                id,
            )
        };
        assert!(!ut.is_null());
        // SAFETY: an instance method on a live `UTType`.
        let answer = unsafe { identifier(ut, objc::sel::get("identifier")) };
        assert_eq!(
            String::from_utf8(unsafe { crate::string::bytes(answer) }).unwrap(),
            "public.png"
        );
        objc::arc::release(id);
    }

    #[test]
    fn an_empty_identifier_is_refused() {
        crate::bootstrap();
        let id = crate::string::from_str("");
        // SAFETY: as above.
        let ut = unsafe {
            type_with_identifier(
                std::ptr::null_mut(),
                objc::sel::get("typeWithIdentifier:"),
                id,
            )
        };
        assert!(ut.is_null());
        objc::arc::release(id);
    }

    #[test]
    fn filename_extension_and_mime_type_resolve_to_the_same_type() {
        crate::bootstrap();
        let ext = crate::string::from_str("png");
        let mime = crate::string::from_str("image/png");
        // SAFETY: class methods dispatched with their declared signatures.
        let by_ext = unsafe {
            type_with_filename_extension(
                std::ptr::null_mut(),
                objc::sel::get("typeWithFilenameExtension:"),
                ext,
            )
        };
        let by_mime = unsafe {
            type_with_mime_type(
                std::ptr::null_mut(),
                objc::sel::get("typeWithMIMEType:"),
                mime,
            )
        };
        // SAFETY: instance methods on live `UTType`s.
        assert_ne!(unsafe { identifier_of(by_ext) }, None);
        assert_eq!(unsafe { identifier_of(by_ext) }, unsafe {
            identifier_of(by_mime)
        });
        for object in [ext, mime] {
            objc::arc::release(object);
        }
    }

    #[test]
    fn conforms_to_and_equality_are_case_insensitive_and_hierarchical() {
        crate::bootstrap();
        let png = make("public.png");
        let image = make("PUBLIC.IMAGE");
        let movie = make("public.movie");
        // SAFETY: instance methods on live `UTType`s.
        unsafe {
            assert_eq!(
                conforms_to_type(png, objc::sel::get("conformsToType:"), image),
                1
            );
            assert_eq!(
                conforms_to_type(png, objc::sel::get("conformsToType:"), movie),
                0
            );
            assert_eq!(is_equal(image, objc::sel::get("isEqual:"), image), 1);
            let same_case = make("public.image");
            assert_eq!(is_equal(image, objc::sel::get("isEqual:"), same_case), 1);
            assert_eq!(
                hash(image, objc::sel::get("hash")),
                hash(same_case, objc::sel::get("hash"))
            );
            for object in [png, image, movie, same_case] {
                objc::arc::release(object);
            }
        }
    }

    #[test]
    fn the_exported_constants_answer_their_documented_identifiers() {
        crate::bootstrap();
        let symbols = data_symbols();
        let identifier_of_symbol = |name: &str| {
            let (_, cell) = symbols.iter().find(|(s, _)| s == name).expect(name);
            // SAFETY: a cell this function leaked, holding one object pointer.
            let object = unsafe { *(*cell as *const u64) } as Id;
            unsafe { identifier_of(object) }
        };
        assert_eq!(
            identifier_of_symbol("_UTTypePNG"),
            Some("public.png".to_string())
        );
        assert_eq!(
            identifier_of_symbol("_UTTypeItem"),
            Some("public.item".to_string())
        );
        assert_eq!(
            identifier_of_symbol("_UTTypeMPEG4Movie"),
            Some("public.mpeg-4".to_string())
        );
    }
}
