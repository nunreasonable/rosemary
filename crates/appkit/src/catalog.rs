//! Compiled asset catalogs - a bundle's `Assets.car` - which is where
//! `+[NSColor colorNamed:]` and `+[NSImage imageNamed:]` find what Xcode put in
//! an `.xcassets` folder.
//!
//! The file is a BOM store, as an installer's bill of materials is: numbered
//! blocks and named B+ trees of them. `FACETKEYS` maps an asset's name to the
//! attributes that pick its renditions, `RENDITIONS` maps a rendition's full
//! key to a CoreUI rendition (`CTSI` header, then its payload), and `KEYFORMAT`
//! says which attribute each slot of a key is. The BOM layer is big-endian;
//! CoreUI's own structures are little-endian.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The rendition-key attributes looked at here, CoreUI's numbering.
mod attribute {
    pub const APPEARANCE: u32 = 7;
    pub const IDENTIFIER: u32 = 17;
}

/// `CTSI` layouts.
const LAYOUT_COLOR: u16 = 1009;

pub struct Catalog {
    keyformat: Vec<u32>,
    /// Name to the attribute pairs of its facet.
    facets: HashMap<String, Vec<(u16, u16)>>,
    /// Each rendition's key, in `keyformat` order, and its bytes.
    renditions: Vec<(Vec<u16>, Vec<u8>)>,
    /// Appearance name to the value the appearance attribute takes for it.
    appearances: HashMap<String, u16>,
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn le32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn le16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn le64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

struct Bom<'a> {
    data: &'a [u8],
    blocks: Vec<(usize, usize)>,
    vars: HashMap<String, u32>,
}

impl<'a> Bom<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if data.get(..8)? != b"BOMStore" {
            return None;
        }
        let index = be32(data, 16)? as usize;
        let vars_at = be32(data, 24)? as usize;
        let count = be32(data, index)? as usize;
        let blocks = (0..count)
            .map(|i| {
                let at = index + 4 + i * 8;
                Some((be32(data, at)? as usize, be32(data, at + 4)? as usize))
            })
            .collect::<Option<Vec<_>>>()?;
        let mut vars = HashMap::new();
        let mut at = vars_at + 4;
        for _ in 0..be32(data, vars_at)? {
            let block = be32(data, at)?;
            let length = *data.get(at + 4)? as usize;
            let name = data.get(at + 5..at + 5 + length)?;
            vars.insert(String::from_utf8_lossy(name).into_owned(), block);
            at += 5 + length;
        }
        Some(Self { data, blocks, vars })
    }

    fn block(&self, index: u32) -> Option<&'a [u8]> {
        let (at, length) = *self.blocks.get(index as usize)?;
        self.data.get(at..at + length)
    }

    /// Every key and value of the tree `name`, in order.
    fn tree(&self, name: &str) -> Option<Vec<(&'a [u8], &'a [u8])>> {
        let tree = self.block(*self.vars.get(name)?)?;
        if tree.get(..4)? != b"tree" {
            return None;
        }
        let mut node = self.block(be32(tree, 8)?)?;
        // Down the leftmost branch to the first leaf.
        let mut depth = 0;
        while be16(node, 0)? == 0 {
            node = self.block(be32(node, 12)?)?;
            depth += 1;
            if depth > 64 {
                return None;
            }
        }
        let mut out = Vec::new();
        let mut seen = 0;
        loop {
            let count = be16(node, 2)? as usize;
            for i in 0..count {
                let value = be32(node, 12 + i * 8)?;
                let key = be32(node, 16 + i * 8)?;
                out.push((self.block(key)?, self.block(value)?));
            }
            let forward = be32(node, 4)?;
            seen += 1;
            if forward == 0 || seen > self.blocks.len() {
                return Some(out);
            }
            node = self.block(forward)?;
        }
    }
}

impl Catalog {
    pub fn parse(data: &[u8]) -> Option<Catalog> {
        let bom = Bom::parse(data)?;
        let format = bom.block(*bom.vars.get("KEYFORMAT")?)?;
        let count = le32(format, 8)? as usize;
        let keyformat = (0..count)
            .map(|i| le32(format, 12 + i * 4))
            .collect::<Option<Vec<_>>>()?;
        let mut facets = HashMap::new();
        for (key, value) in bom.tree("FACETKEYS")? {
            let pairs = le16(value, 4)? as usize;
            let attributes = (0..pairs)
                .map(|i| Some((le16(value, 6 + i * 4)?, le16(value, 8 + i * 4)?)))
                .collect::<Option<Vec<_>>>()?;
            let name = String::from_utf8_lossy(key)
                .trim_end_matches('\0')
                .to_owned();
            facets.insert(name, attributes);
        }
        let renditions = bom
            .tree("RENDITIONS")?
            .into_iter()
            .map(|(key, value)| {
                let key = (0..count).map(|i| le16(key, i * 2).unwrap_or(0)).collect();
                (key, value.to_vec())
            })
            .collect();
        let appearances = bom
            .tree("APPEARANCEKEYS")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(key, value)| {
                let name = String::from_utf8_lossy(key)
                    .trim_end_matches('\0')
                    .to_owned();
                Some((name, le16(value, 0)?))
            })
            .collect();
        Some(Catalog {
            keyformat,
            facets,
            renditions,
            appearances,
        })
    }

    fn slot(&self, attribute: u32) -> Option<usize> {
        self.keyformat.iter().position(|a| *a == attribute)
    }

    /// The renditions of the asset `name`.
    fn renditions_of(&self, name: &str) -> Vec<(&[u16], &[u8])> {
        let Some(facet) = self.facets.get(name) else {
            return Vec::new();
        };
        let identifier = facet
            .iter()
            .find(|(a, _)| *a as u32 == attribute::IDENTIFIER)
            .map(|(_, v)| *v);
        let Some((slot, identifier)) = self.slot(attribute::IDENTIFIER).zip(identifier) else {
            return Vec::new();
        };
        self.renditions
            .iter()
            .filter(|(key, _)| key.get(slot) == Some(&identifier))
            .map(|(key, data)| (key.as_slice(), data.as_slice()))
            .collect()
    }

    /// The value the appearance attribute has for a dark appearance, if the
    /// catalog has one.
    fn dark_appearance(&self) -> Option<u16> {
        self.appearances
            .iter()
            .find(|(name, _)| name.contains("Dark") && !name.contains("HighContrast"))
            .map(|(_, v)| *v)
    }

    /// The colour asset `name`, as RGBA, for a dark appearance or not.
    ///
    /// The components are taken as sRGB whatever space the asset names.
    // FIXME (ACCURACY): a Display P3 or extended-range colour is drawn as if
    // its components were sRGB's.
    pub fn color(&self, name: &str, dark: bool) -> Option<[f64; 4]> {
        let renditions = self.renditions_of(name);
        let appearance = self.slot(attribute::APPEARANCE);
        let wanted = match dark {
            true => self.dark_appearance().unwrap_or(0),
            false => 0,
        };
        let pick = |value: u16| {
            renditions.iter().find(|(key, data)| {
                appearance.is_none_or(|slot| key.get(slot).copied().unwrap_or(0) == value)
                    && le16(data, 36) == Some(LAYOUT_COLOR)
            })
        };
        let (_, data) = pick(wanted).or_else(|| pick(0))?;
        color_payload(data)
    }
}

/// The components of a `COLR` rendition: `CTSI` header, then the payload.
fn color_payload(rendition: &[u8]) -> Option<[f64; 4]> {
    if rendition.get(..4)? != b"ISTC" {
        return None;
    }
    let tlv = le32(rendition, 168)? as usize;
    let payload = rendition.get(184 + tlv..)?;
    if payload.get(..4)? != b"RLOC" {
        return None;
    }
    let count = le32(payload, 12)? as usize;
    let component = |i: usize| le64(payload, 16 + i * 8).map(f64::from_bits);
    match count {
        4 => Some([component(0)?, component(1)?, component(2)?, component(3)?]),
        3 => Some([component(0)?, component(1)?, component(2)?, 1.0]),
        2 => {
            let gray = component(0)?;
            Some([gray, gray, gray, component(1)?])
        }
        _ => None,
    }
}

/// The catalog of the bundle at `root`, read once.
pub fn of_bundle(root: &Path) -> Option<Arc<Catalog>> {
    static LOADED: Mutex<Option<HashMap<PathBuf, Option<Arc<Catalog>>>>> = Mutex::new(None);
    let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    let loaded = loaded.get_or_insert_default();
    if let Some(catalog) = loaded.get(root) {
        return catalog.clone();
    }
    let layout = foundation::bundle::Layout::at(root);
    let path = layout.and_then(|l| l.resource("Assets", "car"));
    let catalog = path.and_then(|path| {
        let data = std::fs::read(&path).ok()?;
        let catalog = Catalog::parse(&data);
        if catalog.is_none() {
            eprintln!(
                "rosemary: appkit: {}: not an asset catalog this runtime can read",
                path.display()
            );
        }
        catalog.map(Arc::new)
    });
    loaded.insert(root.to_owned(), catalog.clone());
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    /// A store holding one colour asset, laid out as `actool` lays one out.
    fn store(components: &[f64]) -> Vec<u8> {
        let mut blocks: Vec<Vec<u8>> = vec![Vec::new()];
        let mut add = |b: Vec<u8>| {
            blocks.push(b);
            blocks.len() as u32 - 1
        };
        let mut keyformat = b"tmfk".to_vec();
        keyformat.extend(0u32.to_le_bytes());
        keyformat.extend(2u32.to_le_bytes());
        keyformat.extend(attribute::APPEARANCE.to_le_bytes());
        keyformat.extend(attribute::IDENTIFIER.to_le_bytes());
        let keyformat = add(keyformat);

        let mut facet = vec![0u8; 4];
        facet.extend(1u16.to_le_bytes());
        facet.extend((attribute::IDENTIFIER as u16).to_le_bytes());
        facet.extend(42u16.to_le_bytes());
        let facet_value = add(facet);
        let facet_key = add(b"Tint".to_vec());

        let mut rendition = b"ISTC".to_vec();
        rendition.resize(36, 0);
        rendition.extend(LAYOUT_COLOR.to_le_bytes());
        rendition.resize(184, 0);
        rendition.extend(b"RLOC");
        rendition.extend(1u32.to_le_bytes());
        rendition.extend(0u32.to_le_bytes());
        rendition.extend((components.len() as u32).to_le_bytes());
        for c in components {
            rendition.extend(c.to_le_bytes());
        }
        let rendition_value = add(rendition);
        let mut key = 0u16.to_le_bytes().to_vec();
        key.extend(42u16.to_le_bytes());
        let rendition_key = add(key);

        // A tree of one leaf holding one pair.
        let mut tree = |key: u32, value: u32| {
            let mut leaf = 1u16.to_be_bytes().to_vec();
            leaf.extend(1u16.to_be_bytes());
            leaf.extend(be(0));
            leaf.extend(be(0));
            leaf.extend(be(value));
            leaf.extend(be(key));
            let leaf = add(leaf);
            let mut head = b"tree".to_vec();
            head.extend(be(1));
            head.extend(be(leaf));
            head.extend(be(4096));
            head.extend(be(1));
            head.push(0);
            add(head)
        };
        let facets = tree(facet_key, facet_value);
        let renditions = tree(rendition_key, rendition_value);

        let mut out = b"BOMStore".to_vec();
        out.extend(be(1));
        out.resize(512, 0);
        let mut addresses = Vec::new();
        for b in &blocks {
            addresses.push((out.len() as u32, b.len() as u32));
            out.extend(b);
        }
        let index = out.len() as u32;
        out.extend(be(addresses.len() as u32));
        for (at, len) in &addresses {
            out.extend(be(*at));
            out.extend(be(*len));
        }
        let vars = out.len() as u32;
        let names = [
            ("KEYFORMAT", keyformat),
            ("FACETKEYS", facets),
            ("RENDITIONS", renditions),
        ];
        out.extend(be(names.len() as u32));
        for (name, block) in names {
            out.extend(be(block));
            out.push(name.len() as u8);
            out.extend(name.as_bytes());
        }
        out[12..16].copy_from_slice(&be(addresses.len() as u32));
        out[16..20].copy_from_slice(&be(index));
        out[24..28].copy_from_slice(&be(vars));
        out
    }

    #[test]
    fn reads_a_colour_asset() {
        let catalog = Catalog::parse(&store(&[0.25, 0.5, 0.75, 1.0])).expect("a catalog");
        assert_eq!(catalog.color("Tint", false), Some([0.25, 0.5, 0.75, 1.0]));
        assert_eq!(catalog.color("Tint", true), Some([0.25, 0.5, 0.75, 1.0]));
        assert_eq!(catalog.color("Missing", false), None);
    }

    #[test]
    fn a_grey_colour_asset_has_equal_components() {
        let catalog = Catalog::parse(&store(&[0.5, 0.25])).expect("a catalog");
        assert_eq!(catalog.color("Tint", false), Some([0.5, 0.5, 0.5, 0.25]));
    }

    #[test]
    fn rejects_what_is_not_a_bom_store() {
        assert!(Catalog::parse(b"not a catalog").is_none());
    }
}
