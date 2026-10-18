//! `NSStream` and `NSInputStream`, `NSURLSessionConfiguration`, and
//! ScriptingBridge's `SBApplication`.
//!
//! The streams are here mostly to be subclassed: a program hands
//! `-setHTTPBodyStream:` a stream of its own, and binding `NSInputStream` as a
//! superclass needs the class to exist. Crashpad's upload transport is one. A
//! stream over an `NSData` is the one concrete kind.

use std::sync::Mutex;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// `NSStreamStatus`.
mod status {
    pub const NOT_OPEN: u64 = 0;
    pub const OPEN: u64 = 2;
    pub const AT_END: u64 = 5;
    pub const CLOSED: u64 = 6;
}

/// A stream over bytes, by the stream's address. A side table rather than
/// fields, because the class is a superclass: a guest's subclass lays its own
/// ivars out after `NSInputStream`'s instance size, which has to stay Apple's.
struct Bytes {
    data: Vec<u8>,
    offset: usize,
    status: u64,
}

static STREAMS: Mutex<Vec<(usize, Bytes)>> = Mutex::new(Vec::new());

fn with_stream<R>(receiver: Id, f: impl FnOnce(&mut Bytes) -> R) -> Option<R> {
    let mut streams = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    streams
        .iter_mut()
        .find(|(s, _)| *s == receiver as usize)
        .map(|(_, bytes)| f(bytes))
}

unsafe extern "C" fn nothing(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn nil(_receiver: Id, _sel: Sel) -> Id {
    std::ptr::null_mut()
}

unsafe extern "C" fn ignore_object(_receiver: Id, _sel: Sel, _object: Id) {}

unsafe extern "C" fn ignore_two(_receiver: Id, _sel: Sel, _a: Id, _b: Id) {}

unsafe extern "C" fn property_for_key(_receiver: Id, _sel: Sel, _key: Id) -> Id {
    std::ptr::null_mut()
}

unsafe extern "C" fn set_property(_receiver: Id, _sel: Sel, _value: Id, _key: Id) -> u8 {
    0
}

unsafe extern "C" fn open(receiver: Id, _sel: Sel) {
    with_stream(receiver, |s| {
        if s.status == status::NOT_OPEN {
            s.status = match s.data.is_empty() {
                true => status::AT_END,
                false => status::OPEN,
            };
        }
    });
}

unsafe extern "C" fn close(receiver: Id, _sel: Sel) {
    with_stream(receiver, |s| s.status = status::CLOSED);
}

unsafe extern "C" fn stream_status(receiver: Id, _sel: Sel) -> u64 {
    with_stream(receiver, |s| s.status).unwrap_or(status::NOT_OPEN)
}

/// `-initWithData:`: a stream reading `data`'s bytes as they are now.
unsafe extern "C" fn init_with_data(receiver: Id, _sel: Sel, data: Id) -> Id {
    // SAFETY: an `NSData`, or anything else, which answers no bytes.
    let bytes =
        unsafe { corefoundation::collections::data(data as corefoundation::base::CFTypeRef) }
            .cloned()
            .unwrap_or_default();
    STREAMS.lock().unwrap_or_else(|e| e.into_inner()).push((
        receiver as usize,
        Bytes {
            data: bytes,
            offset: 0,
            status: status::NOT_OPEN,
        },
    ));
    receiver
}

unsafe extern "C" fn input_stream_with_data(class: Id, sel: Sel, data: Id) -> Id {
    // SAFETY: the receiver is the class, which `objc` can instantiate.
    let class = unsafe { objc::class::from_ptr(class as *const objc::class::Class) }
        .unwrap_or_else(|| objc::class::lookup(c"NSInputStream").expect("registered"));
    let stream = objc::object::create_instance(class, 0);
    objc::arc::autorelease(unsafe { init_with_data(stream, sel, data) })
}

unsafe extern "C" fn read(receiver: Id, _sel: Sel, buffer: *mut u8, length: u64) -> i64 {
    if buffer.is_null() {
        return -1;
    }
    with_stream(receiver, |s| {
        if s.status != status::OPEN {
            return 0;
        }
        let count = (s.data.len() - s.offset).min(length as usize);
        // SAFETY: the caller's buffer, `length` bytes long.
        unsafe { std::ptr::copy_nonoverlapping(s.data.as_ptr().add(s.offset), buffer, count) };
        s.offset += count;
        if s.offset == s.data.len() {
            s.status = status::AT_END;
        }
        count as i64
    })
    .unwrap_or(-1)
}

unsafe extern "C" fn has_bytes_available(receiver: Id, _sel: Sel) -> u8 {
    with_stream(receiver, |s| {
        u8::from(s.status == status::OPEN && s.offset < s.data.len())
    })
    .unwrap_or(0)
}

unsafe extern "C" fn get_buffer(
    _receiver: Id,
    _sel: Sel,
    _buffer: *mut *mut u8,
    _length: *mut u64,
) -> u8 {
    0
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    STREAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(s, _)| *s != receiver as usize);
    objc::arc::finalize(receiver);
}

/// `+defaultSessionConfiguration` and `+ephemeralSessionConfiguration`: one
/// shared object, because nothing in a configuration is read here - the
/// session this runtime has is the one connection pool either way.
unsafe extern "C" fn configuration(_class: Id, _sel: Sel) -> Id {
    static SHARED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SHARED.get_or_init(|| {
        let class = objc::class::lookup(c"NSURLSessionConfiguration").expect("registered");
        let object = objc::object::create_instance(class, 0);
        objc::arc::make_immortal(object);
        object as usize
    }) as Id
}

unsafe extern "C" fn copy_self(receiver: Id, _sel: Sel, _zone: *mut std::ffi::c_void) -> Id {
    objc::arc::retain(receiver)
}

/// `+[SBApplication applicationWithBundleIdentifier:]`: nil, which is what it
/// answers for an application that is not installed.
unsafe extern "C" fn no_application(_class: Id, _sel: Sel, _identifier: Id) -> Id {
    std::ptr::null_mut()
}

/// Every class this module answers for.
pub const CLASSES: &[&str] = &[
    "NSStream",
    "NSInputStream",
    "NSURLSessionConfiguration",
    "SBApplication",
    "NSURLQueryItem",
];

pub(crate) fn install() {
    let size = size_of::<usize>();
    let stream = corefoundation::base::register(c"NSStream", None, size, false);
    methods!(stream, {
        "open" => nothing, c"v16@0:8";
        "close" => nothing, c"v16@0:8";
        "delegate" => nil, c"@16@0:8";
        "setDelegate:" => ignore_object, c"v24@0:8@16";
        "streamError" => nil, c"@16@0:8";
        "streamStatus" => stream_status, c"Q16@0:8";
        "scheduleInRunLoop:forMode:" => ignore_two, c"v32@0:8@16@24";
        "removeFromRunLoop:forMode:" => ignore_two, c"v32@0:8@16@24";
        "propertyForKey:" => property_for_key, c"@24@0:8@16";
        "setProperty:forKey:" => set_property, c"c32@0:8@16@24";
    });
    let input = corefoundation::base::register(c"NSInputStream", Some(stream), size, false);
    class_methods!(input, {
        "inputStreamWithData:" => input_stream_with_data, c"@24@0:8@16";
    });
    methods!(input, {
        "initWithData:" => init_with_data, c"@24@0:8@16";
        "open" => open, c"v16@0:8";
        "close" => close, c"v16@0:8";
        "read:maxLength:" => read, c"q32@0:8*16Q24";
        "hasBytesAvailable" => has_bytes_available, c"c16@0:8";
        "getBuffer:length:" => get_buffer, c"c32@0:8^*16^Q24";
        "dealloc" => dealloc, c"v16@0:8";
    });

    let configuration_class =
        corefoundation::base::register(c"NSURLSessionConfiguration", None, size, false);
    class_methods!(configuration_class, {
        "defaultSessionConfiguration" => configuration, c"@16@0:8";
        "ephemeralSessionConfiguration" => configuration, c"@16@0:8";
    });
    methods!(configuration_class, {
        "copyWithZone:" => copy_self, c"@24@0:8^v16";
    });

    let application = corefoundation::base::register(c"SBApplication", None, size, false);
    class_methods!(application, {
        "applicationWithBundleIdentifier:" => no_application, c"@24@0:8@16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send<R>(receiver: Id, name: &str) -> R {
        let f: unsafe extern "C" fn(Id, Sel) -> R =
            unsafe { std::mem::transmute(objc::msg::entry()) };
        unsafe { f(receiver, objc::sel::get(name)) }
    }

    #[test]
    fn a_data_stream_reads_its_bytes_once() {
        crate::bootstrap();
        let data = corefoundation::collections::new_data(b"hello".to_vec()) as Id;
        let class = objc::class::lookup(c"NSInputStream").expect("registered");
        let stream = objc::object::create_instance(class, 0);
        unsafe { init_with_data(stream, objc::sel::get("initWithData:"), data) };
        assert_eq!(send::<u64>(stream, "streamStatus"), status::NOT_OPEN);
        send::<()>(stream, "open");
        assert_eq!(send::<u8>(stream, "hasBytesAvailable"), 1);
        let mut buffer = [0u8; 8];
        let read: unsafe extern "C" fn(Id, Sel, *mut u8, u64) -> i64 =
            unsafe { std::mem::transmute(objc::msg::entry()) };
        let n = unsafe {
            read(
                stream,
                objc::sel::get("read:maxLength:"),
                buffer.as_mut_ptr(),
                8,
            )
        };
        assert_eq!(&buffer[..n as usize], b"hello");
        assert_eq!(send::<u64>(stream, "streamStatus"), status::AT_END);
    }
}
