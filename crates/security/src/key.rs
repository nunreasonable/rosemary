//! Asymmetric keys: elliptic-curve P-256, generated and used in software.
//!
//! A program asks for a signing key, usually in the Secure Enclave first and in
//! the keychain if that fails. There is no Secure Enclave here, and the answer
//! for one is the one a Mac without the chip gives: no key, and an error. What
//! a program gets instead is an ordinary keychain key - a P-256 key pair held
//! in this process and, when it is made permanent, in the keychain file - which
//! is the tier Apple's own API provides on such Macs.
//!
//! Only P-256 and ECDSA over SHA-256 of the message are implemented, the one
//! curve and algorithm `SecKeyCreateRandomKey` callers in this tree ask for.

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

/// The documented string values of the keys and constants used here.
pub const KEYS: &[(&str, &str)] = &[
    ("_kSecClassKey", "keys"),
    ("_kSecAttrAccessControl", "accc"),
    ("_kSecAttrAccessibleWhenUnlockedThisDeviceOnly", "aku"),
    ("_kSecAttrApplicationLabel", "klbl"),
    ("_kSecAttrApplicationTag", "atag"),
    ("_kSecAttrIsPermanent", "perm"),
    ("_kSecAttrKeyClass", "kcls"),
    ("_kSecAttrKeyClassPublic", "0"),
    ("_kSecAttrKeyClassPrivate", "1"),
    ("_kSecAttrKeySizeInBits", "bsiz"),
    ("_kSecAttrKeyType", "type"),
    ("_kSecAttrKeyTypeEC", "73"),
    ("_kSecAttrKeyTypeECSECPrimeRandom", "73"),
    ("_kSecAttrTokenID", "tkid"),
    ("_kSecAttrTokenIDSecureEnclave", "com.apple.setoken"),
    (
        "_kSecKeyAlgorithmECDSASignatureMessageX962SHA256",
        SIGN_MESSAGE_SHA256,
    ),
    ("_kSecPrivateKeyAttrs", "private"),
    ("_kSecPublicKeyAttrs", "public"),
    ("_kSecUseDataProtectionKeychain", "nleg"),
    ("_kSecValueRef", "v_Ref"),
];

const SIGN_MESSAGE_SHA256: &str = "algid:sign:ECDSA:message-X962:SHA256";

/// `kSecKeyOperationTypeSign`.
const OPERATION_SIGN: i64 = 0;

/// `errSecParam`, `errSecUnimplemented` and `errSecUnsupportedAlgorithm`
/// (as Apple reports an algorithm a key cannot do).
const PARAM: i64 = -50;
const UNIMPLEMENTED: i64 = -4;
const UNSUPPORTED: i64 = -67584;

enum Payload {
    /// A private key, as the PKCS#8 document `ring` reads it back from.
    Private { pkcs8: Vec<u8>, tag: Option<Vec<u8>> },
    /// A public key, as the uncompressed point `04 || X || Y`.
    Public(Vec<u8>),
    /// `SecAccessControlRef`: kept, never consulted, because nothing here asks
    /// for a password or a fingerprint before using a key.
    AccessControl,
}

#[repr(C)]
struct Body {
    isa: *const Class,
    payload: *mut Payload,
}

fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class = corefoundation::base::register(c"__R3SecKeyObject", None, size_of::<Body>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(pointer as *const Class).expect("a key class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: this class's layout, and the payload `new` boxed.
    unsafe {
        let body = receiver as *mut Body;
        drop(Box::from_raw((*body).payload));
        objc::arc::finalize(receiver);
    }
}

fn new(payload: Payload) -> CFTypeRef {
    // SAFETY: `Body` is the registered layout.
    let body = unsafe { corefoundation::base::allocate::<Body>(class()) };
    if body.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a freshly allocated body.
    unsafe { (*body).payload = Box::into_raw(Box::new(payload)) };
    body as CFTypeRef
}

/// # Safety
///
/// `object` is null or an object this runtime allocated, live for `'a`.
unsafe fn payload<'a>(object: CFTypeRef) -> Option<&'a Payload> {
    if object.is_null() || !unsafe { corefoundation::base::is_kind_of(object, class()) } {
        return None;
    }
    // SAFETY: a body of this class, whose payload lives as long as it does.
    Some(unsafe { &*(*(object as *const Body)).payload })
}

fn pair(pkcs8: &[u8]) -> Option<EcdsaKeyPair> {
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &SystemRandom::new()).ok()
}

/// The private scalar inside one of `ring`'s P-256 PKCS#8 documents: the
/// 32-byte octet string after the `ECPrivateKey` version.
fn scalar(pkcs8: &[u8]) -> Option<&[u8]> {
    let marker = [0x02, 0x01, 0x01, 0x04, 0x20];
    let at = pkcs8.windows(marker.len()).position(|w| w == marker)? + marker.len();
    pkcs8.get(at..at + 32)
}

/// A private key's PKCS#8 document and tag, for the keychain to store.
///
/// # Safety
///
/// `object` is null or a live object.
pub(crate) unsafe fn private_parts(object: CFTypeRef) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    match unsafe { payload(object) }? {
        Payload::Private { pkcs8, tag } => Some((pkcs8.clone(), tag.clone())),
        _ => None,
    }
}

/// A private key read back from the keychain, owned by the caller.
pub(crate) fn from_stored(pkcs8: Vec<u8>, tag: Option<Vec<u8>>) -> CFTypeRef {
    if pair(&pkcs8).is_none() {
        return std::ptr::null();
    }
    new(Payload::Private { pkcs8, tag })
}

/// The value at `key` in a dictionary.
///
/// # Safety
///
/// `dictionary` is null or a `CFDictionary`.
pub(crate) unsafe fn value(dictionary: CFTypeRef, key: &str) -> CFTypeRef {
    if dictionary.is_null() {
        return std::ptr::null();
    }
    let name = foundation::string::from_str(key);
    // SAFETY: the caller's obligation; `name` is a live string.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(dictionary, name as CFTypeRef) };
    // SAFETY: a fresh string this call owns.
    unsafe { corefoundation::api::CFRelease(name as CFTypeRef) };
    value
}

/// The bytes of a `CFData` or a string.
///
/// # Safety
///
/// `object` is null or a live object.
pub(crate) unsafe fn bytes_of(object: CFTypeRef) -> Option<Vec<u8>> {
    if object.is_null() {
        return None;
    }
    if let Some(data) = unsafe { corefoundation::collections::data(object) } {
        return Some(data.clone());
    }
    Some(unsafe { foundation::string::bytes(object as Id) })
}

/// Whether a dictionary holds true at `key`, as a `CFBoolean` or a number.
///
/// # Safety
///
/// `dictionary` is null or a `CFDictionary`.
unsafe fn truth(dictionary: CFTypeRef, key: &str) -> bool {
    let value = unsafe { value(dictionary, key) };
    if value.is_null() {
        return false;
    }
    if let Some(number) = unsafe { corefoundation::collections::number(value) } {
        return number.as_i64() != 0;
    }
    unsafe { corefoundation::api::CFBooleanGetValue(value) != 0 }
}

/// A string or a number at `key`, as text: `kSecAttrKeyType` is a string in
/// the SDK and a number in older code.
///
/// # Safety
///
/// `dictionary` is null or a `CFDictionary`.
unsafe fn text(dictionary: CFTypeRef, key: &str) -> Option<String> {
    let value = unsafe { value(dictionary, key) };
    if value.is_null() {
        return None;
    }
    if let Some(number) = unsafe { corefoundation::collections::number(value) } {
        return Some(number.as_i64().to_string());
    }
    let bytes = unsafe { foundation::string::bytes(value as Id) };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Hand a caller its `CFErrorRef`, owned by it, when it asked for one.
///
/// # Safety
///
/// `error` is null or writable.
unsafe fn fail(error: *mut CFTypeRef, code: i64, description: &str) -> CFTypeRef {
    if !error.is_null() {
        let object = objc::arc::retain(foundation::error::make(
            "NSOSStatusErrorDomain",
            code,
            description,
        ));
        // SAFETY: writable, per the caller.
        unsafe { error.write(object as CFTypeRef) };
    }
    std::ptr::null()
}

/// `SecAccessControlRef SecAccessControlCreateWithFlags(CFAllocatorRef,
/// CFTypeRef protection, SecAccessControlCreateFlags, CFErrorRef *)`.
///
/// # Safety
///
/// `error` is null or writable.
pub unsafe extern "C" fn SecAccessControlCreateWithFlags(
    _allocator: CFTypeRef,
    protection: CFTypeRef,
    _flags: u64,
    error: *mut CFTypeRef,
) -> CFTypeRef {
    if protection.is_null() {
        return unsafe { fail(error, PARAM, "No protection class was given.") };
    }
    new(Payload::AccessControl)
}

/// `SecKeyRef SecKeyCreateRandomKey(CFDictionaryRef parameters, CFErrorRef *)`.
///
/// # Safety
///
/// `parameters` is null or a `CFDictionary`; `error` is null or writable.
pub unsafe extern "C" fn SecKeyCreateRandomKey(
    parameters: CFTypeRef,
    error: *mut CFTypeRef,
) -> CFTypeRef {
    if parameters.is_null() {
        return unsafe { fail(error, PARAM, "No key parameters were given.") };
    }
    if unsafe { text(parameters, "tkid") }.as_deref() == Some("com.apple.setoken") {
        // A Mac without a Secure Enclave answers this way: the token does not
        // exist, so neither can a key in it.
        return unsafe { fail(error, UNIMPLEMENTED, "Key generation failed, error -4") };
    }
    let kind = unsafe { text(parameters, "type") };
    let bits = unsafe { text(parameters, "bsiz") };
    if kind.as_deref() != Some("73") || !matches!(bits.as_deref(), None | Some("256")) {
        return unsafe {
            fail(
                error,
                UNSUPPORTED,
                "Only 256-bit elliptic curve keys can be generated.",
            )
        };
    }
    let private = unsafe { value(parameters, "private") };
    let tag = unsafe { bytes_of(value(private, "atag")) }
        .or_else(|| unsafe { bytes_of(value(parameters, "atag")) });
    let permanent = unsafe { truth(parameters, "perm") || truth(private, "perm") };
    let Ok(document) =
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
    else {
        return unsafe { fail(error, UNIMPLEMENTED, "Key generation failed.") };
    };
    let pkcs8 = document.as_ref().to_vec();
    if permanent && let Some(tag) = &tag {
        crate::keychain::store_key(tag, &pkcs8);
    }
    new(Payload::Private { pkcs8, tag })
}

/// `SecKeyRef SecKeyCopyPublicKey(SecKeyRef)`.
///
/// # Safety
///
/// `key` is null or a live object.
pub unsafe extern "C" fn SecKeyCopyPublicKey(key: CFTypeRef) -> CFTypeRef {
    match unsafe { payload(key) } {
        Some(Payload::Private { pkcs8, .. }) => match pair(pkcs8) {
            Some(pair) => new(Payload::Public(pair.public_key().as_ref().to_vec())),
            None => std::ptr::null(),
        },
        Some(Payload::Public(point)) => new(Payload::Public(point.clone())),
        _ => std::ptr::null(),
    }
}

/// The key's bytes as `SecKeyCopyExternalRepresentation` gives them: the
/// point for a public key, and the point followed by the scalar for a private
/// one.
fn external(payload: &Payload) -> Option<Vec<u8>> {
    match payload {
        Payload::Private { pkcs8, .. } => {
            let pair = pair(pkcs8)?;
            let mut bytes = pair.public_key().as_ref().to_vec();
            bytes.extend_from_slice(scalar(pkcs8)?);
            Some(bytes)
        }
        Payload::Public(point) => Some(point.clone()),
        Payload::AccessControl => None,
    }
}

/// `CFDataRef SecKeyCopyExternalRepresentation(SecKeyRef, CFErrorRef *)`.
///
/// # Safety
///
/// `key` is null or a live object; `error` is null or writable.
pub unsafe extern "C" fn SecKeyCopyExternalRepresentation(
    key: CFTypeRef,
    error: *mut CFTypeRef,
) -> CFTypeRef {
    match unsafe { payload(key) }.and_then(external) {
        Some(bytes) => corefoundation::collections::new_data(bytes),
        None => unsafe { fail(error, PARAM, "The key cannot be exported.") },
    }
}

/// `CFDataRef SecKeyCreateSignature(SecKeyRef, SecKeyAlgorithm, CFDataRef,
/// CFErrorRef *)`: a DER-encoded ECDSA signature over SHA-256 of `data`.
///
/// # Safety
///
/// `key` and `algorithm` are null or live objects, `data` is null or a
/// `CFData`, and `error` is null or writable.
pub unsafe extern "C" fn SecKeyCreateSignature(
    key: CFTypeRef,
    algorithm: CFTypeRef,
    data: CFTypeRef,
    error: *mut CFTypeRef,
) -> CFTypeRef {
    let Some(Payload::Private { pkcs8, .. }) = (unsafe { payload(key) }) else {
        return unsafe { fail(error, PARAM, "Only a private key can sign.") };
    };
    if unsafe { bytes_of(algorithm) }.as_deref() != Some(SIGN_MESSAGE_SHA256.as_bytes()) {
        return unsafe { fail(error, UNSUPPORTED, "Algorithm not supported by the key.") };
    }
    let Some(message) = (unsafe { corefoundation::collections::data(data) }) else {
        return unsafe { fail(error, PARAM, "No data to sign.") };
    };
    let Some(pair) = pair(pkcs8) else {
        return unsafe { fail(error, PARAM, "The key is damaged.") };
    };
    match pair.sign(&SystemRandom::new(), message) {
        Ok(signature) => corefoundation::collections::new_data(signature.as_ref().to_vec()),
        Err(_) => unsafe { fail(error, PARAM, "Signing failed.") },
    }
}

/// `Boolean SecKeyIsAlgorithmSupported(SecKeyRef, SecKeyOperationType,
/// SecKeyAlgorithm)`.
///
/// # Safety
///
/// `key` and `algorithm` are null or live objects.
pub unsafe extern "C" fn SecKeyIsAlgorithmSupported(
    key: CFTypeRef,
    operation: i64,
    algorithm: CFTypeRef,
) -> u8 {
    let private = matches!(unsafe { payload(key) }, Some(Payload::Private { .. }));
    let known = unsafe { bytes_of(algorithm) }.as_deref() == Some(SIGN_MESSAGE_SHA256.as_bytes());
    u8::from(private && known && operation == OPERATION_SIGN)
}

/// The attributes a key reports, as `(key, value)` strings and data, each
/// owned by the caller.
fn attributes(payload: &Payload) -> Vec<(CFTypeRef, CFTypeRef)> {
    let string = |s: &str| foundation::string::from_str(s) as CFTypeRef;
    let (class, point, tag) = match payload {
        Payload::Private { pkcs8, tag } => (
            "1",
            pair(pkcs8).map(|p| p.public_key().as_ref().to_vec()),
            tag.clone(),
        ),
        Payload::Public(point) => ("0", Some(point.clone()), None),
        Payload::AccessControl => return Vec::new(),
    };
    let mut pairs = vec![
        (string("type"), string("73")),
        (string("kcls"), string(class)),
        (
            string("bsiz"),
            corefoundation::collections::new_number(corefoundation::collections::Number::Integer(256)),
        ),
    ];
    if let Some(point) = point {
        // The label is the SHA-1 of the public key, as Apple derives it.
        let label = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &point);
        pairs.push((
            string("klbl"),
            corefoundation::collections::new_data(label.as_ref().to_vec()),
        ));
    }
    if let Some(tag) = tag {
        pairs.push((string("atag"), corefoundation::collections::new_data(tag)));
    }
    pairs
}

/// A dictionary of `pairs`, which it takes ownership of.
pub(crate) fn dictionary(pairs: Vec<(CFTypeRef, CFTypeRef)>) -> CFTypeRef {
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (key, value) in pairs {
        // SAFETY: the dictionary took its own references.
        unsafe {
            corefoundation::api::CFRelease(key);
            corefoundation::api::CFRelease(value);
        }
    }
    dictionary
}

/// A key's attributes, for `kSecReturnAttributes` on a key item.
///
/// # Safety
///
/// `key` is null or a live object.
pub(crate) unsafe fn copy_attributes(key: CFTypeRef) -> CFTypeRef {
    match unsafe { payload(key) } {
        Some(payload) => dictionary(attributes(payload)),
        None => std::ptr::null(),
    }
}

/// `CFDictionaryRef SecKeyCopyAttributes(SecKeyRef)`.
///
/// # Safety
///
/// `key` is null or a live object.
pub unsafe extern "C" fn SecKeyCopyAttributes(key: CFTypeRef) -> CFTypeRef {
    unsafe { copy_attributes(key) }
}

exports::exports! {
    functions {
        "SecAccessControlCreateWithFlags" => SecAccessControlCreateWithFlags,
        "SecKeyCreateRandomKey" => SecKeyCreateRandomKey,
        "SecKeyCopyPublicKey" => SecKeyCopyPublicKey,
        "SecKeyCopyExternalRepresentation" => SecKeyCopyExternalRepresentation,
        "SecKeyCreateSignature" => SecKeyCreateSignature,
        "SecKeyIsAlgorithmSupported" => SecKeyIsAlgorithmSupported,
        "SecKeyCopyAttributes" => SecKeyCopyAttributes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_exports_its_point_and_scalar() {
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
                .unwrap();
        let payload = Payload::Private {
            pkcs8: document.as_ref().to_vec(),
            tag: None,
        };
        let bytes = external(&payload).unwrap();
        assert_eq!(bytes.len(), 97);
        assert_eq!(bytes[0], 0x04);
    }
}
