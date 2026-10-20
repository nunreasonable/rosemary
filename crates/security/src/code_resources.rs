//! Resource envelopes are authenticated by the CodeDirectory before this walk.
use corefoundation::plist::{self, Value};
use ring::digest;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

const BAD: i32 = -67054;
const FORMAT: i32 = -67058;

struct Rule {
    regex: regex::Regex,
    weight: f64,
    omit: bool,
}

fn boolean(value: Option<&Value>) -> Result<bool, i32> {
    match value {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(FORMAT),
    }
}
fn regular_digest(path: &Path, algorithm: &'static digest::Algorithm) -> Result<Vec<u8>, i32> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|_| BAD)?;
    let mut hash = digest::Context::new(algorithm);
    let mut bytes = [0; 65536];
    loop {
        let count = file.read(&mut bytes).map_err(|_| BAD)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(hash.finish().as_ref().to_vec())
}

pub(super) fn verify(
    root: &Path,
    executable: &Path,
    envelope: &[u8],
    nested: &mut dyn FnMut(&Path, Option<&str>, Option<&[u8]>) -> Result<(), i32>,
) -> Result<(), i32> {
    let value = plist::parse(envelope).map_err(|_| FORMAT)?;
    let files = value
        .get("files2")
        .and_then(Value::as_dictionary)
        .ok_or(-67007)?;
    let raw_rules = value
        .get("rules2")
        .and_then(Value::as_dictionary)
        .ok_or(FORMAT)?;
    let mut rules = Vec::new();
    for (pattern, value) in raw_rules {
        let (weight, omit) = match value {
            Value::Bool(true) => (1.0, false),
            Value::Dictionary(fields) => {
                if fields
                    .keys()
                    .any(|k| !["weight", "omit", "optional", "nested"].contains(&k.as_str()))
                {
                    return Err(-67053);
                }
                let weight = match fields.get("weight") {
                    None => 1.0,
                    Some(Value::Real(v)) => *v,
                    Some(Value::Integer(v)) => *v as f64,
                    _ => return Err(FORMAT),
                };
                if !weight.is_finite() {
                    return Err(FORMAT);
                }
                (weight, boolean(fields.get("omit"))?)
            }
            _ => return Err(FORMAT),
        };
        rules.push(Rule {
            regex: regex::Regex::new(pattern).map_err(|_| -67053)?,
            weight,
            omit,
        });
    }
    let mut sealed = BTreeSet::new();
    for (name, seal) in files {
        let relative = Path::new(name);
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(FORMAT);
        }
        let path = root.join(relative);
        let mut parent = root.to_path_buf();
        let components: Vec<_> = relative.components().collect();
        for part in &components[..components.len().saturating_sub(1)] {
            parent.push(part.as_os_str());
            if std::fs::symlink_metadata(&parent)
                .map_err(|_| BAD)?
                .file_type()
                .is_symlink()
            {
                return Err(BAD);
            }
        }
        let fields: &BTreeMap<String, Value> = seal.as_dictionary().ok_or(FORMAT)?;
        if fields.contains_key("cdhash") || fields.contains_key("requirement") {
            if fields
                .keys()
                .any(|key| !["cdhash", "requirement", "optional"].contains(&key.as_str()))
            {
                return Err(FORMAT);
            }
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| BAD)?;
            if metadata.file_type().is_symlink() {
                return Err(BAD);
            }
            let requirement = fields
                .get("requirement")
                .map(|v| v.as_str().ok_or(FORMAT))
                .transpose()?;
            let hash = match fields.get("cdhash") {
                Some(Value::Data(bytes)) => Some(bytes.as_slice()),
                None => None,
                _ => return Err(FORMAT),
            };
            nested(&path, requirement, hash)?;
            sealed.insert(name.clone());
            continue;
        }
        if fields
            .keys()
            .any(|k| !["hash", "hash2", "optional", "symlink"].contains(&k.as_str()))
        {
            return Err(FORMAT);
        }
        let optional = boolean(fields.get("optional"))?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && optional => {
                sealed.insert(name.clone());
                continue;
            }
            Err(_) => return Err(BAD),
        };
        if let Some(target) = fields.get("symlink") {
            let target = target.as_str().ok_or(FORMAT)?;
            if !metadata.file_type().is_symlink()
                || std::fs::read_link(&path).map_err(|_| BAD)? != Path::new(target)
            {
                return Err(BAD);
            }
        } else {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(BAD);
            }
            let mut checked = false;
            for (key, algorithm) in [
                ("hash", &digest::SHA1_FOR_LEGACY_USE_ONLY),
                ("hash2", &digest::SHA256),
            ] {
                if let Some(value) = fields.get(key) {
                    let Value::Data(expected) = value else {
                        return Err(FORMAT);
                    };
                    if regular_digest(&path, algorithm)? != *expected {
                        return Err(BAD);
                    }
                    checked = true;
                }
            }
            if !checked {
                return Err(FORMAT);
            }
        }
        sealed.insert(name.clone());
    }
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut count = 0;
    while let Some((directory, depth)) = pending.pop() {
        if depth > 64 {
            return Err(-67004);
        }
        for entry in std::fs::read_dir(directory).map_err(|_| BAD)? {
            count += 1;
            if count > 100_000 {
                return Err(-67004);
            }
            let entry = entry.map_err(|_| BAD)?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| BAD)?
                .to_str()
                .ok_or(BAD)?;
            if relative == "_CodeSignature" || path == executable {
                continue;
            }
            let kind = entry.file_type().map_err(|_| BAD)?;
            if sealed.contains(relative) {
                continue;
            }
            if kind.is_dir() {
                pending.push((path, depth + 1));
                continue;
            }
            if sealed.contains(relative) {
                continue;
            }
            let mut winner: Option<&Rule> = None;
            for rule in &rules {
                if rule.regex.is_match(relative)
                    && winner.is_none_or(|old| {
                        rule.weight > old.weight || (rule.weight == old.weight && !rule.omit)
                    })
                {
                    winner = Some(rule);
                }
            }
            if winner.is_some_and(|r| !r.omit) {
                return Err(BAD);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_seals_detect_changes_and_added_files() {
        let root =
            std::env::temp_dir().join(format!("rosemary-resource-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("data"), b"resource").unwrap();
        let files = BTreeMap::from([(
            "data".to_string(),
            Value::Dictionary(BTreeMap::from([(
                "hash2".to_string(),
                Value::Data(
                    digest::digest(&digest::SHA256, b"resource")
                        .as_ref()
                        .to_vec(),
                ),
            )])),
        )]);
        let envelope = plist::write_xml(&Value::Dictionary(BTreeMap::from([
            ("files2".to_string(), Value::Dictionary(files)),
            (
                "rules2".to_string(),
                Value::Dictionary(BTreeMap::from([("^.*".to_string(), Value::Bool(true))])),
            ),
        ])));
        assert_eq!(
            verify(&root, &root.join("main"), &envelope, &mut |_, _, _| Err(
                -67072
            )),
            Ok(())
        );
        std::fs::write(root.join("extra"), b"unsealed").unwrap();
        assert_eq!(
            verify(&root, &root.join("main"), &envelope, &mut |_, _, _| Err(
                -67072
            )),
            Err(BAD)
        );
        std::fs::remove_file(root.join("extra")).unwrap();
        std::fs::write(root.join("data"), b"modified").unwrap();
        assert_eq!(
            verify(&root, &root.join("main"), &envelope, &mut |_, _, _| Err(
                -67072
            )),
            Err(BAD)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
