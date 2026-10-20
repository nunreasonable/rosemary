use super::*;
use crate::reflection::{Member, ShaderType};

fn number(items: &[Item<'_>], key: &str) -> Option<u32> {
    match fields::keyed(items, key).first()? {
        Item::Number(n) => u32::try_from(*n).ok(),
        _ => None,
    }
}
fn type_node(items: &[Item<'_>]) -> Option<usize> {
    match fields::keyed(items, "air.struct_type_info").first()? {
        Item::Node(n) => Some(*n),
        _ => None,
    }
}

impl Metadata {
    pub(super) fn buffer_reflection(
        &self,
        items: &[Item<'_>],
        values: &ValueTable,
        types: &[DecodedType],
    ) -> Option<ShaderType> {
        let size = number(items, "air.arg_type_size")?;
        let align = number(items, "air.arg_type_align_size")?;
        if align == 0 || !align.is_power_of_two() {
            return None;
        }
        let mut ty = if let Some(node) = type_node(items) {
            self.reflected_struct(node, size, values, types, 0)?
        } else {
            let Item::Text(name) = fields::keyed(items, "air.arg_type_name").first()? else {
                return None;
            };
            named_type(name)?
        };
        if ty.size != size {
            return None;
        }
        ty.alignment = align;
        Some(ty)
    }

    fn reflected_struct(
        &self,
        node: usize,
        size: u32,
        values: &ValueTable,
        types: &[DecodedType],
        depth: usize,
    ) -> Option<ShaderType> {
        if depth > 64 {
            return None;
        }
        let items = self.items(node, values, types).ok()?;
        let mut rest = items.as_slice();
        let mut members = Vec::new();
        let mut alignment = 1;
        while !rest.is_empty() {
            let nested =
                if let [Item::Text("air.struct_type_info"), Item::Node(n), tail @ ..] = rest {
                    rest = tail;
                    Some(*n)
                } else {
                    None
                };
            let [
                Item::Number(offset),
                Item::Number(element_size),
                Item::Number(count),
                Item::Text(type_name),
                Item::Text(name),
                tail @ ..,
            ] = rest
            else {
                return None;
            };
            rest = tail;
            let offset = u32::try_from(*offset).ok()?;
            let element_size = u32::try_from(*element_size).ok()?;
            let count = u32::try_from(*count).ok()?;
            let mut ty = if let Some(n) = nested {
                self.reflected_struct(n, element_size, values, types, depth + 1)?
            } else {
                named_type(type_name)?
            };
            if ty.size != element_size {
                return None;
            }
            alignment = alignment.max(ty.alignment);
            if count != 0 {
                ty = ShaderType {
                    data_type: 2,
                    size: element_size.checked_mul(count)?,
                    alignment: ty.alignment,
                    members: vec![],
                    element: Some(Box::new(ty)),
                    length: count,
                    stride: element_size,
                };
            }
            if offset.checked_add(ty.size)? > size {
                return None;
            }
            members.push(Member {
                name: (*name).to_owned(),
                offset,
                ty,
            });
        }
        Some(ShaderType {
            data_type: 1,
            size,
            alignment,
            members,
            element: None,
            length: 0,
            stride: 0,
        })
    }
}

fn named_type(name: &str) -> Option<ShaderType> {
    let packed = name.starts_with("packed_");
    let name = name.strip_prefix("packed_").unwrap_or(name);
    let split = name
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(name.len());
    let (base, shape) = name.split_at(split);
    let scalar = match base {
        "float" => Type::Float { bits: 32 },
        "half" => Type::Float { bits: 16 },
        "bool" => Type::Bool,
        "int" => Type::Int {
            bits: 32,
            signed: true,
        },
        "uint" => Type::Int {
            bits: 32,
            signed: false,
        },
        "short" => Type::Int {
            bits: 16,
            signed: true,
        },
        "ushort" => Type::Int {
            bits: 16,
            signed: false,
        },
        "char" => Type::Int {
            bits: 8,
            signed: true,
        },
        "uchar" => Type::Int {
            bits: 8,
            signed: false,
        },
        "long" => Type::Int {
            bits: 64,
            signed: true,
        },
        "ulong" => Type::Int {
            bits: 64,
            signed: false,
        },
        _ => return None,
    };
    let ty = if shape.is_empty() {
        scalar.clone()
    } else if let Some((columns, rows)) = shape.split_once('x') {
        Type::Matrix {
            column: Box::new(Type::Vector {
                element: Box::new(scalar.clone()),
                lanes: rows.parse().ok()?,
            }),
            columns: columns.parse().ok()?,
        }
    } else {
        Type::Vector {
            element: Box::new(scalar.clone()),
            lanes: shape.parse().ok()?,
        }
    };
    let mut reflected = crate::reflection::shader_type(&ty)?;
    if packed {
        let Type::Vector { lanes, .. } = ty else {
            return None;
        };
        let scalar = crate::reflection::shader_type(&scalar)?;
        reflected.size = scalar.size * u32::from(lanes);
        reflected.alignment = scalar.alignment;
    }
    Some(reflected)
}
