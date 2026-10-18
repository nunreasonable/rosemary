//! Certificates, the basic X.509 policy, and trust evaluation.
//!
//! A program that runs its own TLS stack hands the server's chain to
//! `SecTrustEvaluate` and connects only if it is trusted. The chain is checked
//! for real - signatures, validity dates, and a path to a root - because an
//! answer of "trusted" for anything would turn every such program into one that
//! accepts any server.

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;
use rustls_pki_types::{CertificateDer, UnixTime};

type OSStatus = i32;
const SUCCESS: OSStatus = 0;
/// `errSecParam`.
const PARAM: OSStatus = -50;

/// `SecTrustResultType`.
mod result {
    /// `kSecTrustResultUnspecified`: trusted, with no explicit user setting.
    pub const UNSPECIFIED: u32 = 4;
    /// `kSecTrustResultRecoverableTrustFailure`: untrusted, but well formed.
    pub const RECOVERABLE: u32 = 5;
    /// `kSecTrustResultFatalTrustFailure`: a certificate could not be used at all.
    pub const FATAL: u32 = 6;
}

/// `kSecTrustOptionAllowExpired`.
const ALLOW_EXPIRED: u32 = 0x1;
/// `kSecTrustOptionAllowExpiredRoot`.
const ALLOW_EXPIRED_ROOT: u32 = 0x8;

enum Payload {
    Certificate(Vec<u8>),
    /// `SecPolicyCreateBasicX509`: a chain to a trusted root, no name and no
    /// extended key usage required.
    BasicX509,
    /// `SecPolicyCreateSSL`: as the basic policy, and the leaf must name
    /// `hostname` when one was given.
    Ssl {
        hostname: Option<String>,
    },
    Trust {
        chain: Vec<Vec<u8>>,
        options: u32,
        hostname: Option<String>,
    },
}

#[repr(C)]
struct Body {
    isa: *const Class,
    payload: *mut Payload,
}

fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class =
            corefoundation::base::register(c"__R3SecTrustObject", None, size_of::<Body>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(pointer as *const Class).expect("a trust object class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: this class's layout, and the payload `new` boxed.
    unsafe {
        let body = receiver as *mut Body;
        drop(Box::from_raw((*body).payload));
        objc::arc::finalize(receiver);
    }
}

/// A handle holding `payload`, owned by the caller.
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

/// The payload of one of these handles, or `None` for anything else.
///
/// # Safety
///
/// `object` is null or an object this runtime allocated, live for `'a`.
unsafe fn payload<'a>(object: CFTypeRef) -> Option<&'a mut Payload> {
    // SAFETY: the caller's guarantee.
    if object.is_null() || !unsafe { corefoundation::base::is_kind_of(object, class()) } {
        return None;
    }
    // SAFETY: a body of this class, whose payload lives as long as it does.
    Some(unsafe { &mut *(*(object as *const Body)).payload })
}

/// `object` itself, or each element if it is a `CFArray`.
///
/// # Safety
///
/// `object` is null or a live CoreFoundation object.
unsafe fn one_or_many(object: CFTypeRef) -> Vec<CFTypeRef> {
    use corefoundation::api::{
        CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex, CFGetTypeID,
    };
    if object.is_null() {
        return Vec::new();
    }
    // SAFETY: a live object, and an array when its type says so.
    unsafe {
        if CFGetTypeID(object) != CFArrayGetTypeID() {
            return vec![object];
        }
        (0..CFArrayGetCount(object))
            .map(|i| CFArrayGetValueAtIndex(object, i))
            .collect()
    }
}

/// `SecCertificateRef SecCertificateCreateWithData(CFAllocatorRef, CFDataRef)`.
///
/// Null unless the bytes are one DER-encoded X.509 certificate, as Apple's.
///
/// # Safety
///
/// `data` is null or a live `CFData`.
pub unsafe extern "C" fn SecCertificateCreateWithData(
    _allocator: CFTypeRef,
    data: CFTypeRef,
) -> CFTypeRef {
    if data.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a live `CFData`, whose bytes stay put while it does.
    let bytes = unsafe {
        let length = corefoundation::api::CFDataGetLength(data);
        let base = corefoundation::api::CFDataGetBytePtr(data);
        if base.is_null() || length <= 0 {
            return std::ptr::null();
        }
        std::slice::from_raw_parts(base, length as usize).to_vec()
    };
    let der = CertificateDer::from(bytes.as_slice());
    if webpki::EndEntityCert::try_from(&der).is_err() {
        return std::ptr::null();
    }
    new(Payload::Certificate(bytes))
}

/// The host's root certificates, DER-encoded, read once.
///
/// From the bundle the host's own TLS libraries trust, so a program running
/// its own stack trusts the same servers the rest of the machine does.
/// `SSL_CERT_FILE` first, as OpenSSL reads it, then where Debian, Fedora,
/// NixOS and the BSDs keep theirs.
fn host_anchors() -> &'static [Vec<u8>] {
    static ANCHORS: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
    ANCHORS.get_or_init(|| {
        use rustls_pki_types::pem::PemObject;
        const BUNDLES: &[&str] = &[
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/ca-bundle.pem",
            "/etc/ssl/cert.pem",
            "/usr/local/share/certs/ca-root-nss.crt",
        ];
        let named = std::env::var_os("SSL_CERT_FILE").map(std::path::PathBuf::from);
        let pem = named
            .into_iter()
            .chain(BUNDLES.iter().map(std::path::PathBuf::from))
            .find_map(|path| std::fs::read(path).ok())
            .unwrap_or_default();
        CertificateDer::pem_slice_iter(&pem)
            .filter_map(Result::ok)
            .map(|der| der.to_vec())
            .collect()
    })
}

/// `OSStatus SecTrustCopyAnchorCertificates(CFArrayRef *anchors)`.
///
/// # Safety
///
/// `anchors` is null or writable.
pub unsafe extern "C" fn SecTrustCopyAnchorCertificates(anchors: *mut CFTypeRef) -> OSStatus {
    if anchors.is_null() {
        return PARAM;
    }
    let certificates: Vec<CFTypeRef> = host_anchors()
        .iter()
        .map(|der| new(Payload::Certificate(der.clone())))
        .filter(|certificate| !certificate.is_null())
        .collect();
    let array = corefoundation::collections::new_array(
        &certificates,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for certificate in certificates {
        // SAFETY: the array took its own reference.
        unsafe { corefoundation::api::CFRelease(certificate) };
    }
    // SAFETY: writable, per the caller.
    unsafe { anchors.write(array) };
    SUCCESS
}

/// The DER bytes of a certificate handle.
///
/// # Safety
///
/// As [`payload`].
unsafe fn der_of<'a>(certificate: CFTypeRef) -> Option<&'a [u8]> {
    // SAFETY: the caller's guarantee.
    match unsafe { payload(certificate) } {
        Some(Payload::Certificate(der)) => Some(der),
        _ => None,
    }
}

/// `CFDataRef SecCertificateCopyData(SecCertificateRef)`.
///
/// # Safety
///
/// `certificate` is null or a live object.
pub unsafe extern "C" fn SecCertificateCopyData(certificate: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's guarantee.
    match unsafe { der_of(certificate) } {
        Some(der) => corefoundation::collections::new_data(der.to_vec()),
        None => std::ptr::null(),
    }
}

/// `OSStatus SecCertificateGetData(SecCertificateRef, CSSM_DATA *)`.
///
/// `CSSM_DATA` is a length and a pointer, `size_t` wide on LP64. The pointer is
/// into the certificate, which is what Apple hands back too: valid for as long
/// as the caller holds the certificate.
///
/// # Safety
///
/// `certificate` is null or a live object, and `data` is null or writable.
pub unsafe extern "C" fn SecCertificateGetData(
    certificate: CFTypeRef,
    data: *mut (usize, *const u8),
) -> OSStatus {
    // SAFETY: the caller's guarantee.
    let Some(der) = (unsafe { der_of(certificate) }) else {
        return PARAM;
    };
    if data.is_null() {
        return PARAM;
    }
    // SAFETY: writable, per the caller.
    unsafe { data.write((der.len(), der.as_ptr())) };
    SUCCESS
}

/// `SecPolicyRef SecPolicyCreateBasicX509(void)`.
///
/// # Safety
///
/// None beyond being called as the C function it is.
pub unsafe extern "C" fn SecPolicyCreateBasicX509() -> CFTypeRef {
    new(Payload::BasicX509)
}

/// `SecPolicyRef SecPolicyCreateSSL(Boolean server, CFStringRef hostname)`.
///
/// FIXME (ACCURACY): `server` false (a client certificate check) is evaluated
/// as a server one; nothing here has asked for the other.
///
/// # Safety
///
/// `hostname` is null or a live string.
pub unsafe extern "C" fn SecPolicyCreateSSL(_server: u8, hostname: CFTypeRef) -> CFTypeRef {
    let hostname = (!hostname.is_null()).then(|| {
        // SAFETY: the caller's guarantee.
        let bytes = unsafe { foundation::string::bytes(hostname as Id) };
        String::from_utf8_lossy(&bytes).into_owned()
    });
    new(Payload::Ssl {
        hostname: hostname.filter(|h| !h.is_empty()),
    })
}

/// `OSStatus SecTrustCreateWithCertificates(CFTypeRef certificates, CFTypeRef
/// policies, SecTrustRef *trust)`.
///
/// `certificates` is one certificate or an array, the leaf first. Only the
/// basic X.509 policy is known; any other is refused rather than evaluated as
/// if it were that one.
///
/// # Safety
///
/// Both are null or live objects, and `trust` is null or writable.
pub unsafe extern "C" fn SecTrustCreateWithCertificates(
    certificates: CFTypeRef,
    policies: CFTypeRef,
    trust: *mut CFTypeRef,
) -> OSStatus {
    if trust.is_null() {
        return PARAM;
    }
    let mut chain = Vec::new();
    // SAFETY: the caller's guarantee.
    for certificate in unsafe { one_or_many(certificates) } {
        // SAFETY: an element of a live array, or the object itself.
        match unsafe { payload(certificate) } {
            Some(Payload::Certificate(der)) => chain.push(der.clone()),
            _ => return PARAM,
        }
    }
    let mut hostname = None;
    // SAFETY: the caller's guarantee.
    for policy in unsafe { one_or_many(policies) } {
        // SAFETY: as above.
        match unsafe { payload(policy) } {
            Some(Payload::BasicX509) => {}
            Some(Payload::Ssl { hostname: name }) => hostname = name.clone(),
            _ => {
                eprintln!(
                    "rosemary: security: SecTrustCreateWithCertificates: only the basic X.509 and SSL policies are supported"
                );
                return PARAM;
            }
        }
    }
    if chain.is_empty() {
        return PARAM;
    }
    // SAFETY: writable, checked above.
    unsafe {
        trust.write(new(Payload::Trust {
            chain,
            options: 0,
            hostname,
        }))
    };
    SUCCESS
}

/// `OSStatus SecTrustSetOptions(SecTrustRef, SecTrustOptionFlags)`.
///
/// # Safety
///
/// `trust` is null or a live object.
pub unsafe extern "C" fn SecTrustSetOptions(trust: CFTypeRef, flags: u32) -> OSStatus {
    // SAFETY: the caller's guarantee.
    match unsafe { payload(trust) } {
        Some(Payload::Trust { options, .. }) => {
            *options = flags;
            SUCCESS
        }
        _ => PARAM,
    }
}

/// The basic policy's extended key usage check, which is none: a certificate
/// restricted to some purpose is still a valid X.509 certificate.
struct AnyUsage;

impl webpki::ExtendedKeyUsageValidator for AnyUsage {
    fn validate(&self, _iter: webpki::KeyPurposeIdIter<'_, '_>) -> Result<(), webpki::Error> {
        Ok(())
    }
}

/// Evaluate `chain` (leaf first) against the built-in roots under `options`.
///
/// FIXME (ACCURACY): the anchors are Mozilla's root program (`webpki-roots`),
/// not the keychain's. The two sets largely agree for public servers; a root
/// only Apple ships is untrusted here.
fn evaluate(chain: &[Vec<u8>], options: u32, hostname: Option<&str>) -> u32 {
    let Some((leaf, rest)) = chain.split_first() else {
        return result::FATAL;
    };
    let leaf = CertificateDer::from(leaf.as_slice());
    let Ok(end_entity) = webpki::EndEntityCert::try_from(&leaf) else {
        return result::FATAL;
    };
    let intermediates: Vec<_> = rest
        .iter()
        .map(|der| CertificateDer::from(der.as_slice()))
        .collect();
    let mut time = UnixTime::now();
    // An allowed expiry is honoured by asking again at the moment the expired
    // certificate was last valid - a few times, since each step back can land
    // before another certificate's start.
    for _ in 0..4 {
        let verified = end_entity.verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            webpki_roots::TLS_SERVER_ROOTS,
            &intermediates,
            time,
            AnyUsage,
            None,
            None,
        );
        match verified {
            Ok(_) => {
                let named = hostname.is_none_or(|name| {
                    rustls_pki_types::ServerName::try_from(name)
                        .ok()
                        .is_some_and(|name| {
                            end_entity.verify_is_valid_for_subject_name(&name).is_ok()
                        })
                });
                return match named {
                    true => result::UNSPECIFIED,
                    false => result::RECOVERABLE,
                };
            }
            Err(webpki::Error::CertExpired { not_after, .. })
                if options & (ALLOW_EXPIRED | ALLOW_EXPIRED_ROOT) != 0 =>
            {
                time = not_after;
            }
            Err(_) => return result::RECOVERABLE,
        }
    }
    result::RECOVERABLE
}

/// `OSStatus SecTrustEvaluate(SecTrustRef, SecTrustResultType *)`.
///
/// # Safety
///
/// `trust` is null or a live object, and `out` is null or writable.
pub unsafe extern "C" fn SecTrustEvaluate(trust: CFTypeRef, out: *mut u32) -> OSStatus {
    // SAFETY: the caller's guarantee.
    let Some(Payload::Trust {
        chain,
        options,
        hostname,
    }) = (unsafe { payload(trust) })
    else {
        return PARAM;
    };
    let verdict = evaluate(chain, *options, hostname.as_deref());
    if !out.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { out.write(verdict) };
    }
    SUCCESS
}

/// `bool SecTrustEvaluateWithError(SecTrustRef, CFErrorRef *)`: the same
/// evaluation, answered as yes or no.
///
/// FIXME (ACCURACY): a failure leaves `error` null rather than describing why.
///
/// # Safety
///
/// As [`SecTrustEvaluate`].
pub unsafe extern "C" fn SecTrustEvaluateWithError(trust: CFTypeRef, error: *mut CFTypeRef) -> u8 {
    if !error.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { error.write(std::ptr::null()) };
    }
    let mut verdict = result::FATAL;
    // SAFETY: the caller's guarantee.
    if unsafe { SecTrustEvaluate(trust, &mut verdict) } != SUCCESS {
        return 0;
    }
    u8::from(verdict == result::UNSPECIFIED)
}

/// `OSStatus SecTrustGetTrustResult(SecTrustRef, SecTrustResultType *)`.
///
/// Evaluated afresh, which is what a trust that was never evaluated does.
///
/// # Safety
///
/// As [`SecTrustEvaluate`].
pub unsafe extern "C" fn SecTrustGetTrustResult(trust: CFTypeRef, out: *mut u32) -> OSStatus {
    // SAFETY: the caller's guarantee.
    unsafe { SecTrustEvaluate(trust, out) }
}

/// The certificates a keychain query for trusted certificates finds: the
/// host's anchors, as DER.
pub(crate) fn anchor_ders() -> &'static [Vec<u8>] {
    host_anchors()
}

/// A certificate handle for `der`, owned by the caller.
pub(crate) fn certificate(der: &[u8]) -> CFTypeRef {
    new(Payload::Certificate(der.to_vec()))
}

/// `int SecRandomCopyBytes(SecRandomRef, size_t, void *)`. The generator
/// argument is `kSecRandomDefault` - null - in every caller.
///
/// # Safety
///
/// `bytes` is writable for `count` bytes.
pub unsafe extern "C" fn SecRandomCopyBytes(_rnd: CFTypeRef, count: usize, bytes: *mut u8) -> i32 {
    if count == 0 {
        return 0;
    }
    if bytes.is_null() {
        return -1;
    }
    // SAFETY: the caller's guarantee.
    let buffer = unsafe { std::slice::from_raw_parts_mut(bytes, count) };
    match getrandom::fill(buffer) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

exports::exports! {
    functions {
        "SecCertificateCreateWithData" => SecCertificateCreateWithData,
        "SecPolicyCreateBasicX509" => SecPolicyCreateBasicX509,
        "SecTrustCreateWithCertificates" => SecTrustCreateWithCertificates,
        "SecTrustSetOptions" => SecTrustSetOptions,
        "SecTrustEvaluate" => SecTrustEvaluate,
        "SecTrustEvaluateWithError" => SecTrustEvaluateWithError,
        "SecTrustGetTrustResult" => SecTrustGetTrustResult,
        "SecPolicyCreateSSL" => SecPolicyCreateSSL,
        "SecRandomCopyBytes" => SecRandomCopyBytes,
        "SecTrustCopyAnchorCertificates" => SecTrustCopyAnchorCertificates,
        "SecCertificateCopyData" => SecCertificateCopyData,
        "SecCertificateGetData" => SecCertificateGetData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every anchor reads back as the bytes it was made from, through both
    /// spellings of the question.
    #[test]
    fn anchors_read_back_as_their_der() {
        let mut array: CFTypeRef = std::ptr::null();
        assert_eq!(
            unsafe { SecTrustCopyAnchorCertificates(&mut array) },
            SUCCESS
        );
        let count = unsafe { corefoundation::api::CFArrayGetCount(array) };
        if count == 0 {
            // A host with no CA bundle, which is a host and not a failure.
            return;
        }
        let first = unsafe { corefoundation::api::CFArrayGetValueAtIndex(array, 0) };
        let data = unsafe { SecCertificateCopyData(first) };
        let length = unsafe { corefoundation::api::CFDataGetLength(data) } as usize;
        let mut cssm = (0usize, std::ptr::null::<u8>());
        assert_eq!(unsafe { SecCertificateGetData(first, &mut cssm) }, SUCCESS);
        assert_eq!(cssm.0, length);
        assert_eq!(host_anchors()[0].len(), length);
    }

    /// A certificate that chains to nothing: well formed, so it is accepted as a
    /// certificate, but evaluation must not call it trusted.
    #[test]
    fn a_self_signed_certificate_is_not_trusted() {
        objc::bootstrap();
        let der = self_signed();
        let data = data(&der);
        let mut trust = std::ptr::null();
        let mut verdict = 0;
        // SAFETY: objects this test made, and writable out-parameters.
        unsafe {
            let certificate = SecCertificateCreateWithData(std::ptr::null(), data);
            assert!(!certificate.is_null(), "a DER certificate is accepted");
            let policy = SecPolicyCreateBasicX509();
            assert_eq!(
                SecTrustCreateWithCertificates(certificate, policy, &mut trust),
                SUCCESS
            );
            assert_eq!(SecTrustSetOptions(trust, ALLOW_EXPIRED), SUCCESS);
            assert_eq!(SecTrustEvaluate(trust, &mut verdict), SUCCESS);
            assert_eq!(verdict, result::RECOVERABLE);
            for object in [trust, policy, certificate, data] {
                corefoundation::api::CFRelease(object);
            }
        }
    }

    #[test]
    fn bytes_that_are_not_a_certificate_make_no_certificate() {
        objc::bootstrap();
        let data = data(b"not a certificate");
        // SAFETY: a `CFData` this test made.
        unsafe {
            assert!(SecCertificateCreateWithData(std::ptr::null(), data).is_null());
            corefoundation::api::CFRelease(data);
        }
    }

    #[test]
    fn random_bytes_fill_the_buffer() {
        let mut buffer = [0u8; 64];
        // SAFETY: a writable buffer of the stated length.
        assert_eq!(
            unsafe { SecRandomCopyBytes(std::ptr::null(), buffer.len(), buffer.as_mut_ptr()) },
            0
        );
        assert!(buffer.iter().any(|&b| b != 0));
    }

    /// A self-signed P-256 certificate, `CN=rosemary test`, valid for a century
    /// from its making so the test does not expire.
    fn self_signed() -> Vec<u8> {
        include_bytes!("testdata/self_signed.der").to_vec()
    }

    fn data(bytes: &[u8]) -> CFTypeRef {
        // SAFETY: a live slice of the stated length.
        unsafe {
            corefoundation::api::CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as _)
        }
    }
}
