#![allow(non_snake_case)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use corefoundation::base::CFTypeRef;

/// `OSStatus`, in the two values these calls answer with.
type OSStatus = i32;
const SUCCESS: OSStatus = 0;
/// `errSecItemNotFound`.
const ITEM_NOT_FOUND: OSStatus = -25300;
/// `errSecDuplicateItem`.
const DUPLICATE_ITEM: OSStatus = -25299;
/// `errSecParam`.
const PARAM: OSStatus = -50;

/// The attribute names a query is built from, as their documented string
/// values.
///
/// The values matter and are not the symbol names: `kSecClass` is the string
/// `"class"`, and a caller that builds its query dictionary by hand - or reads
/// one back - uses the string. Published as data as well, because each is a
/// `CFStringRef` **variable**: unbound it binds to zero and the guest's
/// `CFDictionarySetValue` key is address zero.
pub const KEYS: &[(&str, &str)] = &[
    ("_kSecClass", "class"),
    ("_kSecClassGenericPassword", "genp"),
    ("_kSecClassInternetPassword", "inet"),
    ("_kSecAttrService", "svce"),
    ("_kSecAttrAccount", "acct"),
    ("_kSecAttrLabel", "labl"),
    ("_kSecAttrAccessible", "pdmn"),
    ("_kSecAttrAccessibleAfterFirstUnlock", "ck"),
    ("_kSecAttrAccessibleWhenUnlocked", "ak"),
    ("_kSecValueData", "v_Data"),
    ("_kSecReturnData", "r_Data"),
    ("_kSecReturnAttributes", "r_Attributes"),
    ("_kSecMatchLimit", "m_Limit"),
    ("_kSecMatchLimitOne", "m_LimitOne"),
    ("_kSecMatchLimitAll", "m_LimitAll"),
    ("_kSecMatchTrustedOnly", "m_TrustedOnly"),
    ("_kSecMatchSearchList", "m_SearchList"),
    ("_kSecClassCertificate", "cert"),
    ("_kSecReturnRef", "r_Ref"),
];

/// What identifies an item: its class, its service and its account.
///
/// Not the whole attribute set. Apple's primary key for a generic password is
/// exactly these, and an item that matched on more would be one a caller could
/// never find again after storing it with a label it did not repeat.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    class: String,
    service: String,
    account: String,
}

static ITEMS: Mutex<BTreeMap<Identity, Vec<u8>>> = Mutex::new(BTreeMap::new());
static LOADED: std::sync::Once = std::sync::Once::new();

fn items() -> std::sync::MutexGuard<'static, BTreeMap<Identity, Vec<u8>>> {
    LOADED.call_once(|| {
        *ITEMS.lock().unwrap_or_else(|e| e.into_inner()) = load();
    });
    ITEMS.lock().unwrap_or_else(|e| e.into_inner())
}

fn store_path() -> PathBuf {
    foundation::host_home_directory().join("Library/Keychains/rosemary-items.tsv")
}

/// Tab-separated, with the value hex-encoded.
///
/// Hex rather than raw bytes because the value is a `CFData` and may hold
/// anything, including a tab, a newline or a zero - and the three fields around
/// it are the identity, which must stay readable for the file to be debuggable.
fn load() -> BTreeMap<Identity, Vec<u8>> {
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let identity = Identity {
                class: f.next()?.to_string(),
                service: f.next()?.to_string(),
                account: f.next()?.to_string(),
            };
            Some((identity, from_hex(f.next()?)?))
        })
        .collect()
}

fn save(items: &BTreeMap<Identity, Vec<u8>>) {
    let mut text = String::new();
    for (identity, value) in items {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            identity.class,
            identity.service,
            identity.account,
            to_hex(value)
        ));
    }
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// The value at `key` in `query`, as text.
///
/// # Safety
///
/// `query` must be null or a `CFDictionary`.
unsafe fn attribute(query: CFTypeRef, key: &str) -> Option<String> {
    let name = foundation::string::from_str(key);
    // SAFETY: the caller's obligation for `query`; `name` is a live string.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(query, name as CFTypeRef) };
    // SAFETY: a fresh string this call owns.
    unsafe { corefoundation::api::CFRelease(name as CFTypeRef) };
    if value.is_null() {
        return None;
    }
    // SAFETY: whatever the caller stored, read as a string.
    let bytes = unsafe { foundation::string::bytes(value as objc::object::Id) };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// # Safety
///
/// `query` must be null or a `CFDictionary`.
unsafe fn identity_of(query: CFTypeRef) -> Identity {
    let class = unsafe { attribute(query, "class") }.unwrap_or_else(|| "genp".to_string());
    if class == "keys" {
        // A key is found by its application tag, which is bytes.
        let tag = unsafe { crate::key::bytes_of(crate::key::value(query, "atag")) };
        return key_identity(&tag.unwrap_or_default());
    }
    Identity {
        class,
        service: unsafe { attribute(query, "svce") }.unwrap_or_default(),
        account: unsafe { attribute(query, "acct") }.unwrap_or_default(),
    }
}

fn key_identity(tag: &[u8]) -> Identity {
    Identity {
        class: "keys".to_string(),
        service: String::new(),
        account: to_hex(tag),
    }
}

/// Store a permanent key under its tag, as `SecKeyCreateRandomKey` does for
/// `kSecAttrIsPermanent`. An existing key under the tag is kept.
pub(crate) fn store_key(tag: &[u8], pkcs8: &[u8]) {
    let mut items = items();
    if items.contains_key(&key_identity(tag)) {
        return;
    }
    items.insert(key_identity(tag), pkcs8.to_vec());
    save(&items);
}

/// The answer to a query for a key: the key itself for `kSecReturnRef`, its
/// attributes for `kSecReturnAttributes`, and its external representation for
/// `kSecReturnData`.
///
/// # Safety
///
/// `query` is a `CFDictionary`; `result` is writable.
unsafe fn key_result(
    query: CFTypeRef,
    identity: &Identity,
    pkcs8: Vec<u8>,
    result: *mut CFTypeRef,
) -> OSStatus {
    let tag = from_hex(&identity.account);
    let key = crate::key::from_stored(pkcs8, tag);
    if key.is_null() {
        return ITEM_NOT_FOUND;
    }
    let object = if unsafe { flag(query, "r_Ref") } {
        key
    } else {
        let object = if unsafe { flag(query, "r_Attributes") } {
            unsafe { crate::key::copy_attributes(key) }
        } else {
            unsafe { crate::key::SecKeyCopyExternalRepresentation(key, std::ptr::null_mut()) }
        };
        // SAFETY: the key this call made.
        unsafe { corefoundation::api::CFRelease(key) };
        object
    };
    // SAFETY: writable, per the caller.
    unsafe { result.write(object) };
    SUCCESS
}

/// The bytes of the `kSecValueData` in `query`, if it has one.
///
/// # Safety
///
/// `query` must be null or a `CFDictionary`.
unsafe fn value_data(query: CFTypeRef) -> Option<Vec<u8>> {
    // A key is stored as its private key document.
    let reference = unsafe { crate::key::value(query, "v_Ref") };
    if let Some((pkcs8, _)) = unsafe { crate::key::private_parts(reference) } {
        return Some(pkcs8);
    }
    let name = foundation::string::from_str("v_Data");
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(query, name as CFTypeRef) };
    // SAFETY: a fresh string this call owns.
    unsafe { corefoundation::api::CFRelease(name as CFTypeRef) };
    if value.is_null() {
        return None;
    }
    // A `CFData` or a string, and **type-checked** rather than read as bytes on
    // the strength of the key it was under. `CFDataGetLength` on an object that
    // is not a `CFData` reads a length out of another type's field, and the
    // first thing done with a length is to allocate it: the failure is
    // `memory allocation of 140733193388032 bytes failed`, in this runtime,
    // nowhere near the call that passed the wrong object.
    //
    // SAFETY: the caller's obligation; both accessors answer `None` for an
    // object of the wrong class.
    if let Some(data) = unsafe { corefoundation::collections::data(value) } {
        return Some(data.clone());
    }
    // SAFETY: as above - answers empty for anything that is not a string.
    let text = unsafe { foundation::string::bytes(value as objc::object::Id) };
    (!text.is_empty()).then_some(text)
}

/// `OSStatus SecItemAdd(CFDictionaryRef attributes, CFTypeRef *result)`.
///
/// `errSecDuplicateItem` for an identity already stored, which is Apple's
/// answer and the one a caller branches on to decide between `SecItemAdd` and
/// `SecItemUpdate`.
///
/// # Safety
///
/// `attributes` is null or a `CFDictionary`; `result` is null or writable.
pub unsafe extern "C" fn SecItemAdd(attributes: CFTypeRef, result: *mut CFTypeRef) -> OSStatus {
    if !result.is_null() {
        // Nothing is returned: `kSecReturnData` on an *add* asks for the item
        // back, and this store has nothing to say about it that the caller did
        // not just provide.
        unsafe { result.write(std::ptr::null_mut()) };
    }
    if attributes.is_null() {
        return PARAM;
    }
    let identity = unsafe { identity_of(attributes) };
    let Some(data) = (unsafe { value_data(attributes) }) else {
        return PARAM;
    };
    let mut items = items();
    if items.contains_key(&identity) {
        return DUPLICATE_ITEM;
    }
    items.insert(identity, data);
    save(&items);
    SUCCESS
}

/// `OSStatus SecItemCopyMatching(CFDictionaryRef query, CFTypeRef *result)`.
///
/// Answers the item's data when `kSecReturnData` asked for it. A caller that
/// asked for attributes instead gets `errSecItemNotFound` rather than a
/// fabricated dictionary: the attributes this store keeps are the three it was
/// given, and handing those back as though they were the item's would tell a
/// caller its creation date and accessibility were things it could rely on.
///
/// # Safety
///
/// As [`SecItemAdd`].
pub unsafe extern "C" fn SecItemCopyMatching(query: CFTypeRef, result: *mut CFTypeRef) -> OSStatus {
    if !result.is_null() {
        unsafe { result.write(std::ptr::null_mut()) };
    }
    if query.is_null() {
        return PARAM;
    }
    let key = foundation::string::from_str("m_SearchList");
    // SAFETY: query is a dictionary, and key is a live string.
    let search_list = unsafe { corefoundation::api::CFDictionaryGetValue(query, key as _) };
    unsafe { corefoundation::api::CFRelease(key as _) };
    if !search_list.is_null() {
        let Some(list) = (unsafe { corefoundation::collections::array(search_list) }) else {
            return PARAM;
        };
        if list
            .values
            .iter()
            .any(|item| !unsafe { corefoundation::base::is_kind_of(*item, keychain_class()) })
        {
            return PARAM;
        }
        // Keychain handles currently have no item store. An explicit search
        // must not silently widen to the process store or host trust anchors.
        return ITEM_NOT_FOUND;
    }
    let identity = unsafe { identity_of(query) };
    if identity.class == "cert" {
        // SAFETY: the caller's obligation.
        return unsafe { certificates(query, result) };
    }
    let Some(data) = items().get(&identity).cloned() else {
        return ITEM_NOT_FOUND;
    };
    if result.is_null() {
        // A query with no out parameter is asking only whether the item is
        // there, which it is.
        return SUCCESS;
    }
    if identity.class == "keys" {
        // SAFETY: the caller's obligation.
        return unsafe { key_result(query, &identity, data, result) };
    }
    let object = corefoundation::collections::new_data(data);
    // SAFETY: a `Copy`, so the caller owns what it is handed.
    unsafe { result.write(object) };
    SUCCESS
}

/// Whether `query` holds `kCFBooleanTrue` at `key`.
///
/// # Safety
///
/// `query` must be a `CFDictionary`.
unsafe fn flag(query: CFTypeRef, key: &str) -> bool {
    let name = foundation::string::from_str(key);
    // SAFETY: the caller's obligation; `name` is a live string.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(query, name as CFTypeRef) };
    // SAFETY: a fresh string this call owns.
    unsafe { corefoundation::api::CFRelease(name as CFTypeRef) };
    // SAFETY: whatever was stored there; a `CFBoolean` reads as its value.
    !value.is_null() && unsafe { corefoundation::api::CFBooleanGetValue(value) } != 0
}

/// A query for certificates. The only ones here are the host's trusted roots,
/// which is what a program building its own TLS stack asks the keychain for:
/// `kSecClassCertificate`, `kSecMatchTrustedOnly`, `kSecMatchLimitAll`.
///
/// Answered as `CFData` with `kSecReturnData`, and as certificate handles
/// otherwise; an array for `kSecMatchLimitAll`, the first one for anything
/// else.
///
/// # Safety
///
/// `query` is a `CFDictionary`; `result` is null or writable.
unsafe fn certificates(query: CFTypeRef, result: *mut CFTypeRef) -> OSStatus {
    let ders = crate::trust::anchor_ders();
    if ders.is_empty() {
        return ITEM_NOT_FOUND;
    }
    if result.is_null() {
        return SUCCESS;
    }
    // SAFETY: the caller's obligation.
    let as_data = unsafe { flag(query, "r_Data") };
    let all = unsafe { attribute(query, "m_Limit") }.as_deref() == Some("m_LimitAll");
    let make = |der: &Vec<u8>| -> CFTypeRef {
        match as_data {
            true => corefoundation::collections::new_data(der.clone()),
            false => crate::trust::certificate(der),
        }
    };
    if !all {
        // SAFETY: writable, per the caller.
        unsafe { result.write(make(&ders[0])) };
        return SUCCESS;
    }
    let items: Vec<CFTypeRef> = ders.iter().map(make).filter(|i| !i.is_null()).collect();
    let array = corefoundation::collections::new_array(
        &items,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for item in items {
        // SAFETY: the array took its own reference.
        unsafe { corefoundation::api::CFRelease(item) };
    }
    // SAFETY: writable, per the caller.
    unsafe { result.write(array) };
    SUCCESS
}

/// `OSStatus SecItemDelete(CFDictionaryRef query)`.
///
/// # Safety
///
/// `query` is null or a `CFDictionary`.
pub unsafe extern "C" fn SecItemDelete(query: CFTypeRef) -> OSStatus {
    if query.is_null() {
        return PARAM;
    }
    let identity = unsafe { identity_of(query) };
    let mut items = items();
    if items.remove(&identity).is_none() {
        return ITEM_NOT_FOUND;
    }
    save(&items);
    SUCCESS
}

/// `OSStatus SecItemUpdate(CFDictionaryRef query, CFDictionaryRef changes)`.
///
/// Only the value changes. An update that renamed the account would move the
/// item to an identity the caller did not ask about, and no caller in this tree
/// sends one.
///
/// # Safety
///
/// Both are null or `CFDictionary`s.
pub unsafe extern "C" fn SecItemUpdate(query: CFTypeRef, changes: CFTypeRef) -> OSStatus {
    if query.is_null() || changes.is_null() {
        return PARAM;
    }
    let identity = unsafe { identity_of(query) };
    let Some(data) = (unsafe { value_data(changes) }) else {
        return PARAM;
    };
    let mut items = items();
    if !items.contains_key(&identity) {
        return ITEM_NOT_FOUND;
    }
    items.insert(identity, data);
    save(&items);
    SUCCESS
}

/// A `SecKeychainRef`: a handle with nothing behind it, since this runtime
/// has one store and no keychain files to tell apart.
#[repr(C)]
struct KeychainBody {
    isa: *const objc::class::Class,
}

fn keychain_class() -> &'static objc::class::Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class = corefoundation::base::register(
            c"__R3SecKeychainObject",
            None,
            size_of::<KeychainBody>(),
            true,
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe {
        objc::class::from_ptr(pointer as *const objc::class::Class).expect("a keychain class")
    }
}

/// `OSStatus SecKeychainOpen(const char *path, SecKeychainRef *)`: a handle
/// owned by the caller, whether or not a file is there, as Apple's - opening
/// only names the keychain.
///
/// # Safety
///
/// `out` is null or writable.
pub unsafe extern "C" fn SecKeychainOpen(
    _path: *const std::ffi::c_char,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return -50;
    }
    // SAFETY: `KeychainBody` is the registered layout.
    let body = unsafe { corefoundation::base::allocate::<KeychainBody>(keychain_class()) };
    // SAFETY: writable, checked above.
    unsafe { out.write(body as CFTypeRef) };
    match body.is_null() {
        true => -108,
        false => 0,
    }
}

/// `OSStatus SecKeychainCopySearchList(CFArrayRef *)`: an empty list, owned by
/// the caller - there are no keychain files here to search.
///
/// # Safety
///
/// `out` is null or writable.
pub unsafe extern "C" fn SecKeychainCopySearchList(out: *mut CFTypeRef) -> i32 {
    if out.is_null() {
        return -50;
    }
    let empty = corefoundation::collections::new_array(
        &[],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: writable, checked above.
    unsafe { out.write(empty) };
    0
}

/// `OSStatus SecKeychainAddCallback(SecKeychainCallback, SecKeychainEventMask,
/// void *)`: registered, and never called - nothing outside the program
/// changes this keychain while it runs.
pub unsafe extern "C" fn SecKeychainAddCallback(
    _callback: *const std::ffi::c_void,
    _events: u32,
    _context: *mut std::ffi::c_void,
) -> OSStatus {
    SUCCESS
}

/// `OSStatus SecKeychainRemoveCallback(SecKeychainCallback)`.
pub unsafe extern "C" fn SecKeychainRemoveCallback(_callback: *const std::ffi::c_void) -> OSStatus {
    SUCCESS
}

/// `SecTaskRef SecTaskCreateFromSelf(CFAllocatorRef)`: null, which is the
/// documented answer when the task's code signature cannot be read - there
/// is none to read here, and a caller asking for an entitlement next must
/// already handle that.
pub unsafe extern "C" fn SecTaskCreateFromSelf(_allocator: CFTypeRef) -> CFTypeRef {
    std::ptr::null()
}

exports::exports! {
    functions {
        "SecItemAdd" => SecItemAdd,
        "SecKeychainAddCallback" => SecKeychainAddCallback,
        "SecKeychainCopySearchList" => SecKeychainCopySearchList,
        "SecKeychainOpen" => SecKeychainOpen,
        "SecKeychainRemoveCallback" => SecKeychainRemoveCallback,
        "SecTaskCreateFromSelf" => SecTaskCreateFromSelf,
        "SecItemCopyMatching" => SecItemCopyMatching,
        "SecItemDelete" => SecItemDelete,
        "SecItemUpdate" => SecItemUpdate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_search_lists_do_not_fall_back_to_host_anchors() {
        use corefoundation::{api, callbacks::Policy, collections};
        let key = foundation::string::from_str("m_SearchList") as CFTypeRef;
        let class = foundation::string::from_str("class") as CFTypeRef;
        let certificate = foundation::string::from_str("cert") as CFTypeRef;
        let empty = collections::new_array(&[], Policy::from_type_callbacks(), false);
        for (value, expected) in [(empty, ITEM_NOT_FOUND), (certificate, PARAM)] {
            let query = collections::new_dictionary(
                &[(class, certificate), (key, value)],
                Policy::from_type_callbacks(),
                Policy::from_type_callbacks(),
                false,
            );
            let mut result = certificate;
            // SAFETY: live query and writable result; the query retains its values.
            unsafe {
                assert_eq!(SecItemCopyMatching(query, &mut result), expected);
                assert!(result.is_null());
                api::CFRelease(query);
            }
        }
        for object in [key, class, certificate, empty] {
            unsafe { api::CFRelease(object) };
        }
    }

    #[test]
    fn hex_round_trips_every_byte() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(from_hex(&to_hex(&bytes)), Some(bytes));
    }

    /// An odd-length line is a truncated file, not an item with half a byte.
    #[test]
    fn a_malformed_value_is_refused_rather_than_half_read() {
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
