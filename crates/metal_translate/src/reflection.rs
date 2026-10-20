//! Metal-facing types, preserved independently of Vulkan lowering.
use crate::ir::{Access, Resource, ResourceKind, TextureDimension, Type};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    pub name: String,
    pub offset: u32,
    pub ty: ShaderType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShaderType {
    pub data_type: u64,
    pub size: u32,
    pub alignment: u32,
    pub members: Vec<Member>,
    pub element: Option<Box<ShaderType>>,
    pub length: u32,
    pub stride: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Argument {
    pub active: bool,
    pub name: String,
    pub index: u32,
    pub kind: u64,
    pub access: u64,
    pub count: u32,
    pub ty: Option<ShaderType>,
    pub texture_type: u64,
    pub depth: bool,
}

pub fn arguments(resources: &[Resource], function: &crate::ir::Function) -> Option<Vec<Argument>> {
    resources
        .iter()
        .filter(|r| r.inline_sampler.is_none())
        .map(|r| {
            let (kind, ty, texture_type, depth) = match &r.ty {
                Type::Pointer { pointee, .. } if r.kind == ResourceKind::Buffer => {
                    (0, Some(r.reflection.clone().or_else(|| shader_type(pointee))?), 0, false)
                }
                Type::Texture(t) => {
                    let shape = match (t.dimension, t.array, t.multisampled) {
                        (TextureDimension::D1, false, false) => 0,
                        (TextureDimension::D1, true, false) => 1,
                        (TextureDimension::D2, false, false) => 2,
                        (TextureDimension::D2, true, false) => 3,
                        (TextureDimension::D2, false, true) => 4,
                        (TextureDimension::Cube, false, false) => 5,
                        (TextureDimension::Cube, true, false) => 6,
                        (TextureDimension::D3, false, false) => 7,
                        (TextureDimension::D2, true, true) => 8,
                        _ => return None,
                    };
                    (2, Some(shader_type(&t.sampled_scalar())?), shape, t.depth)
                }
                Type::Sampler => (3, None, 0, false),
                _ => return None,
            };
            let parameter = function.parameters.get(r.argument as usize)?;
            let active = function.blocks.iter().any(|block| {
                block.instructions.iter().flat_map(|i| crate::ir::instruction_operands(&i.kind))
                    .chain(crate::ir::terminator_operands(&block.terminator))
                    .any(|operand| matches!(operand, crate::ir::Operand::Value(value) if *value == parameter.value))
            });
            Some(Argument {
                active,
                name: r.name.clone()?,
                index: r.index,
                kind,
                access: match r.access {
                    Access::Read | Access::Sample => 0,
                    Access::ReadWrite => 1,
                    Access::Write => 2,
                },
                count: r.count,
                ty,
                texture_type,
                depth,
            })
        })
        .collect()
}

pub fn shader_type(ty: &Type) -> Option<ShaderType> {
    let mut result = ShaderType {
        data_type: 0,
        size: 0,
        alignment: 1,
        members: vec![],
        element: None,
        length: 0,
        stride: 0,
    };
    match ty {
        Type::Struct(s) => {
            result.data_type = 1;
            result.size = s.size;
            result.alignment = s.alignment;
            result.members = s
                .fields
                .iter()
                .map(|f| {
                    Some(Member {
                        name: f.name.clone()?,
                        offset: f.offset,
                        ty: shader_type(&f.ty)?,
                    })
                })
                .collect::<Option<_>>()?;
        }
        Type::Array { element, count } => {
            let element = shader_type(element)?;
            result.data_type = 2;
            result.alignment = element.alignment;
            result.stride = element.size.checked_add(element.alignment - 1)? / element.alignment
                * element.alignment;
            result.length = *count;
            result.size = result.stride.checked_mul(*count)?;
            result.element = Some(Box::new(element));
        }
        Type::Matrix { column, columns } => {
            let Type::Vector { element, lanes } = column.as_ref() else {
                return None;
            };
            let base = match element.as_ref() {
                Type::Float { bits: 32 } => 7,
                Type::Float { bits: 16 } => 20,
                _ => return None,
            };
            if !(2..=4).contains(columns) || !(2..=4).contains(lanes) {
                return None;
            }
            let column = shader_type(column)?;
            result.data_type = base + u64::from(*columns - 2) * 3 + u64::from(*lanes - 2);
            result.size = column.size.checked_mul(u32::from(*columns))?;
            result.alignment = column.alignment;
        }
        Type::Vector { element, lanes } => {
            if !(2..=4).contains(lanes) {
                return None;
            }
            let scalar = shader_type(element)?;
            result.data_type = scalar.data_type + u64::from(*lanes - 1);
            result.size = scalar.size * u32::from(*lanes).next_power_of_two();
            result.alignment = result.size;
        }
        Type::Bool => {
            result.data_type = 53;
            result.size = 1;
        }
        Type::Float { bits } => {
            result.data_type = match bits {
                32 => 3,
                16 => 16,
                _ => return None,
            };
            result.size = u32::from(*bits) / 8;
            result.alignment = result.size;
        }
        Type::Int { bits, signed } => {
            result.data_type = match (bits, signed) {
                (32, true) => 29,
                (32, false) => 33,
                (16, true) => 37,
                (16, false) => 41,
                (8, true) => 45,
                (8, false) => 49,
                (64, true) => 81,
                (64, false) => 85,
                _ => return None,
            };
            result.size = u32::from(*bits) / 8;
            result.alignment = result.size;
        }
        _ => return None,
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_reflection_preserves_nested_layout_and_resource_indices() {
        let module=crate::parse_msl("struct Inner { float3 direction; float gain; }; struct Params { Inner inner; float4 colors[2]; }; kernel void shade(constant Params& params [[buffer(4)]], device float* output [[buffer(1)]]) { output[0] = params.inner.gain; }").unwrap();
        let library = crate::emit_spirv(&module, &crate::TargetProfile::vulkan_1_3()).unwrap();
        let args = library.entries[0].arguments.as_ref().unwrap();
        let params = args.iter().find(|a| a.name == "params").unwrap();
        assert_eq!(params.index, 4);
        let ty = params.ty.as_ref().unwrap();
        assert_eq!((ty.size, ty.alignment), (64, 16));
        assert_eq!(ty.members[0].name, "inner");
        assert_eq!(ty.members[0].ty.members[1].offset, 16);
        let colors = &ty.members[1];
        assert_eq!(colors.offset, 32);
        assert_eq!((colors.ty.length, colors.ty.stride), (2, 16));
    }
}
