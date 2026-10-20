//! Bounds-checked CodeDirectory integrity checks. Certificate trust is separate.

use ring::digest;

pub(super) const INVALID: i32 = -67045;
pub(super) const UNSUPPORTED: i32 = -67059;
pub(super) const UNSIGNED: i32 = -67062;
pub(super) const MODIFIED: i32 = -67061;

fn word(bytes: &[u8], offset: usize, little: bool) -> Result<usize, i32> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or(INVALID)?
        .try_into()
        .unwrap();
    Ok(if little {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    } as usize)
}

/// Select by Mach CPU type, never by an unchecked file offset.
/// Resolve a requested slice only after validating the universal header and ranges.
pub(super) fn cpu_at_offset(image: &[u8], offset: usize) -> Result<u32, i32> {
    let cpu = u32::try_from(word(image, offset.checked_add(4).ok_or(INVALID)?, true)?)
        .map_err(|_| INVALID)?;
    let selected = architecture(image, cpu)?;
    if selected.as_ptr() as usize - image.as_ptr() as usize != offset {
        return Err(INVALID);
    }
    Ok(cpu)
}

pub(super) fn architecture(image: &[u8], cpu: u32) -> Result<&[u8], i32> {
    let magic = word(image, 0, false)?;
    if magic != 0xcafebabe && magic != 0xcafebabf {
        if word(image, 0, true)? != 0xfeedfacf {
            return Err(UNSUPPORTED);
        }
        return if word(image, 4, true)? == cpu as usize {
            Ok(image)
        } else {
            Err(-67066)
        };
    }
    let count = word(image, 4, false)?;
    let stride = if magic == 0xcafebabf {
        32usize
    } else {
        20usize
    };
    let table_end = 8usize
        .checked_add(count.checked_mul(stride).ok_or(INVALID)?)
        .ok_or(INVALID)?;
    if count == 0 || table_end > image.len() {
        return Err(INVALID);
    }
    let mut selected = None;
    let mut ranges = Vec::new();
    for i in 0..count {
        let at = 8 + i * stride;
        let kind = word(image, at, false)?;
        let (offset, length, alignment) = if stride == 20 {
            (
                word(image, at + 8, false)?,
                word(image, at + 12, false)?,
                word(image, at + 16, false)?,
            )
        } else {
            let wide = |at| -> Result<usize, i32> {
                usize::try_from(u64::from_be_bytes(
                    image.get(at..at + 8).ok_or(INVALID)?.try_into().unwrap(),
                ))
                .map_err(|_| INVALID)
            };
            (wide(at + 8)?, wide(at + 16)?, word(image, at + 24, false)?)
        };
        let end = offset.checked_add(length).ok_or(INVALID)?;
        let align = 1usize
            .checked_shl(u32::try_from(alignment).map_err(|_| INVALID)?)
            .ok_or(INVALID)?;
        if offset < table_end
            || offset % align != 0
            || length < 32
            || end > image.len()
            || ranges.iter().any(|&(s, e)| offset < e && end > s)
        {
            return Err(INVALID);
        }
        let slice = &image[offset..end];
        if word(slice, 0, true)? != 0xfeedfacf
            || word(slice, 4, true)? != kind
            || word(slice, 8, true)? != word(image, at + 4, false)?
        {
            return Err(INVALID);
        }
        ranges.push((offset, end));
        if kind == cpu as usize {
            if selected.is_some() {
                return Err(-67066);
            }
            selected = Some(slice);
        }
    }
    selected.ok_or(-67066)
}

pub(super) struct Directory<'a> {
    pub bytes: &'a [u8],
    pub identifier: &'a str,
    pub flags: usize,
    hash_offset: usize,
    slots: usize,
    special: usize,
    limit: usize,
    page: usize,
    hash_size: usize,
    algorithm: &'static digest::Algorithm,
}

impl<'a> Directory<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, i32> {
        if word(bytes, 0, false)? != 0xfade0c02 {
            return Err(INVALID);
        }
        let bytes = bytes.get(..word(bytes, 4, false)?).ok_or(INVALID)?;
        let version = word(bytes, 8, false)?;
        let minimum = match version {
            0x20000 => 44,
            0x20100 => 48,
            0x20200 => 52,
            0x20300 => 64,
            0x20400 => 88,
            0x20500 => 96,
            0x20600 => 108,
            _ => return Err(UNSUPPORTED),
        };
        if bytes.len() < minimum {
            return Err(INVALID);
        }
        if version >= 0x20100 && word(bytes, 44, false)? != 0 {
            return Err(UNSUPPORTED);
        }
        if version >= 0x20300 && bytes[56..64].iter().any(|b| *b != 0) {
            return Err(UNSUPPORTED);
        }
        if version >= 0x20500 && word(bytes, 92, false)? != 0 {
            return Err(UNSUPPORTED);
        }
        if version >= 0x20600 && bytes[96..108].iter().any(|b| *b != 0) {
            return Err(UNSUPPORTED);
        }
        let (algorithm, hash_size) = match bytes[37] {
            1 => (&digest::SHA1_FOR_LEGACY_USE_ONLY, 20),
            2 => (&digest::SHA256, 32),
            3 => (&digest::SHA256, 20),
            4 => (&digest::SHA384, 48),
            _ => return Err(UNSUPPORTED),
        };
        if bytes[36] as usize != hash_size {
            return Err(INVALID);
        }
        let hash_offset = word(bytes, 16, false)?;
        let special = word(bytes, 24, false)?;
        let slots = word(bytes, 28, false)?;
        let start = hash_offset
            .checked_sub(special.checked_mul(hash_size).ok_or(INVALID)?)
            .ok_or(INVALID)?;
        let end = hash_offset
            .checked_add(slots.checked_mul(hash_size).ok_or(INVALID)?)
            .ok_or(INVALID)?;
        if start < minimum || end > bytes.len() {
            return Err(INVALID);
        }
        let identifier_offset = word(bytes, 20, false)?;
        if identifier_offset < minimum || identifier_offset >= start {
            return Err(INVALID);
        }
        let text = &bytes[identifier_offset..start];
        let end = text.iter().position(|b| *b == 0).ok_or(INVALID)?;
        let identifier = std::str::from_utf8(&text[..end]).map_err(|_| INVALID)?;
        let limit = word(bytes, 32, false)?;
        let page = if bytes[39] == 0 {
            limit.max(1)
        } else {
            1usize.checked_shl(bytes[39] as u32).ok_or(INVALID)?
        };
        if slots != limit.div_ceil(page) {
            return Err(INVALID);
        }
        Ok(Self {
            bytes,
            identifier,
            flags: word(bytes, 12, false)?,
            hash_offset,
            slots,
            special,
            limit,
            page,
            hash_size,
            algorithm,
        })
    }

    pub fn authenticated_by(&self, hashes: &[(String, Vec<u8>)]) -> bool {
        let oid = match self.bytes[37] {
            1 => "1.3.14.3.2.26",
            2 | 3 => "2.16.840.1.101.3.4.2.1",
            4 => "2.16.840.1.101.3.4.2.2",
            _ => return false,
        };
        let digest = digest::digest(self.algorithm, self.bytes);
        hashes
            .iter()
            .any(|(algorithm, value)| algorithm == oid && value.as_slice() == digest.as_ref())
    }

    pub fn cdhash(&self) -> Vec<u8> {
        digest::digest(self.algorithm, self.bytes).as_ref()[..20].to_vec()
    }

    pub fn verify_pages(&self, image: &[u8], signature_offset: usize) -> Result<(), i32> {
        if self.limit != signature_offset {
            return Err(UNSUPPORTED);
        }
        let code = image.get(..self.limit).ok_or(INVALID)?;
        for (i, page) in code.chunks(self.page).enumerate() {
            let offset = self.hash_offset + i * self.hash_size;
            if digest::digest(self.algorithm, page).as_ref()[..self.hash_size]
                != self.bytes[offset..offset + self.hash_size]
            {
                return Err(MODIFIED);
            }
        }
        debug_assert_eq!(code.chunks(self.page).len(), self.slots);
        Ok(())
    }

    pub fn verify_special(&self, slot: usize, content: Option<&[u8]>) -> Result<(), i32> {
        if slot == 0 || slot > self.special {
            return if content.is_none() {
                Ok(())
            } else {
                Err(MODIFIED)
            };
        }
        let at = self.hash_offset - slot * self.hash_size;
        let expected = &self.bytes[at..at + self.hash_size];
        if expected.iter().all(|b| *b == 0) {
            return if content.is_none() {
                Ok(())
            } else {
                Err(MODIFIED)
            };
        }
        let content = content.ok_or(MODIFIED)?;
        if &digest::digest(self.algorithm, content).as_ref()[..self.hash_size] != expected {
            return Err(MODIFIED);
        }
        Ok(())
    }

    pub fn special_slots(&self) -> usize {
        self.special
    }
}

/// A thin 64-bit Mach-O. Universal selection must be explicit at the API layer.
pub(super) fn signature(image: &[u8]) -> Result<(usize, &[u8]), i32> {
    if word(image, 0, true)? != 0xfeedfacf {
        return Err(UNSUPPORTED);
    }
    let count = word(image, 16, true)?;
    let end = 32usize.checked_add(word(image, 20, true)?).ok_or(INVALID)?;
    if end > image.len() || count > (end - 32) / 8 {
        return Err(INVALID);
    }
    let mut cursor = 32;
    let mut found = None;
    for _ in 0..count {
        let command = word(image, cursor, true)?;
        let size = word(image, cursor + 4, true)?;
        if size < 8 || size % 8 != 0 || size > end - cursor {
            return Err(INVALID);
        }
        if command == 0x1d {
            if size != 16 || found.is_some() {
                return Err(INVALID);
            }
            let offset = word(image, cursor + 8, true)?;
            let length = word(image, cursor + 12, true)?;
            if offset < end {
                return Err(INVALID);
            }
            found = Some((
                offset,
                image
                    .get(offset..offset.checked_add(length).ok_or(INVALID)?)
                    .ok_or(INVALID)?,
            ));
        }
        cursor += size;
    }
    if cursor != end {
        return Err(INVALID);
    }
    found.ok_or(UNSIGNED)
}

pub(super) fn components(blob: &[u8]) -> Result<std::collections::BTreeMap<usize, &[u8]>, i32> {
    if word(blob, 0, false)? != 0xfade0cc0 {
        return Err(INVALID);
    }
    let blob = blob.get(..word(blob, 4, false)?).ok_or(INVALID)?;
    let count = word(blob, 8, false)?;
    let index_end = 12usize
        .checked_add(count.checked_mul(8).ok_or(INVALID)?)
        .ok_or(INVALID)?;
    if index_end > blob.len() {
        return Err(INVALID);
    }
    let mut result = std::collections::BTreeMap::new();
    let mut ranges = Vec::new();
    for i in 0..count {
        let slot = word(blob, 12 + 8 * i, false)?;
        let offset = word(blob, 16 + 8 * i, false)?;
        if offset < index_end {
            return Err(INVALID);
        }
        let length = word(blob, offset.checked_add(4).ok_or(INVALID)?, false)?;
        let end = offset.checked_add(length).ok_or(INVALID)?;
        if length < 8 || end > blob.len() || ranges.iter().any(|&(s, e)| offset < e && end > s) {
            return Err(INVALID);
        }
        ranges.push((offset, end));
        if result.insert(slot, &blob[offset..end]).is_some() {
            return Err(INVALID);
        }
    }
    Ok(result)
}

pub(super) fn primary(blob: &[u8]) -> Result<Directory<'_>, i32> {
    let parts = components(blob)?;
    Directory::parse(parts.get(&0).ok_or(INVALID)?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn directory(code: &[u8]) -> Vec<u8> {
        let mut cd = vec![0; 80];
        for (offset, value) in [
            (0, 0xfade0c02u32),
            (4, 80),
            (8, 0x20000),
            (12, 2),
            (16, 48),
            (20, 44),
            (28, 1),
            (32, code.len() as u32),
        ] {
            cd[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
        cd[36] = 32;
        cd[37] = 2;
        cd[39] = 12;
        cd[44..46].copy_from_slice(b"x\0");
        cd[48..80].copy_from_slice(digest::digest(&digest::SHA256, code).as_ref());
        cd
    }
    pub(crate) fn signed_image() -> Vec<u8> {
        let mut image = vec![0; 64];
        for (offset, value) in [
            (0, 0xfeedfacfu32),
            (4, 0x1000007),
            (12, 2),
            (16, 1),
            (20, 16),
            (32, 0x1d),
            (36, 16),
            (40, 64),
            (44, 100),
        ] {
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let cd = directory(&image);
        for value in [0xfade0cc0u32, 100, 1, 0, 20] {
            image.extend_from_slice(&value.to_be_bytes());
        }
        image.extend_from_slice(&cd);
        image
    }

    #[test]
    fn selects_universal_architecture_and_rejects_overlap() {
        let x86 = signed_image();
        let mut arm = x86.clone();
        arm[4..8].copy_from_slice(&0x100000cu32.to_le_bytes());
        let mut fat = vec![0; 256 + arm.len()];
        for (at, value) in [
            (0, 0xcafebabeu32),
            (4, 2),
            (8, 0x1000007),
            (16, 64),
            (20, x86.len() as u32),
            (28, 0x100000c),
            (36, 256),
            (40, arm.len() as u32),
        ] {
            fat[at..at + 4].copy_from_slice(&value.to_be_bytes());
        }
        fat[64..64 + x86.len()].copy_from_slice(&x86);
        fat[256..].copy_from_slice(&arm);
        assert_eq!(architecture(&fat, 0x1000007).unwrap(), x86);
        assert_eq!(architecture(&fat, 0x100000c).unwrap(), arm);
        assert!(architecture(&fat, 7).is_err());
        assert_eq!(cpu_at_offset(&fat, 64), Ok(0x1000007));
        assert_eq!(cpu_at_offset(&fat, 256), Ok(0x100000c));
        assert!(cpu_at_offset(&fat, 65).is_err());
        assert!(cpu_at_offset(&fat, usize::MAX).is_err());
        fat[36..40].copy_from_slice(&64u32.to_be_bytes());
        assert!(architecture(&fat, 0x1000007).is_err());
    }

    #[test]
    fn checks_embedded_signature_and_rejects_overlapping_index() {
        let mut image = signed_image();
        let (offset, blob) = signature(&image).unwrap();
        let cd = primary(blob).unwrap();
        assert_eq!(cd.verify_pages(&image, offset), Ok(()));
        for end in 0..blob.len() {
            assert!(primary(&blob[..end]).is_err());
        }
        image[80..84].copy_from_slice(&12u32.to_be_bytes());
        assert!(primary(signature(&image).unwrap().1).is_err());
    }

    #[test]
    fn detects_modified_code_and_accepts_matching_hashes() {
        let cd = directory(b"test code");
        let parsed = Directory::parse(&cd).unwrap();
        assert_eq!(parsed.verify_pages(b"test code", 9), Ok(()));
        assert_eq!(parsed.verify_pages(b"Test code", 9), Err(MODIFIED));
        assert_eq!(parsed.identifier, "x");
        assert_eq!(parsed.cdhash().len(), 20);
    }
    #[test]
    fn rejects_truncation_and_invalid_hash_tables() {
        let mut cd = directory(b"test code");
        for end in 0..cd.len() {
            assert!(Directory::parse(&cd[..end]).is_err());
        }
        cd[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Directory::parse(&cd).is_err());
    }
    #[test]
    fn distinguishes_unsigned_and_unsupported_images() {
        let mut image = vec![0; 32];
        image[..4].copy_from_slice(&0xfeedfacfu32.to_le_bytes());
        assert_eq!(signature(&image).err(), Some(UNSIGNED));
        assert_eq!(signature(b"not a Mach-O").err(), Some(UNSUPPORTED));
    }
}
