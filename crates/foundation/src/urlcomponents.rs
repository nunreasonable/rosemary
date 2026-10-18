use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
struct Body {
    isa: *const Class,
    contents: *mut Contents,
}

#[derive(Default)]
struct Contents {
    scheme: String,
    host: String,
    path: String,
    /// Still percent-encoded, and `None` for a URL with no `?` at all, which
    /// is what separates `queryItems` answering nil from answering empty.
    query: Option<String>,
}

unsafe extern "C" fn init(receiver: Id, _sel: Sel) -> Id {
    let body = receiver as *mut Body;
    // SAFETY: `receiver` is a newly allocated instance of this class.
    unsafe {
        if (*body).contents.is_null() {
            (*body).contents = Box::into_raw(Box::new(Contents::default()));
        }
    }
    receiver
}

unsafe fn contents<'a>(receiver: Id) -> Option<&'a mut Contents> {
    let body = receiver as *mut Body;
    // SAFETY: callers only pass instances of this class.
    unsafe { (*body).contents.as_mut() }
}

unsafe fn text(value: Id) -> String {
    // SAFETY: setter arguments are NSStrings or nil.
    String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned()
}

unsafe extern "C" fn set_path(receiver: Id, _sel: Sel, value: Id) {
    if let Some(contents) = unsafe { contents(receiver) } {
        contents.path = unsafe { text(value) };
    }
}

unsafe extern "C" fn set_scheme(receiver: Id, _sel: Sel, value: Id) {
    if let Some(contents) = unsafe { contents(receiver) } {
        contents.scheme = unsafe { text(value) };
    }
}

unsafe extern "C" fn set_host(receiver: Id, _sel: Sel, value: Id) {
    if let Some(contents) = unsafe { contents(receiver) } {
        contents.host = unsafe { text(value) };
    }
}

unsafe fn component(receiver: Id, get: impl FnOnce(&Contents) -> &str) -> Id {
    let Some(contents) = (unsafe { contents(receiver) }) else {
        return std::ptr::null_mut();
    };
    let value = corefoundation::string::from_bytes(get(contents).as_bytes(), false);
    // SAFETY: a newly created string returned by a non-create getter.
    unsafe { corefoundation::api::CFAutorelease(value) as Id }
}

unsafe extern "C" fn path(receiver: Id, _sel: Sel) -> Id {
    unsafe { component(receiver, |contents| &contents.path) }
}

unsafe extern "C" fn scheme(receiver: Id, _sel: Sel) -> Id {
    unsafe { component(receiver, |contents| &contents.scheme) }
}

unsafe extern "C" fn host(receiver: Id, _sel: Sel) -> Id {
    unsafe { component(receiver, |contents| &contents.host) }
}

unsafe extern "C" fn url(receiver: Id, _sel: Sel) -> Id {
    let Some(contents) = (unsafe { contents(receiver) }) else {
        return std::ptr::null_mut();
    };
    if contents.scheme.is_empty() {
        return std::ptr::null_mut();
    }
    let mut value = contents.scheme.clone();
    value.push_str("://");
    value.push_str(&contents.host);
    if !contents.path.is_empty() && !contents.path.starts_with('/') {
        value.push('/');
    }
    value.push_str(&contents.path);
    let value = corefoundation::url::new(&value);
    // SAFETY: a newly created URL object.
    unsafe { corefoundation::api::CFAutorelease(value) as Id }
}

unsafe extern "C" fn components_with_url(
    class: Id,
    _sel: Sel,
    source: Id,
    _resolve_base: u8,
) -> Id {
    // SAFETY: the argument is an NSURL or nil.
    let Some(source) = (unsafe { corefoundation::url::as_url(source.cast()) }) else {
        return std::ptr::null_mut();
    };
    let scheme = source.scheme();
    let host = source
        .text
        .strip_prefix(&format!("{scheme}://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or_default()
        .to_string();
    let path = source.path();
    let query = source
        .text
        .split('#')
        .next()
        .and_then(|text| text.split_once('?'))
        .map(|(_, query)| query.to_string());
    // SAFETY: the receiver is the NSURLComponents class object.
    let Some(class) = (unsafe { objc::class::from_ptr(class.cast()) }) else {
        return std::ptr::null_mut();
    };
    let object = objc::object::create_instance(class, 0);
    // SAFETY: the new object's body has this class's registered layout.
    unsafe {
        init(object, objc::sel::get("init"));
        if let Some(contents) = contents(object) {
            contents.scheme = scheme;
            contents.host = host;
            contents.path = path;
            contents.query = query;
        }
    }
    // SAFETY: a freshly created object returned by a non-create class method.
    unsafe { corefoundation::api::CFAutorelease(object.cast()) as Id }
}

/// Percent-decoded, as `NSURLComponents` decodes a query item. A `+` is left
/// alone: it is a space only to HTML forms, not to a URL.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .filter(|_| bytes[index] == b'%')
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A query item's name and, for one written with an `=`, its value.
#[repr(C)]
struct QueryItem {
    isa: *const Class,
    item: *mut (String, Option<String>),
}

fn query_item_class() -> &'static Class {
    corefoundation::base::register(c"NSURLQueryItem", None, size_of::<QueryItem>(), false)
}

fn new_query_item(name: String, value: Option<String>) -> Id {
    let object = objc::object::create_instance(query_item_class(), 0);
    // SAFETY: a fresh instance with this class's layout.
    unsafe { (*(object as *mut QueryItem)).item = Box::into_raw(Box::new((name, value))) };
    object
}

unsafe fn query_item<'a>(receiver: Id) -> Option<&'a (String, Option<String>)> {
    // SAFETY: callers only pass instances of `NSURLQueryItem`.
    unsafe { (*(receiver as *mut QueryItem)).item.as_ref() }
}

unsafe extern "C" fn query_item_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `NSURLQueryItem`.
    let Some((name, _)) = (unsafe { query_item(receiver) }) else {
        return std::ptr::null_mut();
    };
    let value = corefoundation::string::from_bytes(name.as_bytes(), false);
    // SAFETY: a newly created string returned by a non-create getter.
    unsafe { corefoundation::api::CFAutorelease(value) as Id }
}

unsafe extern "C" fn query_item_value(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `NSURLQueryItem`.
    let Some((_, Some(value))) = (unsafe { query_item(receiver) }) else {
        return std::ptr::null_mut();
    };
    let value = corefoundation::string::from_bytes(value.as_bytes(), false);
    // SAFETY: a newly created string returned by a non-create getter.
    unsafe { corefoundation::api::CFAutorelease(value) as Id }
}

unsafe extern "C" fn query_item_dealloc(receiver: Id, _sel: Sel) {
    let body = receiver as *mut QueryItem;
    // SAFETY: this class owns the boxed pair in its second word.
    unsafe {
        if !(*body).item.is_null() {
            drop(Box::from_raw((*body).item));
            (*body).item = std::ptr::null_mut();
        }
        objc::arc::finalize(receiver);
    }
}

/// `-queryItems`: nil for a URL with no query, and otherwise one item per
/// `&`-separated field, empty fields included, as Foundation splits them.
unsafe extern "C" fn query_items(receiver: Id, _sel: Sel) -> Id {
    let Some(query) = (unsafe { contents(receiver) }).and_then(|c| c.query.clone()) else {
        return std::ptr::null_mut();
    };
    let items: Vec<CFTypeRef> = query
        .split('&')
        .map(|field| {
            let (name, value) = match field.split_once('=') {
                Some((name, value)) => (name, Some(percent_decode(value))),
                None => (field, None),
            };
            new_query_item(percent_decode(name), value) as CFTypeRef
        })
        .collect();
    let array = corefoundation::collections::new_array(
        &items,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for item in items {
        // SAFETY: the array holds its own reference to each item.
        unsafe { corefoundation::api::CFRelease(item) };
    }
    // SAFETY: a newly created array returned by a non-create getter.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    let body = receiver as *mut Body;
    // SAFETY: this class owns the boxed contents in its second word.
    unsafe {
        if !(*body).contents.is_null() {
            drop(Box::from_raw((*body).contents));
            (*body).contents = std::ptr::null_mut();
        }
        objc::arc::finalize(receiver);
    }
}

pub(crate) fn install() {
    // Registered now rather than at first use, so that it is there when the
    // class symbols are published.
    query_item_class();
    let class = corefoundation::base::register(c"NSURLComponents", None, size_of::<Body>(), false);
    methods!(class, {
        "init" => init, c"@16@0:8";
        "setPath:" => set_path, c"v24@0:8@16";
        "setScheme:" => set_scheme, c"v24@0:8@16";
        "setHost:" => set_host, c"v24@0:8@16";
        "path" => path, c"@16@0:8";
        "scheme" => scheme, c"@16@0:8";
        "host" => host, c"@16@0:8";
        "URL" => url, c"@16@0:8";
        "queryItems" => query_items, c"@16@0:8";
        "dealloc" => dealloc, c"v16@0:8"
    });
    methods!(query_item_class(), {
        "name" => query_item_name, c"@16@0:8";
        "value" => query_item_value, c"@16@0:8";
        "dealloc" => query_item_dealloc, c"v16@0:8"
    });
    class_methods!(class, {
        "componentsWithURL:resolvingAgainstBaseURL:" => components_with_url, c"@32@0:8@16B24"
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(value: &str) -> Id {
        corefoundation::string::from_bytes(value.as_bytes(), false) as Id
    }

    #[test]
    fn setters_build_an_absolute_url() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSURLComponents").unwrap();
        let object = objc::object::create_instance(class, 0);
        // SAFETY: direct calls with the Objective-C signatures installed above.
        unsafe {
            init(object, objc::sel::get("init"));
            set_scheme(object, objc::sel::get("setScheme:"), string("https"));
            set_host(object, objc::sel::get("setHost:"), string("example.test"));
            set_path(object, objc::sel::get("setPath:"), string("login"));
            let result = url(object, objc::sel::get("URL"));
            assert_eq!(
                corefoundation::url::as_url(result.cast()).map(|u| u.text.as_str()),
                Some("https://example.test/login")
            );
            objc::arc::release(object);
        }
    }

    #[test]
    fn query_items_are_split_and_percent_decoded() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSURLComponents").unwrap();
        let source = corefoundation::url::new(
            "https://messages.example.test/message?msg0=%7B%22a%22%3A1%7D&flag&x=a+b#frag",
        ) as Id;
        // SAFETY: direct calls with the Objective-C signatures installed above.
        unsafe {
            let object = components_with_url(
                objc::class::ptr(class).cast(),
                objc::sel::get("componentsWithURL:resolvingAgainstBaseURL:"),
                source,
                0,
            );
            let items = query_items(object, objc::sel::get("queryItems"));
            let array = corefoundation::collections::array(items as CFTypeRef).unwrap();
            let fields: Vec<(Vec<u8>, Option<Vec<u8>>)> = array
                .values
                .iter()
                .map(|item| {
                    let item = *item as Id;
                    let value = query_item_value(item, objc::sel::get("value"));
                    (
                        crate::string::bytes(query_item_name(item, objc::sel::get("name"))),
                        (!value.is_null()).then(|| crate::string::bytes(value)),
                    )
                })
                .collect();
            assert_eq!(
                fields,
                [
                    (b"msg0".to_vec(), Some(b"{\"a\":1}".to_vec())),
                    (b"flag".to_vec(), None),
                    (b"x".to_vec(), Some(b"a+b".to_vec())),
                ]
            );
            let bare = corefoundation::url::new("https://example.test/") as Id;
            let object = components_with_url(
                objc::class::ptr(class).cast(),
                objc::sel::get("componentsWithURL:resolvingAgainstBaseURL:"),
                bare,
                0,
            );
            assert!(query_items(object, objc::sel::get("queryItems")).is_null());
        }
    }

    #[test]
    fn components_with_url_preserves_absolute_url_parts() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSURLComponents").unwrap();
        let source = corefoundation::url::new("https://example.test/original/path") as Id;
        // SAFETY: direct calls with the Objective-C signatures installed above.
        unsafe {
            let object = components_with_url(
                objc::class::ptr(class).cast(),
                objc::sel::get("componentsWithURL:resolvingAgainstBaseURL:"),
                source,
                0,
            );
            let result = url(object, objc::sel::get("URL"));
            assert_eq!(
                crate::string::bytes(path(object, objc::sel::get("path"))),
                b"/original/path"
            );
            assert_eq!(
                corefoundation::url::as_url(result.cast()).map(|u| u.text.as_str()),
                Some("https://example.test/original/path")
            );
            objc::arc::release(object);
            corefoundation::api::CFRelease(source.cast());
        }
    }
}
