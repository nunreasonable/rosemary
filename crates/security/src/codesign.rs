#![allow(non_snake_case)]

use crate::code_directory;
use std::ffi::c_void;

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;

type OSStatus = i32;
const SUCCESS: OSStatus = 0;
/// `errSecCSInvalidObjectRef`: a handle that is not one of these.
const INVALID_OBJECT_REF: OSStatus = -67071;
/// `errSecParam`.
const PARAM: OSStatus = -50;

#[repr(C)]
struct Body {
    isa: *const Class,
    description: CFTypeRef,
    code: bool,
    cpu: u32,
    requirement: *mut super::code_requirement::Expr,
}

fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class =
            corefoundation::base::register(c"__R3SecCodeObject", None, size_of::<Body>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(pointer as *const Class).expect("a code object class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: this class's object layout and its owned description.
    unsafe {
        let body = receiver as *mut Body;
        objc::arc::release((*body).description as Id);
        if !(*body).requirement.is_null() {
            drop(Box::from_raw((*body).requirement));
        }
        objc::arc::finalize(receiver);
    }
}

/// A handle carrying `description`, owned by the caller.
fn new(description: CFTypeRef, code: bool) -> CFTypeRef {
    // SAFETY: `Body` is the registered layout.
    let body = unsafe { corefoundation::base::allocate::<Body>(class()) };
    if body.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a freshly allocated body.
    unsafe {
        (*body).description = description;
        (*body).code = code;
        (*body).requirement = std::ptr::null_mut();
        (*body).cpu = darwin::sysctl::guest_arch().cpu_type() as u32;
    };
    body as CFTypeRef
}

/// # Safety
///
/// `object` is null or an object this runtime allocated.
unsafe fn is_ours(object: CFTypeRef) -> bool {
    unsafe { corefoundation::base::is_kind_of(object, class()) }
}

/// `OSStatus SecRequirementCreateWithString(CFStringRef text, SecCSFlags flags,
/// SecRequirementRef *requirement)`
///
/// Supported expressions are parsed before publishing a requirement handle.
///
/// # Safety
///
/// `requirement` is null or a writable `SecRequirementRef`.
pub unsafe extern "C" fn SecRequirementCreateWithString(
    text: CFTypeRef,
    flags: u32,
    requirement: *mut CFTypeRef,
) -> OSStatus {
    if flags != 0 {
        return -67070;
    }
    if requirement.is_null() {
        return PARAM;
    }
    // SAFETY: a string or null.
    if unsafe { corefoundation::string::as_string(text) }.is_none() {
        return PARAM;
    }
    let string = unsafe { corefoundation::string::as_string(text) }
        .unwrap()
        .utf8();
    let string = match std::str::from_utf8(&string) {
        Ok(s) => s,
        Err(_) => return PARAM,
    };
    let parsed = match super::code_requirement::parse(string) {
        Ok(expr) => expr,
        Err(error) => return error,
    };
    let held = objc::arc::retain(text as Id) as CFTypeRef;
    let object = new(held, false);
    unsafe {
        (*(object as *mut Body)).requirement = Box::into_raw(Box::new(parsed));
        *requirement = object;
    }
    SUCCESS
}

/// `OSStatus SecRequirementCreateWithData(CFDataRef data, SecCSFlags flags,
/// SecRequirementRef *requirement)`
///
/// Decode the supported expression opcodes and reject unsupported requirements.
///
/// # Safety
///
/// As [`SecRequirementCreateWithString`].
pub unsafe extern "C" fn SecRequirementCreateWithData(
    data: CFTypeRef,
    flags: u32,
    requirement: *mut CFTypeRef,
) -> OSStatus {
    if requirement.is_null() {
        return PARAM;
    }
    if flags != 0 {
        return -67070;
    }
    let Some(bytes) = (unsafe { corefoundation::collections::data(data) }) else {
        return PARAM;
    };
    let parsed = match super::code_requirement::binary(bytes) {
        Ok(expr) => expr,
        Err(error) => return error,
    };
    let object = new(objc::arc::retain(data as Id) as CFTypeRef, false);
    unsafe {
        (*(object as *mut Body)).requirement = Box::into_raw(Box::new(parsed));
        *requirement = object;
    }
    SUCCESS
}

/// `OSStatus SecCodeCopySelf(SecCSFlags flags, SecCodeRef *self)`
///
/// # Safety
///
/// `out` is null or a writable `SecCodeRef`.
pub unsafe extern "C" fn SecCodeCopySelf(_flags: u32, out: *mut CFTypeRef) -> OSStatus {
    if out.is_null() {
        return PARAM;
    }
    let path = foundation::bundle::main_executable()
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let description = corefoundation::string::from_bytes(path.as_bytes(), false);
    // SAFETY: the caller states it is writable.
    unsafe { *out = new(description, true) };
    SUCCESS
}

/// `OSStatus SecCodeCheckValidity(SecCodeRef code, SecCSFlags flags,
/// SecRequirementRef requirement)`
///
/// Dynamic process validation requires kernel signing state, not just file hashes.
///
/// # Safety
///
/// Both arguments are null or objects this runtime allocated.
pub unsafe extern "C" fn SecCodeCheckValidity(
    code: CFTypeRef,
    _flags: u32,
    requirement: CFTypeRef,
) -> OSStatus {
    if !unsafe { is_ours(code) } {
        return INVALID_OBJECT_REF;
    }
    // SAFETY: null means that no additional requirement was supplied.
    if !requirement.is_null() && !unsafe { is_ours(requirement) } {
        return INVALID_OBJECT_REF;
    }
    if !unsafe { (*(code as *const Body)).code } {
        return INVALID_OBJECT_REF;
    }
    -67072
}

/// # Safety
/// `url` is a CFURL and `out` is writable when nonnull.
pub unsafe extern "C" fn SecStaticCodeCreateWithPath(
    url: CFTypeRef,
    flags: u32,
    out: *mut CFTypeRef,
) -> OSStatus {
    if out.is_null() {
        return PARAM;
    }
    if flags != 0 {
        return -67070;
    }
    let Some(path) = (unsafe { corefoundation::url::as_url(url) }).and_then(|u| u.file_path())
    else {
        return PARAM;
    };
    let mut host = prefix::ns::host_path(&path);
    if host.is_dir() {
        let Some(layout) = foundation::bundle::Layout::at(&host) else {
            return -67028;
        };
        host = layout.executable;
    }
    if !host.is_file() {
        return -67068;
    }
    let description = corefoundation::string::from_bytes(host.to_string_lossy().as_bytes(), false);
    unsafe {
        *out = new(description, true);
    }
    SUCCESS
}

/// # Safety
/// As SecStaticCodeCreateWithPath; attributes is null or a CFDictionary.
pub unsafe extern "C" fn SecStaticCodeCreateWithPathAndAttributes(
    url: CFTypeRef,
    flags: u32,
    attributes: CFTypeRef,
    out: *mut CFTypeRef,
) -> OSStatus {
    let mut cpu = None;
    let mut offset = None;
    if !attributes.is_null() {
        let Some(dict) = (unsafe { corefoundation::collections::dictionary(attributes) }) else {
            return PARAM;
        };
        for &(key, value) in &dict.entries {
            let Some(key) = (unsafe { corefoundation::string::as_string(key) }) else {
                return PARAM;
            };
            if key.utf8() == b"UniversalFileOffset" {
                offset = unsafe { corefoundation::collections::number(value) }
                    .and_then(|n| usize::try_from(n.as_i64()).ok());
                if offset.is_none() {
                    return PARAM;
                }
                continue;
            }
            if key.utf8() != b"architecture" {
                return -67067;
            }
            cpu = if let Some(number) = unsafe { corefoundation::collections::number(value) } {
                u32::try_from(number.as_i64()).ok()
            } else {
                match unsafe { corefoundation::string::as_string(value) }
                    .map(|s| s.utf8())
                    .as_deref()
                {
                    Some(b"x86_64") => Some(0x1000007),
                    Some(b"arm64") => Some(0x100000c),
                    _ => None,
                }
            };
            if cpu.is_none() {
                return -67066;
            }
        }
    }
    let mut object = std::ptr::null();
    if out.is_null() {
        return PARAM;
    }
    let result = unsafe { SecStaticCodeCreateWithPath(url, flags, &mut object) };
    if result != SUCCESS {
        return result;
    }
    if let Some(offset) = offset {
        let selected = unsafe { image(object) }
            .and_then(|bytes| code_directory::cpu_at_offset(&bytes, offset));
        match selected {
            Ok(selected) if cpu.is_none_or(|cpu| cpu == selected) => cpu = Some(selected),
            Ok(_) => {
                unsafe { corefoundation::api::CFRelease(object) };
                return -67066;
            }
            Err(error) => {
                unsafe { corefoundation::api::CFRelease(object) };
                return error;
            }
        }
    }
    if let Some(cpu) = cpu {
        unsafe {
            (*(object as *mut Body)).cpu = cpu;
        }
    }
    unsafe {
        *out = object;
    }
    SUCCESS
}

unsafe fn image(code: CFTypeRef) -> Result<Vec<u8>, OSStatus> {
    if !unsafe { is_ours(code) } || !unsafe { (*(code as *const Body)).code } {
        return Err(INVALID_OBJECT_REF);
    }
    let text = unsafe { corefoundation::string::as_string((*(code as *const Body)).description) }
        .ok_or(INVALID_OBJECT_REF)?;
    let path = String::from_utf8(text.utf8()).map_err(|_| PARAM)?;
    read_file(std::path::Path::new(&path))
}

fn read_file(path: &std::path::Path) -> Result<Vec<u8>, OSStatus> {
    let file = std::fs::File::open(path).map_err(|_| -67060)?;
    // Bound temporary storage even if an untrusted file grows during the read.
    use std::io::Read;
    const LIMIT: u64 = 256 * 1024 * 1024;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| -67060)?;
    if bytes.len() as u64 > LIMIT {
        return Err(-67004);
    }
    Ok(bytes)
}

fn validate_path(
    path: &std::path::Path,
    cpu: u32,
    requirement: Option<&super::code_requirement::Expr>,
    depth: usize,
) -> Result<Vec<u8>, i32> {
    if depth > 16 {
        return Err(-67004);
    }
    let executable = if path.is_dir() {
        foundation::bundle::Layout::at(path)
            .ok_or(-67028)?
            .executable
    } else {
        path.to_path_buf()
    };
    let path = executable.as_path();
    let data = read_file(path)?;
    let cpu = if depth > 0 && data.get(..4) == Some(&0xfeedfacfu32.to_le_bytes()) {
        u32::from_le_bytes(data.get(4..8).ok_or(-67045)?.try_into().unwrap())
    } else {
        cpu
    };
    let external;
    let (image, offset, parts) = if data.get(..4).is_some_and(|magic| {
        [
            0xfeedfacfu32.to_le_bytes(),
            0xcafebabeu32.to_be_bytes(),
            0xcafebabfu32.to_be_bytes(),
        ]
        .iter()
        .any(|known| magic == known)
    }) {
        let image = code_directory::architecture(&data, cpu)?;
        let (offset, signature) = code_directory::signature(image)?;
        (image, offset, code_directory::components(signature)?)
    } else {
        external = super::code_xattr::components(path)?;
        (
            data.as_slice(),
            data.len(),
            external
                .iter()
                .map(|(slot, bytes)| (*slot, bytes.as_slice()))
                .collect(),
        )
    };
    let directory =
        code_directory::Directory::parse(parts.get(&0).ok_or(code_directory::INVALID)?)?;
    directory.verify_pages(image, offset)?;
    let layout =
        foundation::bundle::Layout::enclosing(path).filter(|layout| layout.executable == path);
    let info = layout
        .as_ref()
        .map(|l| read_file(&l.contents.join("Info.plist")).map_err(|_| -67030))
        .transpose()?;
    let resources = layout
        .as_ref()
        .and_then(|l| {
            let path = l.contents.join("_CodeSignature/CodeResources");
            path.exists().then(|| read_file(&path).map_err(|_| -67023))
        })
        .transpose()?;
    let special_limit = parts
        .keys()
        .copied()
        .filter(|slot| *slot < 0x1000)
        .max()
        .unwrap_or(0)
        .max(if layout.is_some() { 3 } else { 0 });
    if special_limit > 11 {
        return Err(code_directory::UNSUPPORTED);
    }
    for slot in 1..=directory.special_slots().max(special_limit) {
        let content = match slot {
            1 => info.as_deref(),
            3 => resources.as_deref().or_else(|| parts.get(&3).copied()),
            _ => parts.get(&slot).copied(),
        };
        directory.verify_special(slot, content)?;
    }
    let mut certificates = Vec::new();
    let mut apple_trusted = false;
    let mut directory_hashes = Vec::new();
    if directory.flags & 2 == 0 {
        let cms = parts.get(&0x10000).ok_or(code_directory::INVALID)?;
        if cms.get(..4) != Some(&0xfade0b01u32.to_be_bytes()) {
            return Err(code_directory::INVALID);
        }
        let signer = super::code_cms::verify(&cms[8..], directory.bytes)?;
        signer.verify_apple_chain()?;
        apple_trusted = true;
        certificates = signer.certificates;
        directory_hashes = signer.directory_hashes;
    } else if parts.contains_key(&0x10000) {
        return Err(code_directory::INVALID);
    }
    if let Some(expr) = requirement {
        if !expr.matches(directory.identifier, apple_trusted, &certificates)? {
            return Err(-67050);
        }
    }
    for (&slot, bytes) in &parts {
        if (0x1000..0x1005).contains(&slot) {
            let alternate = code_directory::Directory::parse(bytes)?;
            if !alternate.authenticated_by(&directory_hashes) {
                return Err(code_directory::MODIFIED);
            }
            alternate.verify_pages(image, offset)?;
            for slot in 1..=alternate.special_slots().max(special_limit) {
                let content = match slot {
                    1 => info.as_deref(),
                    3 => resources.as_deref().or_else(|| parts.get(&3).copied()),
                    _ => parts.get(&slot).copied(),
                };
                alternate.verify_special(slot, content)?;
            }
        }
    }

    let hash = directory.cdhash();
    drop(data);
    if let Some(layout) = layout {
        let resources = resources.as_deref().ok_or(-67057)?;
        super::code_resources::verify(
            &layout.contents,
            &layout.executable,
            resources,
            &mut |child, requirement, expected_hash| {
                let expr = requirement
                    .map(super::code_requirement::parse)
                    .transpose()?;
                let hash =
                    validate_path(child, cpu, expr.as_ref(), depth + 1).inspect_err(|error| {
                        #[cfg(test)]
                        eprintln!(
                            "nested verification failed for {:?}: {error}",
                            child.file_name()
                        );
                        #[cfg(not(test))]
                        let _ = error;
                    })?;
                if expr.is_none() && expected_hash.is_none_or(|expected| expected != hash) {
                    return Err(-67021);
                }
                Ok(())
            },
        )?;
    }
    Ok(hash)
}

/// # Safety
/// Code and requirement are null or runtime objects.
pub unsafe extern "C" fn SecStaticCodeCheckValidity(
    code: CFTypeRef,
    flags: u32,
    requirement: CFTypeRef,
) -> OSStatus {
    let check = || -> Result<(), OSStatus> {
        if !unsafe { is_ours(code) } || !unsafe { (*(code as *const Body)).code } {
            return Err(INVALID_OBJECT_REF);
        }
        if flags != 0 {
            return Err(-67072);
        }
        if !requirement.is_null() {
            if !unsafe { is_ours(requirement) } || unsafe { (*(requirement as *const Body)).code } {
                return Err(INVALID_OBJECT_REF);
            }
            if unsafe { (*(requirement as *const Body)).requirement.is_null() } {
                return Err(-67051);
            }
        }
        let description =
            unsafe { corefoundation::string::as_string((*(code as *const Body)).description) }
                .ok_or(INVALID_OBJECT_REF)?;
        let path = String::from_utf8(description.utf8()).map_err(|_| PARAM)?;
        let expr = if requirement.is_null() {
            None
        } else {
            Some(unsafe { &*(*(requirement as *const Body)).requirement })
        };
        validate_path(
            std::path::Path::new(&path),
            unsafe { (*(code as *const Body)).cpu },
            expr,
            0,
        )?;
        Ok(())
    };
    check().err().unwrap_or(SUCCESS)
}

/// # Safety
/// As SecStaticCodeCheckValidity; error is null or writable.
pub unsafe extern "C" fn SecStaticCodeCheckValidityWithErrors(
    code: CFTypeRef,
    flags: u32,
    requirement: CFTypeRef,
    error: *mut CFTypeRef,
) -> OSStatus {
    let status = unsafe { SecStaticCodeCheckValidity(code, flags, requirement) };
    if status != SUCCESS && !error.is_null() {
        unsafe {
            *error = foundation::error::create(
                "NSOSStatusErrorDomain",
                status as i64,
                "Code signature validation failed or requires an unsupported feature",
                &[],
            ) as CFTypeRef;
        }
    }
    status
}

/// # Safety
/// Code is a runtime object and out is writable when nonnull.
pub unsafe extern "C" fn SecCodeCopySigningInformation(
    code: CFTypeRef,
    flags: u32,
    out: *mut CFTypeRef,
) -> OSStatus {
    use corefoundation::{callbacks::Policy, collections};
    if out.is_null() {
        return PARAM;
    }
    // Additional groups require certificates, requirements, or dynamic state.
    if flags & !2 != 0 {
        return -67072;
    }
    let image = match unsafe { image(code) } {
        Ok(image) => image,
        Err(error) => return error,
    };
    let image = match code_directory::architecture(&image, unsafe { (*(code as *const Body)).cpu })
    {
        Ok(image) => image,
        Err(error) => return error,
    };
    let directory = match code_directory::signature(image) {
        Ok((_, blob)) => match code_directory::primary(blob) {
            Ok(cd) => Some(cd),
            Err(error) => return error,
        },
        Err(code_directory::UNSIGNED) => None,
        Err(error) => return error,
    };
    let text =
        unsafe { corefoundation::string::as_string((*(code as *const Body)).description) }.unwrap();
    let path = String::from_utf8_lossy(&text.utf8()).into_owned();
    let host_path = std::path::PathBuf::from(path);
    let guest_path = prefix::ns::to_guest(&host_path).unwrap_or(host_path);
    let entitlements = if flags & 2 != 0 {
        match code_directory::signature(image) {
            Ok((_, blob)) => {
                let parts = match code_directory::components(blob) {
                    Ok(parts) => parts,
                    Err(error) => return error,
                };
                match parts.get(&5) {
                    Some(blob) => {
                        if blob.get(..4) != Some(&0xfade7171u32.to_be_bytes()) {
                            return code_directory::INVALID;
                        }
                        match corefoundation::plist::parse(&blob[8..]) {
                            Ok(value @ corefoundation::plist::Value::Dictionary(_)) => Some(value),
                            _ => return code_directory::INVALID,
                        }
                    }
                    None => None,
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let mut pairs = vec![(
        corefoundation::string::from_bytes(b"main-executable", false),
        corefoundation::url::new_file_url(&guest_path.to_string_lossy(), false),
    )];
    if let Some(value) = entitlements {
        if let Some(object) = corefoundation::propertylist::object_from_value(&value) {
            pairs.push((
                corefoundation::string::from_bytes(b"entitlements-dict", false),
                object,
            ));
        }
    }
    if let Some(cd) = directory {
        pairs.push((
            corefoundation::string::from_bytes(b"identifier", false),
            corefoundation::string::from_bytes(cd.identifier.as_bytes(), false),
        ));
        pairs.push((
            corefoundation::string::from_bytes(b"flags", false),
            collections::new_number(collections::Number::Integer(cd.flags as i64)),
        ));
        pairs.push((
            corefoundation::string::from_bytes(b"unique", false),
            collections::new_data(cd.cdhash()),
        ));
    }
    if flags & 2 != 0 {
        if let Ok((_, blob)) = code_directory::signature(image) {
            let parts = code_directory::components(blob).expect("signature was already parsed");
            if let Some(cms) = parts.get(&0x10000) {
                if cms.get(..4) == Some(&0xfade0b01u32.to_be_bytes()) {
                    pairs.push((
                        corefoundation::string::from_bytes(b"cms", false),
                        collections::new_data(cms[8..].to_vec()),
                    ));
                    if let Some(cd) = parts.get(&0) {
                        if let Ok(signer) = super::code_cms::verify(&cms[8..], cd) {
                            let certs: Vec<_> = signer
                                .certificates
                                .iter()
                                .map(|der| super::trust::certificate(der))
                                .collect();
                            let array = collections::new_array(
                                &certs,
                                Policy::from_type_callbacks(),
                                false,
                            );
                            for cert in certs {
                                unsafe {
                                    corefoundation::api::CFRelease(cert);
                                }
                            }
                            pairs.push((
                                corefoundation::string::from_bytes(b"certificates", false),
                                array,
                            ));
                        }
                    }
                }
            } else {
                pairs.push((
                    corefoundation::string::from_bytes(b"cms", false),
                    collections::new_data(Vec::new()),
                ));
            }
        }
    }
    let result = collections::new_dictionary(
        &pairs,
        Policy::from_type_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    for (key, value) in pairs {
        unsafe {
            corefoundation::api::CFRelease(key);
            corefoundation::api::CFRelease(value);
        }
    }
    unsafe {
        *out = result;
    }
    SUCCESS
}

pub(crate) const KEYS: &[(&str, &str)] = &[
    ("_kSecCodeInfoCMS", "cms"),
    ("_kSecCodeInfoEntitlementsDict", "entitlements-dict"),
    (
        "_kSecCodeAttributeUniversalFileOffset",
        "UniversalFileOffset",
    ),
    ("_kSecCodeInfoCertificates", "certificates"),
    ("_kSecCodeAttributeArchitecture", "architecture"),
    ("_kSecCodeInfoMainExecutable", "main-executable"),
    ("_kSecCodeInfoIdentifier", "identifier"),
    ("_kSecCodeInfoFlags", "flags"),
    ("_kSecCodeInfoUnique", "unique"),
];

exports::exports! {
    functions {
        "SecRequirementCreateWithString" => SecRequirementCreateWithString,
        "SecRequirementCreateWithData" => SecRequirementCreateWithData,
        "SecStaticCodeCreateWithPath" => SecStaticCodeCreateWithPath,
        "SecStaticCodeCreateWithPathAndAttributes" => SecStaticCodeCreateWithPathAndAttributes,
        "SecStaticCodeCheckValidity" => SecStaticCodeCheckValidity,
        "SecStaticCodeCheckValidityWithErrors" => SecStaticCodeCheckValidityWithErrors,
        "SecCodeCopySigningInformation" => SecCodeCopySigningInformation,
        "SecCodeCopySelf" => SecCodeCopySelf,
        "SecCodeCheckValidity" => SecCodeCheckValidity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires ROSEMARY_SIGNING_TEST_IMAGE naming an external signed bundle executable"]
    fn verifies_external_bundle() {
        let path = std::env::var_os("ROSEMARY_SIGNING_TEST_IMAGE").expect("fixture path");
        let expr = super::super::code_requirement::parse("anchor apple generic").unwrap();
        validate_path(std::path::Path::new(&path), 0x1000007, Some(&expr), 0)
            .expect("bundle signature");
    }

    #[test]
    fn static_validation_detects_file_changes_and_preserves_error_outputs() {
        foundation::bootstrap();
        let path =
            std::env::temp_dir().join(format!("rosemary-signature-test-{}", std::process::id()));
        let mut bytes = crate::code_directory::tests::signed_image();
        std::fs::write(&path, &bytes).unwrap();
        let description =
            corefoundation::string::from_bytes(path.to_str().unwrap().as_bytes(), false);
        let code = new(description, true);
        unsafe {
            assert_eq!(
                SecStaticCodeCheckValidity(code, 0, std::ptr::null()),
                SUCCESS
            );
            let mut info = std::ptr::null();
            assert_eq!(SecCodeCopySigningInformation(code, 0, &mut info), SUCCESS);
            let dictionary = corefoundation::collections::dictionary(info).unwrap();
            let key = corefoundation::string::from_bytes(b"identifier", false);
            let index = dictionary.position(key).unwrap();
            assert_eq!(
                corefoundation::string::as_string(dictionary.entries[index].1)
                    .unwrap()
                    .utf8(),
                b"x"
            );
            corefoundation::api::CFRelease(key);
            corefoundation::api::CFRelease(info);
            bytes[60] ^= 1;
            std::fs::write(&path, &bytes).unwrap();
            let mut error = std::ptr::null();
            assert_eq!(
                SecStaticCodeCheckValidityWithErrors(code, 0, std::ptr::null(), &mut error),
                code_directory::MODIFIED
            );
            assert!(!error.is_null());
            corefoundation::api::CFRelease(error);
            let mut output = code;
            assert_eq!(
                SecStaticCodeCreateWithPath(std::ptr::null(), 0, &mut output),
                PARAM
            );
            assert_eq!(output, code);
            corefoundation::api::CFRelease(code);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn dynamic_validation_does_not_claim_unverified_success() {
        objc::bootstrap();
        let text = corefoundation::string::from_bytes(b"anchor apple generic", false);
        let mut requirement = std::ptr::null();
        let mut code = std::ptr::null();
        // SAFETY: a string this test made, and two writable out-parameters.
        unsafe {
            assert_eq!(
                SecRequirementCreateWithString(text, 0, &mut requirement),
                SUCCESS
            );
            assert_eq!(SecCodeCopySelf(0, &mut code), SUCCESS);
        }
        assert!(!requirement.is_null() && !code.is_null());
        // SAFETY: two handles the calls above made.
        assert_eq!(
            unsafe { SecCodeCheckValidity(code, 0, requirement) },
            -67072
        );
        // SAFETY: a handle this test owns, against storage that is not one.
        let mut other = 0_u64;
        assert_eq!(
            unsafe { SecCodeCheckValidity((&raw mut other).cast(), 0, requirement) },
            INVALID_OBJECT_REF
        );
        // SAFETY: three objects this test owns.
        unsafe {
            corefoundation::api::CFRelease(requirement);
            corefoundation::api::CFRelease(code);
            corefoundation::api::CFRelease(text);
        }
    }
}
