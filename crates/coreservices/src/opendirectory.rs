//! OpenDirectory attribute keys. Directory sessions and queries are not yet
//! implemented; publishing keys does not claim that a directory is available.

const KEYS: &[(&str, &str)] = &[
    ("_kODAttributeTypeGUID", "dsAttrTypeStandard:GeneratedUID"),
    (
        "_kODAttributeTypeRecordName",
        "dsAttrTypeStandard:RecordName",
    ),
    ("_kODRecordTypeUsers", "dsRecTypeStandard:Users"),
];

pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::base::string_globals(KEYS)
}

#[cfg(test)]
mod tests {
    #[test]
    fn directory_keys_export_string_cells() {
        for ((expected_name, expected_value), (name, address)) in
            super::KEYS.iter().zip(super::data_symbols())
        {
            assert_eq!(*expected_name, name);
            // SAFETY: string_globals returns process-lifetime CFString cells.
            let value = unsafe { *(address as *const corefoundation::base::CFTypeRef) };
            assert!(!value.is_null());
            assert_eq!(
                unsafe { foundation::string::bytes(value as _) },
                expected_value.as_bytes()
            );
        }
    }
}
