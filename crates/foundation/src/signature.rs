use std::ffi::{CStr, CString, c_char, c_void};

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
struct Signature {
    isa: *const c_void,
    /// Owned; freed by `-dealloc`.
    parsed: *mut Parsed,
}

struct Parsed {
    /// The return type first, then one per argument including `self` and `_cmd`.
    types: Vec<CString>,
    frame_length: usize,
}

fn signature_class() -> &'static objc::class::Class {
    corefoundation::base::register(
        c"NSMethodSignature",
        None,
        std::mem::size_of::<Signature>(),
        false,
    )
}

/// Split an encoding such as `v24@0:8@16` into its types, dropping offsets.
///
/// `None` for an encoding that does not parse, which `+signatureWithObjCTypes:`
/// answers with nil rather than a signature with the wrong arity.
fn parse(encoding: &[u8]) -> Option<Parsed> {
    let mut types = Vec::new();
    let mut frame_length = None;
    let mut rest = encoding;
    while !rest.is_empty() {
        let length = type_length(rest)?;
        types.push(CString::new(&rest[..length]).ok()?);
        rest = &rest[length..];
        let digits = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b'-' || **b == b'+')
            .count();
        if types.len() == 1 {
            frame_length = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok();
        }
        rest = &rest[digits..];
    }
    if types.is_empty() {
        return None;
    }
    let frame_length = frame_length.unwrap_or_else(|| {
        types[1..]
            .iter()
            .map(|t| size_of_type(t.as_bytes()).unwrap_or(8).next_multiple_of(8))
            .sum()
    });
    Some(Parsed {
        types,
        frame_length,
    })
}

/// The length of the single type at the front of `bytes`, qualifiers included.
fn type_length(bytes: &[u8]) -> Option<usize> {
    let qualifiers = bytes.iter().take_while(|b| b"rnNoORVA".contains(b)).count();
    let rest = &bytes[qualifiers..];
    let length = match *rest.first()? {
        b'^' => 1 + type_length(&rest[1..])?,
        b'{' => bracketed(rest, b'{', b'}')?,
        b'(' => bracketed(rest, b'(', b')')?,
        b'[' => bracketed(rest, b'[', b']')?,
        b'b' => 1 + rest[1..].iter().take_while(|b| b.is_ascii_digit()).count(),
        b'@' => match rest.get(1) {
            Some(b'?') => 2,
            Some(b'"') => 2 + rest[2..].iter().position(|b| *b == b'"')? + 1,
            _ => 1,
        },
        c if b"cislqCISLQfdDBv*#:?".contains(&c) => 1,
        _ => return None,
    };
    Some(qualifiers + length)
}

fn bracketed(bytes: &[u8], open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == open {
            depth += 1;
        } else if *b == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
    }
    None
}

/// The size of one x86-64/arm64 type, or `None` where it cannot be known.
pub(crate) fn size_of_type(bytes: &[u8]) -> Option<usize> {
    if type_length(bytes)? != bytes.len() {
        return None;
    }
    let bytes = &bytes[bytes.iter().take_while(|b| b"rnNoORVA".contains(b)).count()..];
    Some(match *bytes.first()? {
        b'c' | b'C' | b'B' => 1,
        b's' | b'S' => 2,
        b'i' | b'I' | b'f' => 4,
        b'l' | b'L' | b'q' | b'Q' | b'd' | b'^' | b'*' | b'#' | b':' | b'@' | b'?' => 8,
        b'D' => 16,
        b'v' => 0,
        b'{' => {
            let inner = &bytes[1..bytes.len() - 1];
            let fields = &inner[inner.iter().position(|b| *b == b'=')? + 1..];
            let (mut size, mut align, mut rest) = (0usize, 1usize, fields);
            while !rest.is_empty() {
                let length = type_length(rest)?;
                let field = size_of_type(&rest[..length])?;
                let a = alignment_of_type(&rest[..length])?;
                size = size.next_multiple_of(a) + field;
                align = align.max(a);
                rest = &rest[length..];
            }
            size.next_multiple_of(align)
        }
        _ => return None,
    })
}

fn alignment_of_type(bytes: &[u8]) -> Option<usize> {
    let bytes = &bytes[bytes.iter().take_while(|b| b"rnNoORVA".contains(b)).count()..];
    match *bytes.first()? {
        b'{' => {
            let inner = &bytes[1..bytes.len() - 1];
            let mut rest = &inner[inner.iter().position(|b| *b == b'=')? + 1..];
            let mut align = 1;
            while !rest.is_empty() {
                let length = type_length(rest)?;
                align = align.max(alignment_of_type(&rest[..length])?);
                rest = &rest[length..];
            }
            Some(align)
        }
        _ => size_of_type(bytes).map(|s| s.clamp(1, 8)),
    }
}

fn new_signature(encoding: &[u8]) -> Id {
    let Some(parsed) = parse(encoding) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the body begins with `isa` and matches the registered size.
    let signature = unsafe { corefoundation::base::allocate::<Signature>(signature_class()) };
    // SAFETY: a fresh instance whose payload is ours to initialize.
    unsafe { (*signature).parsed = Box::into_raw(Box::new(parsed)) };
    objc::arc::autorelease(signature as Id)
}

fn parsed(receiver: Id) -> &'static Parsed {
    // SAFETY: installed only on `NSMethodSignature`, whose `parsed` is set at
    // creation and lives until `-dealloc`.
    unsafe { &*(*(receiver as *const Signature)).parsed }
}

unsafe extern "C" fn signature_with_types(_class: Id, _sel: Sel, types: *const c_char) -> Id {
    if types.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller passed a NUL-terminated encoding.
    new_signature(unsafe { CStr::from_ptr(types) }.to_bytes())
}

unsafe extern "C" fn number_of_arguments(receiver: Id, _sel: Sel) -> usize {
    parsed(receiver).types.len() - 1
}

unsafe extern "C" fn argument_type(receiver: Id, _sel: Sel, index: usize) -> *const c_char {
    match parsed(receiver).types.get(index + 1) {
        Some(t) => t.as_ptr(),
        None => {
            objc::fail!("-[NSMethodSignature getArgumentTypeAtIndex:]: index {index} out of bounds")
        }
    }
}

unsafe extern "C" fn return_type(receiver: Id, _sel: Sel) -> *const c_char {
    parsed(receiver).types[0].as_ptr()
}

unsafe extern "C" fn return_length(receiver: Id, _sel: Sel) -> usize {
    size_of_type(parsed(receiver).types[0].as_bytes()).unwrap_or(0)
}

unsafe extern "C" fn frame_length(receiver: Id, _sel: Sel) -> usize {
    parsed(receiver).frame_length
}

unsafe extern "C" fn is_oneway(receiver: Id, _sel: Sel) -> bool {
    parsed(receiver).types[0].as_bytes().starts_with(b"V")
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: the box `new_signature` leaked, freed once here.
    unsafe { drop(Box::from_raw((*(receiver as *mut Signature)).parsed)) };
    objc::arc::finalize(receiver);
}

/// `-methodSignatureForSelector:`: nil for a message the receiver does not
/// answer, which is how a caller tells the two apart.
unsafe extern "C" fn method_signature_for_selector(receiver: Id, _sel: Sel, selector: Sel) -> Id {
    // SAFETY: the receiver is a live object; a class receiver resolves through
    // its metaclass, which is where its class methods are.
    let method = unsafe { objc::object::class_of(receiver) }.and_then(|c| c.method(selector));
    signature_of(method)
}

unsafe extern "C" fn instance_method_signature_for_selector(
    receiver: Id,
    _sel: Sel,
    selector: Sel,
) -> Id {
    // SAFETY: the receiver is a class.
    let class = unsafe { objc::class::from_ptr(receiver as *const objc::class::Class) };
    signature_of(class.and_then(|c| c.method(selector)))
}

fn signature_of(method: Option<&'static objc::class::Method>) -> Id {
    let Some(method) = method else {
        return std::ptr::null_mut();
    };
    let types = method.types();
    if types.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a method's types are a NUL-terminated string the runtime keeps.
    new_signature(unsafe { CStr::from_ptr(types) }.to_bytes())
}

pub(crate) fn install() {
    let class = signature_class();
    class_methods!(class, {
        "signatureWithObjCTypes:" => signature_with_types, c"@24@0:8r*16";
    });
    methods!(class, {
        "numberOfArguments" => number_of_arguments, c"Q16@0:8";
        "getArgumentTypeAtIndex:" => argument_type, c"r*24@0:8Q16";
        "methodReturnType" => return_type, c"r*16@0:8";
        "methodReturnLength" => return_length, c"Q16@0:8";
        "frameLength" => frame_length, c"Q16@0:8";
        "isOneway" => is_oneway, c"B16@0:8";
        "dealloc" => dealloc, c"v16@0:8";
    });
    let nsobject = objc::class::lookup(c"NSObject").expect("the runtime's own NSObject");
    methods!(nsobject, {
        "methodSignatureForSelector:" => method_signature_for_selector, c"@24@0:8:16";
    });
    class_methods!(nsobject, {
        "methodSignatureForSelector:" => method_signature_for_selector, c"@24@0:8:16";
        "instanceMethodSignatureForSelector:" => instance_method_signature_for_selector,
            c"@24@0:8:16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_an_encoding_into_types() {
        let parsed =
            parse(b"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16")
                .expect("parses");
        assert_eq!(parsed.types.len(), 4);
        assert_eq!(parsed.types[2].as_bytes(), b":");
        assert_eq!(parsed.frame_length, 48);
        assert_eq!(size_of_type(parsed.types[0].as_bytes()), Some(32));
        assert_eq!(size_of_type(b"{x=cid}"), Some(16));
    }

    #[test]
    fn answers_nil_for_a_missing_selector() {
        crate::bootstrap();
        let pool = objc::arc::pool_push();
        let class = objc::class::lookup(c"NSObject").expect("NSObject");
        let object = objc::object::create_instance(class, 0);
        let sel = objc::sel::get("methodSignatureForSelector:");
        // SAFETY: a live object and registered selectors.
        unsafe {
            assert!(
                method_signature_for_selector(object, sel, objc::sel::get("noSuchThing")).is_null()
            );
            let signature = method_signature_for_selector(object, sel, objc::sel::get("isEqual:"));
            assert!(!signature.is_null());
            assert_eq!(number_of_arguments(signature, sel), 3);
            assert_eq!(CStr::from_ptr(return_type(signature, sel)).to_bytes(), b"B");
            assert_eq!(return_length(signature, sel), 1);
        }
        objc::arc::pool_pop(pool);
        objc::arc::release(object);
    }
}
