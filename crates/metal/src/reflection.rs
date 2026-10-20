//! Owned Objective-C shader reflection trees.
use crate::object::{self, methods};
use metal_translate::reflection::{Argument, ShaderType};
use objc::{object::Id, sel::Sel};
use std::collections::BTreeMap;
use std::ffi::CStr;

#[derive(Default)]
struct Node {
    objects: BTreeMap<&'static str, Id>,
    numbers: BTreeMap<&'static str, u64>,
}
impl Drop for Node {
    fn drop(&mut self) {
        for id in self.objects.values() {
            objc::arc::release(*id);
        }
    }
}
impl Node {
    fn number(mut self, key: &'static str, value: impl Into<u64>) -> Self {
        self.numbers.insert(key, value.into());
        self
    }
    fn object(mut self, key: &'static str, value: Id) -> Self {
        self.objects.insert(key, value);
        self
    }
    fn make(self, class: &CStr) -> Id {
        object::make(class, self)
    }
}
unsafe extern "C" fn get_object(receiver: Id, sel: Sel) -> Id {
    // SAFETY: these getters are registered only for Node instances.
    let node = unsafe { object::body::<Node>(receiver) };
    let name = unsafe { CStr::from_ptr(sel) }.to_str().unwrap_or("");
    node.objects
        .get(name)
        .copied()
        .unwrap_or(std::ptr::null_mut())
}
unsafe extern "C" fn get_number(receiver: Id, sel: Sel) -> u64 {
    let node = unsafe { object::body::<Node>(receiver) };
    let name = unsafe { CStr::from_ptr(sel) }.to_str().unwrap_or("");
    node.numbers.get(name).copied().unwrap_or(0)
}
unsafe extern "C" fn get_bool(receiver: Id, sel: Sel) -> bool {
    unsafe { get_number(receiver, sel) != 0 }
}
unsafe extern "C" fn dealloc(receiver: Id, _: Sel) {
    // SAFETY: every reflection object is made with an initialized Node.
    unsafe { std::ptr::drop_in_place(object::body::<Node>(receiver)) };
    objc::arc::finalize(receiver);
}
fn array(objects: Vec<Id>) -> Id {
    let refs: Vec<_> = objects
        .iter()
        .map(|id| *id as corefoundation::base::CFTypeRef)
        .collect();
    let array = corefoundation::collections::new_array(
        &refs,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    ) as Id;
    for object in objects {
        objc::arc::release(object);
    }
    array
}
unsafe extern "C" fn member_by_name(receiver: Id, _: Sel, name: Id) -> Id {
    let members = unsafe { get_object(receiver, objc::sel::get("members")) };
    let count = unsafe { corefoundation::api::CFArrayGetCount(members.cast()) };
    for index in 0..count {
        let member =
            unsafe { corefoundation::api::CFArrayGetValueAtIndex(members.cast(), index) } as Id;
        let member_name = unsafe { get_object(member, objc::sel::get("name")) };
        if unsafe { corefoundation::api::CFEqual(member_name.cast(), name.cast()) } != 0 {
            return member;
        }
    }
    std::ptr::null_mut()
}
fn type_object(ty: &ShaderType) -> Id {
    match ty.data_type {
        1 => {
            let members = ty
                .members
                .iter()
                .map(|m| {
                    let mut node = Node::default()
                        .object("name", foundation::string::from_str(&m.name))
                        .number("offset", m.offset)
                        .number("dataType", m.ty.data_type);
                    if m.ty.data_type == 1 {
                        node = node.object("structType", type_object(&m.ty));
                    }
                    if m.ty.data_type == 2 {
                        node = node.object("arrayType", type_object(&m.ty));
                    }
                    node.make(c"MTLStructMember")
                })
                .collect();
            Node::default()
                .number("dataType", 1u64)
                .object("members", array(members))
                .make(c"MTLStructType")
        }
        2 => {
            let element = ty
                .element
                .as_ref()
                .expect("array reflection has an element");
            let mut node = Node::default()
                .number("dataType", 2u64)
                .number("arrayLength", ty.length)
                .number("stride", ty.stride)
                .number("elementType", element.data_type);
            if element.data_type == 1 {
                node = node.object("elementStructType", type_object(element));
            }
            if element.data_type == 2 {
                node = node.object("elementArrayType", type_object(element));
            }
            node.make(c"MTLArrayType")
        }
        _ => std::ptr::null_mut(),
    }
}
fn argument(a: &Argument, types: bool) -> Id {
    let mut node = Node::default()
        .object("name", foundation::string::from_str(&a.name))
        .number("index", a.index)
        .number("type", a.kind)
        .number("access", a.access)
        .number("arrayLength", a.count)
        .number("isActive", u64::from(a.active))
        .number("textureType", a.texture_type)
        .number("isDepthTexture", u64::from(a.depth));
    if let Some(ty) = &a.ty {
        if a.kind == 0 {
            node = node
                .number("bufferDataSize", ty.size)
                .number("bufferAlignment", ty.alignment)
                .number("bufferDataType", ty.data_type);
            if types {
                if ty.data_type == 1 {
                    node = node.object("bufferStructType", type_object(ty));
                }
                let mut pointer = Node::default()
                    .number("dataType", 60u64)
                    .number("elementType", ty.data_type)
                    .number("access", a.access)
                    .number("alignment", ty.alignment)
                    .number("dataSize", ty.size);
                if ty.data_type == 1 {
                    pointer = pointer.object("elementStructType", type_object(ty));
                }
                if ty.data_type == 2 {
                    pointer = pointer.object("elementArrayType", type_object(ty));
                }
                node = node.object("bufferPointerType", pointer.make(c"MTLPointerType"));
            }
        } else if a.kind == 2 {
            node = node.number("textureDataType", ty.data_type);
        }
    }
    node.make(c"MTLArgument")
}
pub(crate) fn render(vertex: &[Argument], fragment: &[Argument], options: u64) -> Id {
    let args =
        |args: &[Argument]| array(args.iter().map(|a| argument(a, options & 2 != 0)).collect());
    Node::default()
        .object("vertexArguments", args(vertex))
        .object("fragmentArguments", args(fragment))
        .make(c"MTLRenderPipelineReflection")
}

pub(crate) fn install() {
    for name in [
        c"MTLArgument",
        c"MTLStructMember",
        c"MTLStructType",
        c"MTLArrayType",
        c"MTLPointerType",
        c"MTLRenderPipelineReflection",
    ] {
        let class = object::register::<Node>(name);
        methods!(class, { "dealloc"=>dealloc,c"v16@0:8"; });
        for selector in [
            "name",
            "members",
            "structType",
            "arrayType",
            "elementStructType",
            "elementArrayType",
            "bufferStructType",
            "bufferPointerType",
            "vertexArguments",
            "fragmentArguments",
        ] {
            class.replace_method(
                objc::sel::get(selector),
                get_object as *const std::ffi::c_void,
                c"@16@0:8",
            );
        }
        for selector in [
            "index",
            "type",
            "access",
            "arrayLength",
            "bufferDataSize",
            "bufferAlignment",
            "bufferDataType",
            "textureDataType",
            "textureType",
            "dataType",
            "offset",
            "stride",
            "elementType",
            "alignment",
            "dataSize",
        ] {
            class.replace_method(
                objc::sel::get(selector),
                get_number as *const std::ffi::c_void,
                c"Q16@0:8",
            );
        }
        for selector in ["isActive", "isDepthTexture", "elementIsArgumentBuffer"] {
            class.replace_method(
                objc::sel::get(selector),
                get_bool as *const std::ffi::c_void,
                c"B16@0:8",
            );
        }
    }
    methods!(objc::class::lookup(c"MTLStructType").unwrap(), {
        "memberByName:"=>member_by_name,c"@24@0:8@16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reflection_exposes_owned_struct_members_and_arrays() {
        crate::bootstrap();
        let module=metal_translate::parse_msl("struct Params { float4 colors[2]; float gain; }; kernel void shade(constant Params& params [[buffer(4)]], device float* output [[buffer(1)]]) { output[0] = params.gain; }").unwrap();
        let library =
            metal_translate::emit_spirv(&module, &metal_translate::TargetProfile::vulkan_1_3())
                .unwrap();
        let reflected = render(library.entries[0].arguments.as_ref().unwrap(), &[], 3);
        drop(library);
        unsafe {
            let args = get_object(reflected, objc::sel::get("vertexArguments"));
            let arg = corefoundation::api::CFArrayGetValueAtIndex(args.cast(), 0) as Id;
            assert_eq!(get_number(arg, objc::sel::get("index")), 4);
            assert_eq!(get_number(arg, objc::sel::get("bufferDataSize")), 48);
            let structure = get_object(arg, objc::sel::get("bufferStructType"));
            let name = foundation::string::from_str("colors");
            let member = member_by_name(structure, std::ptr::null(), name);
            objc::arc::release(name);
            assert!(!member.is_null());
            let array = get_object(member, objc::sel::get("arrayType"));
            assert_eq!(get_number(array, objc::sel::get("arrayLength")), 2);
            assert_eq!(get_number(array, objc::sel::get("stride")), 16);
            assert_eq!(get_number(array, objc::sel::get("elementType")), 6);
        }
        objc::arc::release(reflected);
    }
}
