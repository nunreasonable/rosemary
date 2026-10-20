//! Non-Mach-O code signatures live in extended attributes.
use super::code_directory::{INVALID, UNSIGNED};
use starnix::xattr::{self, Target};
use std::{collections::BTreeMap, ffi::CString, path::Path};

pub(super) fn components(path: &Path) -> Result<BTreeMap<usize, Vec<u8>>, i32> {
    let path = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| INVALID)?;
    let mut parts = BTreeMap::new();
    for (slot, name) in [
        (0, "CodeDirectory"),
        (2, "CodeRequirements"),
        (3, "CodeResources"),
        (4, "CodeApplication"),
        (5, "CodeEntitlements"),
        (0x10000, "CodeSignature"),
    ] {
        let name = CString::new(format!("user.com.apple.cs.{name}")).unwrap();
        let target = Target::Path(&path, true);
        let length = match xattr::get(target, &name, &mut []) {
            Ok(n) => n,
            Err(error) if xattr::is_missing(&error) => continue,
            Err(_) => return Err(-67060),
        };
        if length > 16 * 1024 * 1024 {
            return Err(-67004);
        }
        let mut data = vec![0; length];
        let actual = xattr::get(target, &name, &mut data).map_err(|_| -67060)?;
        if actual != length {
            return Err(INVALID);
        }
        if slot == 0x10000 {
            let mut wrapped = 0xfade0b01u32.to_be_bytes().to_vec();
            wrapped.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
            wrapped.extend(data);
            data = wrapped;
        }
        parts.insert(slot, data);
    }
    if !parts.contains_key(&0) {
        return Err(UNSIGNED);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_detached_attributes_and_reports_unsigned_files() {
        let path =
            std::env::temp_dir().join(format!("rosemary-xattr-signature-{}", std::process::id()));
        std::fs::write(&path, b"test code").unwrap();
        assert_eq!(components(&path).err(), Some(UNSIGNED));
        let name = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let cd = crate::code_directory::tests::directory(b"test code");
        xattr::set(
            Target::Path(&name, true),
            c"user.com.apple.cs.CodeDirectory",
            &cd,
        )
        .unwrap();
        let parts = components(&path).unwrap();
        let directory = crate::code_directory::Directory::parse(parts.get(&0).unwrap()).unwrap();
        assert_eq!(directory.verify_pages(b"test code", 9), Ok(()));
        assert!(directory.verify_pages(b"Test code", 9).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
