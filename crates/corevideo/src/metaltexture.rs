//! `CVMetalTextureCache`: Metal textures made from pixel buffers, one plane
//! each.
//!
//! The texture is made with ordinary messages to the guest's `MTLDevice`,
//! so this crate needs nothing of the Metal implementation but its classes.
//!
//! FIXME (ACCURACY): the texture is a copy of the plane taken when it is
//! made, where Apple's aliases the IOSurface. A decoded frame is never written
//! after it is handed out, so on that path nothing can tell.

use std::ffi::c_void;
use std::sync::OnceLock;

use corefoundation::base::{CFTypeID, CFTypeRef};
use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::buffer;
use crate::pixelbuffer;

/// `MTLPixelFormat` values a plane can become.
pub const R8_UNORM: usize = 10;
pub const RG8_UNORM: usize = 30;
pub const RGBA8_UNORM: usize = 70;
pub const RGBA8_UNORM_SRGB: usize = 71;
pub const BGRA8_UNORM: usize = 80;
pub const BGRA8_UNORM_SRGB: usize = 81;

/// The bytes per pixel of a Metal format this cache can fill, or `None`.
fn bytes_per_pixel(pixel_format: usize) -> Option<usize> {
    match pixel_format {
        R8_UNORM => Some(1),
        RG8_UNORM => Some(2),
        RGBA8_UNORM | RGBA8_UNORM_SRGB | BGRA8_UNORM | BGRA8_UNORM_SRGB => Some(4),
        _ => None,
    }
}

/// `MTLRegion`: an origin and a size, three `NSUInteger` each. Forty-eight
/// bytes, so it travels on the stack.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub x: usize,
    pub y: usize,
    pub z: usize,
    pub width: usize,
    pub height: usize,
    pub depth: usize,
}

struct CacheState {
    /// The retained `id<MTLDevice>`.
    device: usize,
}

impl Drop for CacheState {
    fn drop(&mut self) {
        objc::arc::release(self.device as Id);
    }
}

#[repr(C)]
struct Cache {
    isa: *const c_void,
    state: CacheState,
}

struct TextureState {
    /// The retained `id<MTLTexture>`.
    texture: usize,
    /// The retained pixel buffer it was made from.
    source: CFTypeRef,
}

impl Drop for TextureState {
    fn drop(&mut self) {
        objc::arc::release(self.texture as Id);
        buffer::release(self.source);
    }
}

fn cache_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class =
            corefoundation::base::register(c"CVMetalTextureCache", None, size_of::<Cache>(), true);
        unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
            // SAFETY: `-dealloc` of a cache, whose body is `Cache`, written
            // once when it was made and dropped only here.
            unsafe { std::ptr::drop_in_place(&mut (*(receiver as *mut Cache)).state) };
            objc::arc::finalize(receiver);
        }
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

fn texture_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        objc::class::ptr(buffer::register_subclass::<TextureState>(c"CVMetalTexture")) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

fn cache<'a>(object: CFTypeRef) -> Option<&'a CacheState> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_cache =
        !object.is_null() && unsafe { corefoundation::base::is_kind_of(object, cache_class()) };
    // SAFETY: checked to be a cache, whose body is `Cache`.
    is_cache.then(|| unsafe { &(*(object as *const Cache)).state })
}

/// `CVReturn CVMetalTextureCacheCreate(CFAllocatorRef, CFDictionaryRef
/// cacheAttributes, id<MTLDevice>, CFDictionaryRef textureAttributes,
/// CVMetalTextureCacheRef *)`.
pub unsafe extern "C" fn CVMetalTextureCacheCreate(
    _allocator: CFTypeRef,
    _cache_attributes: CFTypeRef,
    device: Id,
    _texture_attributes: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() || device.is_null() {
        return pixelbuffer::INVALID_ARGUMENT;
    }
    // SAFETY: `Cache` is the registered layout.
    let made = unsafe { corefoundation::base::allocate::<Cache>(cache_class()) };
    // SAFETY: a fresh, zeroed allocation whose state is written once here.
    unsafe {
        std::ptr::write(
            &mut (*made).state,
            CacheState {
                device: objc::arc::retain(device) as usize,
            },
        );
        out.write(made as CFTypeRef);
    }
    pixelbuffer::SUCCESS
}

fn refuse(what: &str) -> i32 {
    eprintln!("rosemary: CVMetalTextureCacheCreateTextureFromImage: {what}; no texture is made");
    pixelbuffer::INVALID_ARGUMENT
}

/// A new texture holding `bytes`, from `device`: +1 retained, or null.
fn new_texture(
    device: Id,
    pixel_format: usize,
    width: usize,
    height: usize,
    bytes: *const u8,
    bytes_per_row: usize,
) -> Id {
    let Some(descriptor_class) = objc::class::lookup(c"MTLTextureDescriptor") else {
        return std::ptr::null_mut();
    };
    let pool = objc::arc::pool_push();
    // SAFETY: the selectors' own prototypes, sent to Metal's descriptor class
    // and the guest's device, and a region inside the plane at `bytes`.
    let texture = unsafe {
        let descriptor_for: unsafe extern "C" fn(Id, Sel, usize, usize, usize, u8) -> Id =
            std::mem::transmute(objc::msg::entry());
        let descriptor = descriptor_for(
            objc::class::ptr(descriptor_class) as Id,
            objc::sel::get("texture2DDescriptorWithPixelFormat:width:height:mipmapped:"),
            pixel_format,
            width,
            height,
            0,
        );
        let new_texture: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            std::mem::transmute(objc::msg::entry());
        let texture = match descriptor.is_null() {
            true => std::ptr::null_mut(),
            false => new_texture(
                device,
                objc::sel::get("newTextureWithDescriptor:"),
                descriptor,
            ),
        };
        if !texture.is_null() {
            let replace: unsafe extern "C" fn(Id, Sel, Region, usize, *const u8, usize) =
                std::mem::transmute(objc::msg::entry());
            replace(
                texture,
                objc::sel::get("replaceRegion:mipmapLevel:withBytes:bytesPerRow:"),
                Region {
                    width,
                    height,
                    depth: 1,
                    ..Region::default()
                },
                0,
                bytes,
                bytes_per_row,
            );
        }
        texture
    };
    objc::arc::pool_pop(pool);
    texture
}

/// `CVReturn CVMetalTextureCacheCreateTextureFromImage(CFAllocatorRef,
/// CVMetalTextureCacheRef, CVImageBufferRef, CFDictionaryRef
/// textureAttributes, MTLPixelFormat, size_t width, size_t height,
/// size_t planeIndex, CVMetalTextureRef *)`.
///
/// The format has to match the plane's bytes per element and the size the
/// plane's size, since nothing here converts or scales.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CVMetalTextureCacheCreateTextureFromImage(
    _allocator: CFTypeRef,
    texture_cache: CFTypeRef,
    image: CFTypeRef,
    _texture_attributes: CFTypeRef,
    pixel_format: usize,
    width: usize,
    height: usize,
    plane_index: usize,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return pixelbuffer::INVALID_ARGUMENT;
    }
    // SAFETY: the caller's out-parameter.
    unsafe { out.write(std::ptr::null()) };
    let Some(cache) = cache(texture_cache) else {
        return refuse("the cache is not a CVMetalTextureCache");
    };
    // SAFETY: each is null-safe and checks what it is given.
    let (planes, base, plane_width, plane_height, bytes_per_row) = unsafe {
        let planes = pixelbuffer::CVPixelBufferGetPlaneCount(image);
        match planes {
            0 => (
                planes,
                pixelbuffer::CVPixelBufferGetBaseAddress(image),
                pixelbuffer::CVPixelBufferGetWidth(image),
                pixelbuffer::CVPixelBufferGetHeight(image),
                pixelbuffer::CVPixelBufferGetBytesPerRow(image),
            ),
            _ => (
                planes,
                pixelbuffer::CVPixelBufferGetBaseAddressOfPlane(image, plane_index),
                pixelbuffer::CVPixelBufferGetWidthOfPlane(image, plane_index),
                pixelbuffer::CVPixelBufferGetHeightOfPlane(image, plane_index),
                pixelbuffer::CVPixelBufferGetBytesPerRowOfPlane(image, plane_index),
            ),
        }
    };
    if !pixelbuffer::is_pixel_buffer(image) {
        return refuse("the image is not a CVPixelBuffer");
    }
    if (planes == 0 && plane_index != 0) || (planes != 0 && plane_index >= planes) {
        return refuse(&format!(
            "plane {plane_index} of a buffer with {} planes",
            planes.max(1)
        ));
    }
    if (width, height) != (plane_width, plane_height) {
        return refuse(&format!(
            "{width}x{height} for a {plane_width}x{plane_height} plane; nothing scales"
        ));
    }
    let element = crate::format::planes(
        // SAFETY: checked to be a pixel buffer above.
        unsafe { pixelbuffer::CVPixelBufferGetPixelFormatType(image) },
    )
    .and_then(|shapes| shapes.get(plane_index))
    .map(|shape| shape.bytes_per_element);
    if bytes_per_pixel(pixel_format).is_none() || bytes_per_pixel(pixel_format) != element {
        return refuse(&format!(
            "MTLPixelFormat {pixel_format} for a plane of {} bytes per element",
            element.unwrap_or(0)
        ));
    }
    let texture = new_texture(
        cache.device as Id,
        pixel_format,
        width,
        height,
        base.cast(),
        bytes_per_row,
    );
    if texture.is_null() {
        eprintln!(
            "rosemary: CVMetalTextureCacheCreateTextureFromImage: the device made no texture"
        );
        return pixelbuffer::ERROR;
    }
    let made = buffer::make(
        texture_class(),
        TextureState {
            texture: texture as usize,
            source: buffer::retain(image),
        },
    );
    // SAFETY: the caller's out-parameter.
    unsafe { out.write(made) };
    pixelbuffer::SUCCESS
}

/// `id<MTLTexture> CVMetalTextureGetTexture(CVMetalTextureRef)`: not retained.
pub unsafe extern "C" fn CVMetalTextureGetTexture(texture: CFTypeRef) -> Id {
    buffer::state::<TextureState>(texture, texture_class())
        .map_or(std::ptr::null_mut(), |s| s.texture as Id)
}

/// `void CVMetalTextureCacheFlush(CVMetalTextureCacheRef, CVOptionFlags)`:
/// nothing is cached.
pub unsafe extern "C" fn CVMetalTextureCacheFlush(_cache: CFTypeRef, _options: u64) {}

pub unsafe extern "C" fn CVMetalTextureCacheGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(cache_class())
}

pub unsafe extern "C" fn CVMetalTextureGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(texture_class())
}

exports::exports! {
    functions {
        "CVMetalTextureCacheCreate" => CVMetalTextureCacheCreate,
        "CVMetalTextureCacheCreateTextureFromImage" => CVMetalTextureCacheCreateTextureFromImage,
        "CVMetalTextureGetTexture" => CVMetalTextureGetTexture,
        "CVMetalTextureCacheFlush" => CVMetalTextureCacheFlush,
        "CVMetalTextureCacheGetTypeID" => CVMetalTextureCacheGetTypeID,
        "CVMetalTextureGetTypeID" => CVMetalTextureGetTypeID,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::format;
    use crate::pixelbuffer::tests::make;

    /// A fake Metal object's body: what was asked of it and what it was given.
    #[repr(C)]
    struct Fake {
        isa: *const c_void,
        pixel_format: usize,
        width: usize,
        height: usize,
        /// Rows written by `replaceRegion:`, `width * bpp` bytes each.
        bytes: *mut Vec<u8>,
    }

    fn fake(class: &'static Class, pixel_format: usize, width: usize, height: usize) -> Id {
        // SAFETY: `Fake` is the layout the fake classes were registered with.
        let object = unsafe { corefoundation::base::allocate::<Fake>(class) };
        // SAFETY: a fresh allocation; the vector is leaked with the object.
        unsafe {
            (*object).pixel_format = pixel_format;
            (*object).width = width;
            (*object).height = height;
            (*object).bytes = Box::into_raw(Box::default());
        }
        object as Id
    }

    fn body<'a>(object: Id) -> &'a Fake {
        // SAFETY: one of the fakes, whose body is `Fake`.
        unsafe { &*(object as *const Fake) }
    }

    unsafe extern "C" fn descriptor(
        _class: Id,
        _sel: Sel,
        pixel_format: usize,
        width: usize,
        height: usize,
        _mipmapped: u8,
    ) -> Id {
        let object = fake(classes().0, pixel_format, width, height);
        objc::arc::autorelease(object)
    }

    unsafe extern "C" fn new_texture(_device: Id, _sel: Sel, descriptor: Id) -> Id {
        let d = body(descriptor);
        fake(classes().2, d.pixel_format, d.width, d.height)
    }

    unsafe extern "C" fn replace(
        texture: Id,
        _sel: Sel,
        region: Region,
        level: usize,
        bytes: *const u8,
        bytes_per_row: usize,
    ) {
        let t = body(texture);
        assert_eq!(level, 0);
        assert_eq!(
            region,
            Region {
                width: t.width,
                height: t.height,
                depth: 1,
                ..Region::default()
            }
        );
        let bpp = bytes_per_pixel(t.pixel_format).unwrap();
        // SAFETY: the vector this fake owns, and the caller's rows.
        unsafe {
            let out = &mut *t.bytes;
            for row in 0..region.height {
                out.extend_from_slice(std::slice::from_raw_parts(
                    bytes.add(row * bytes_per_row),
                    region.width * bpp,
                ));
            }
        }
    }

    /// The fake descriptor, device and texture classes, registered once.
    fn classes() -> (&'static Class, &'static Class, &'static Class) {
        static CLASSES: OnceLock<(usize, usize, usize)> = OnceLock::new();
        let (d, v, t) = *CLASSES.get_or_init(|| {
            corefoundation::bootstrap();
            let register =
                |name| corefoundation::base::register(name, None, size_of::<Fake>(), false);
            let d = register(c"MTLTextureDescriptor");
            d.metaclass().add_method(
                objc::sel::get("texture2DDescriptorWithPixelFormat:width:height:mipmapped:"),
                descriptor as *const c_void,
                c"@48@0:8Q16Q24Q32c40",
            );
            let v = register(c"FakeMTLDevice");
            v.add_method(
                objc::sel::get("newTextureWithDescriptor:"),
                new_texture as *const c_void,
                c"@24@0:8@16",
            );
            let t = register(c"FakeMTLTexture");
            t.add_method(
                objc::sel::get("replaceRegion:mipmapLevel:withBytes:bytesPerRow:"),
                replace as *const c_void,
                c"v96@0:8{?=QQQQQQ}16Q64^v72Q80",
            );
            (
                objc::class::ptr(d) as usize,
                objc::class::ptr(v) as usize,
                objc::class::ptr(t) as usize,
            )
        });
        // SAFETY: leaked classes registered above.
        unsafe {
            (
                &*(d as *const Class),
                &*(v as *const Class),
                &*(t as *const Class),
            )
        }
    }

    fn texture_cache() -> CFTypeRef {
        let device = fake(classes().1, 0, 0, 0);
        let mut cache = std::ptr::null();
        // SAFETY: a live device and out-parameter.
        let status = unsafe {
            CVMetalTextureCacheCreate(
                std::ptr::null(),
                std::ptr::null(),
                device,
                std::ptr::null(),
                &mut cache,
            )
        };
        assert_eq!(status, pixelbuffer::SUCCESS);
        objc::arc::release(device);
        cache
    }

    fn texture_from(
        cache: CFTypeRef,
        image: CFTypeRef,
        pixel_format: usize,
        width: usize,
        height: usize,
        plane: usize,
    ) -> (i32, CFTypeRef) {
        let mut out = 1 as CFTypeRef;
        // SAFETY: objects this test made and a live out-parameter.
        let status = unsafe {
            CVMetalTextureCacheCreateTextureFromImage(
                std::ptr::null(),
                cache,
                image,
                std::ptr::null(),
                pixel_format,
                width,
                height,
                plane,
                &mut out,
            )
        };
        (status, out)
    }

    /// A buffer whose plane `p`'s row `y`, byte `x` holds `p * 64 + y + x`.
    fn patterned(format: u32, width: usize, height: usize) -> CFTypeRef {
        let buffer = make(format, width, height, std::ptr::null());
        for (p, (bytes, row)) in pixelbuffer::planes_mut(buffer)
            .unwrap()
            .into_iter()
            .enumerate()
        {
            for (y, line) in bytes.chunks_mut(row).enumerate() {
                for (x, byte) in line.iter_mut().enumerate() {
                    *byte = (p * 64 + y + x) as u8;
                }
            }
        }
        buffer
    }

    fn expected(plane: usize, width: usize, height: usize) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (plane * 64 + y + x) as u8))
            .collect()
    }

    fn texture_of(made: CFTypeRef) -> &'static Fake {
        // SAFETY: a CVMetalTexture this test made.
        let texture = unsafe { CVMetalTextureGetTexture(made) };
        assert!(!texture.is_null());
        body(texture)
    }

    fn written(fake: &Fake) -> &Vec<u8> {
        // SAFETY: the fake's own vector.
        unsafe { &*fake.bytes }
    }

    #[test]
    fn luma_plane_becomes_r8_texture_with_its_bytes() {
        let cache = texture_cache();
        let image = patterned(format::NV12_VIDEO, 6, 4);
        let (status, made) = texture_from(cache, image, R8_UNORM, 6, 4, 0);
        assert_eq!(status, pixelbuffer::SUCCESS);
        // SAFETY: objects this test made.
        unsafe {
            assert_eq!(
                corefoundation::api::CFGetTypeID(made),
                CVMetalTextureGetTypeID()
            );
            assert_eq!(
                corefoundation::api::CFGetTypeID(cache),
                CVMetalTextureCacheGetTypeID()
            );
        }
        let texture = texture_of(made);
        assert_eq!(
            (texture.pixel_format, texture.width, texture.height),
            (R8_UNORM, 6, 4)
        );
        assert_eq!(written(texture), &expected(0, 6, 4));
        // The texture holds the buffer.
        buffer::release(image);
        assert!(buffer::is_buffer(made));
        buffer::release(made);
        buffer::release(cache);
    }

    #[test]
    fn nv12_chroma_plane_is_rg8_at_half_size() {
        let cache = texture_cache();
        let image = patterned(format::NV12_FULL, 7, 5);
        let (status, made) = texture_from(cache, image, RG8_UNORM, 4, 3, 1);
        assert_eq!(status, pixelbuffer::SUCCESS);
        let texture = texture_of(made);
        assert_eq!(
            (texture.pixel_format, texture.width, texture.height),
            (RG8_UNORM, 4, 3)
        );
        assert_eq!(written(texture), &expected(1, 8, 3));
        buffer::release(made);
        buffer::release(image);
        buffer::release(cache);
    }

    #[test]
    fn y420_cr_plane_is_plane_two() {
        let cache = texture_cache();
        let image = patterned(format::Y420, 8, 8);
        let (status, made) = texture_from(cache, image, R8_UNORM, 4, 4, 2);
        assert_eq!(status, pixelbuffer::SUCCESS);
        assert_eq!(written(texture_of(made)), &expected(2, 4, 4));
        buffer::release(made);
        buffer::release(image);
        buffer::release(cache);
    }

    #[test]
    fn bgra_plane_zero_is_bgra8() {
        let cache = texture_cache();
        let image = patterned(format::BGRA, 3, 2);
        let (status, made) = texture_from(cache, image, BGRA8_UNORM, 3, 2, 0);
        assert_eq!(status, pixelbuffer::SUCCESS);
        let texture = texture_of(made);
        assert_eq!(texture.pixel_format, BGRA8_UNORM);
        assert_eq!(written(texture), &expected(0, 12, 2));
        buffer::release(made);
        buffer::release(image);
        buffer::release(cache);
    }

    #[test]
    fn wrong_size_format_or_plane_is_refused() {
        let cache = texture_cache();
        let nv12 = patterned(format::NV12_VIDEO, 8, 8);
        let bgra = patterned(format::BGRA, 8, 8);
        let refused = pixelbuffer::INVALID_ARGUMENT;
        for (image, pixel_format, width, height, plane) in [
            (nv12, R8_UNORM, 8, 4, 0),
            (nv12, RG8_UNORM, 8, 8, 0),
            (nv12, R8_UNORM, 4, 4, 1),
            (nv12, RG8_UNORM, 4, 4, 2),
            (nv12, 999, 8, 8, 0),
            (bgra, BGRA8_UNORM, 8, 8, 1),
            (bgra, R8_UNORM, 8, 8, 0),
        ] {
            let (status, made) = texture_from(cache, image, pixel_format, width, height, plane);
            assert_eq!(
                status, refused,
                "format {pixel_format} {width}x{height} plane {plane}"
            );
            assert!(made.is_null());
        }
        assert_eq!(
            texture_from(std::ptr::null(), nv12, R8_UNORM, 8, 8, 0).0,
            refused
        );
        assert_eq!(texture_from(cache, cache, R8_UNORM, 8, 8, 0).0, refused);
        let mut out = std::ptr::null();
        // SAFETY: a live out-parameter.
        let status = unsafe {
            CVMetalTextureCacheCreate(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(status, refused, "a cache needs a device");
        buffer::release(nv12);
        buffer::release(bgra);
        buffer::release(cache);
    }
}
