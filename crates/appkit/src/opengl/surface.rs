//! `CGLTexImageIOSurface2D`: a texture whose storage is an IOSurface.
//!
//! On a Mac the texture and the surface are the same memory, so what one
//! process draws into the texture is what another composites from the
//! surface. The host GL cannot adopt foreign memory as texture storage, so
//! the surface is copied in when it is bound and the texture copied back out
//! whenever the guest flushes - the point at which a Mac program hands the
//! surface on.
//!
//! FIXME (ACCURACY): a surface changed by someone else after it was bound is
//! not seen by the texture, and a texture given new storage with
//! `glTexImage2D` after binding is still copied back into the surface.

use std::ffi::{CStr, c_void};
use std::sync::Mutex;

use corefoundation::base::CFTypeRef;

const TEXTURE_2D: u32 = 0x0DE1;
const TEXTURE_RECTANGLE: u32 = 0x84F5;
const TEXTURE_BINDING_2D: u32 = 0x8069;
const TEXTURE_BINDING_RECTANGLE: u32 = 0x84F6;
const UNPACK_ROW_LENGTH: u32 = 0x0CF2;
const UNPACK_ALIGNMENT: u32 = 0x0CF5;
const PACK_ROW_LENGTH: u32 = 0x0D02;
const PACK_ALIGNMENT: u32 = 0x0D05;
const PIXEL_UNPACK_BUFFER: u32 = 0x88EC;
const PIXEL_UNPACK_BUFFER_BINDING: u32 = 0x88EF;
const PIXEL_PACK_BUFFER: u32 = 0x88EB;
const PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;

#[allow(non_snake_case)]
struct Gl {
    GetIntegerv: unsafe extern "C" fn(u32, *mut i32),
    BindTexture: unsafe extern "C" fn(u32, u32),
    BindBuffer: unsafe extern "C" fn(u32, u32),
    PixelStorei: unsafe extern "C" fn(u32, i32),
    TexImage2D: unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    GetTexImage: unsafe extern "C" fn(u32, i32, u32, u32, *mut c_void),
}

fn gl() -> Option<&'static Gl> {
    static GL: std::sync::OnceLock<Option<Gl>> = std::sync::OnceLock::new();
    GL.get_or_init(|| {
        fn get<T>(name: &CStr) -> Option<T> {
            let found = sdl3::video::gl_proc_address(name)?;
            assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
            // SAFETY: the caller names the prototype GL declares for it.
            Some(unsafe { std::mem::transmute_copy(&found) })
        }
        Some(Gl {
            GetIntegerv: get(c"glGetIntegerv")?,
            BindTexture: get(c"glBindTexture")?,
            BindBuffer: get(c"glBindBuffer")?,
            PixelStorei: get(c"glPixelStorei")?,
            TexImage2D: get(c"glTexImage2D")?,
            GetTexImage: get(c"glGetTexImage")?,
        })
    })
    .as_ref()
}

/// A texture backed by a surface: what to copy back, and how.
struct Binding {
    context: usize,
    target: u32,
    texture: u32,
    format: u32,
    kind: u32,
    /// Retained for as long as the binding lasts.
    surface: usize,
}

static BINDINGS: Mutex<Vec<Binding>> = Mutex::new(Vec::new());

fn binding_query(target: u32) -> Option<u32> {
    match target {
        TEXTURE_RECTANGLE => Some(TEXTURE_BINDING_RECTANGLE),
        TEXTURE_2D => Some(TEXTURE_BINDING_2D),
        _ => None,
    }
}

/// Run `f` with the pixel store set for rows `row_length` pixels long and no
/// buffer bound for `buffer`, then put both back.
///
/// # Safety
///
/// A context is current on this thread.
unsafe fn with_store(
    gl: &Gl,
    (row_length, alignment, buffer, buffer_binding): (u32, u32, u32, u32),
    row: i32,
    f: impl FnOnce(),
) {
    let (mut old_row, mut old_alignment, mut old_buffer) = (0, 0, 0);
    // SAFETY: plain state queries and sets on the current context.
    unsafe {
        (gl.GetIntegerv)(row_length, &mut old_row);
        (gl.GetIntegerv)(alignment, &mut old_alignment);
        (gl.GetIntegerv)(buffer_binding, &mut old_buffer);
        (gl.BindBuffer)(buffer, 0);
        (gl.PixelStorei)(row_length, row);
        (gl.PixelStorei)(alignment, 1);
    }
    f();
    // SAFETY: as above.
    unsafe {
        (gl.PixelStorei)(row_length, old_row);
        (gl.PixelStorei)(alignment, old_alignment);
        (gl.BindBuffer)(buffer, old_buffer as u32);
    }
}

/// `CGLError CGLTexImageIOSurface2D(CGLContextObj ctx, GLenum target, GLenum
/// internal_format, GLsizei width, GLsizei height, GLenum format, GLenum type,
/// IOSurfaceRef ioSurface, GLuint plane)`
///
/// # Safety
///
/// `ctx` is a context and current on this thread, `surface` an IOSurface.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CGLTexImageIOSurface2D(
    ctx: super::CGLContextObj,
    target: u32,
    internal_format: u32,
    width: i32,
    height: i32,
    format: u32,
    kind: u32,
    surface: CFTypeRef,
    plane: u32,
) -> super::CGLError {
    let Some(object) = super::handle_in(ctx) else {
        return super::kCGLBadContext;
    };
    let (Some(gl), Some(pixels), Some(query)) =
        (gl(), iosurface::pixels(surface), binding_query(target))
    else {
        return super::kCGLBadValue;
    };
    // FIXME (ACCURACY): only the first plane; a planar YUV surface is refused.
    if plane != 0
        || pixels.bytes_per_element == 0
        || width as usize > pixels.width
        || height as usize > pixels.height
    {
        eprintln!(
            "rosemary: appkit: CGLTexImageIOSurface2D: plane {plane} of a \
             {}x{} surface as {width}x{height} is not supported",
            pixels.width, pixels.height
        );
        return super::kCGLBadValue;
    }
    let row = (pixels.bytes_per_row / pixels.bytes_per_element) as i32;
    let mut texture = 0;
    let unpack = (
        UNPACK_ROW_LENGTH,
        UNPACK_ALIGNMENT,
        PIXEL_UNPACK_BUFFER,
        PIXEL_UNPACK_BUFFER_BINDING,
    );
    // SAFETY: the current context, and the surface's own pixels, which are
    // `bytes_per_row * height` bytes.
    unsafe {
        (gl.GetIntegerv)(query, &mut texture);
        with_store(gl, unpack, row, || {
            (gl.TexImage2D)(
                target,
                0,
                internal_format as i32,
                width,
                height,
                0,
                format,
                kind,
                pixels.data.cast(),
            )
        });
    }
    let texture = texture as u32;
    let mut bindings = BINDINGS.lock().unwrap_or_else(|e| e.into_inner());
    let context = object as usize;
    if let Some(at) = bindings
        .iter()
        .position(|b| b.context == context && b.texture == texture)
    {
        let old = bindings.remove(at);
        // SAFETY: the reference this module took.
        unsafe { corefoundation::api::CFRelease(old.surface as CFTypeRef) };
    }
    // SAFETY: an IOSurface, per the caller.
    unsafe { corefoundation::api::CFRetain(surface) };
    bindings.push(Binding {
        context,
        target,
        texture,
        format,
        kind,
        surface: surface as usize,
    });
    0
}

/// Copy every surface-backed texture of the current context back into its
/// surface; the guest is about to hand the surfaces on.
pub fn write_back() {
    let context = super::CURRENT.get() as usize;
    if context == 0 {
        return;
    }
    let bindings = BINDINGS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(gl) = gl() else {
        return;
    };
    let pack = (
        PACK_ROW_LENGTH,
        PACK_ALIGNMENT,
        PIXEL_PACK_BUFFER,
        PIXEL_PACK_BUFFER_BINDING,
    );
    for binding in bindings.iter().filter(|b| b.context == context) {
        let Some(pixels) = iosurface::pixels(binding.surface as CFTypeRef) else {
            continue;
        };
        let Some(query) = binding_query(binding.target) else {
            continue;
        };
        let row = (pixels.bytes_per_row / pixels.bytes_per_element.max(1)) as i32;
        let mut previous = 0;
        // SAFETY: the current context, and the surface's pixels, writable
        // shared memory of `bytes_per_row * height` bytes.
        unsafe {
            (gl.GetIntegerv)(query, &mut previous);
            (gl.BindTexture)(binding.target, binding.texture);
            with_store(gl, pack, row, || {
                (gl.GetTexImage)(
                    binding.target,
                    0,
                    binding.format,
                    binding.kind,
                    pixels.data.cast_mut().cast(),
                )
            });
            (gl.BindTexture)(binding.target, previous as u32);
        }
    }
}

/// `glDeleteTextures` is about to free `textures` in the current context.
///
/// # Safety
///
/// `textures` is null or `count` names.
pub unsafe fn forget(count: i32, textures: *const u32) {
    if textures.is_null() || count <= 0 {
        return;
    }
    // SAFETY: `count` names, per the caller.
    let names = unsafe { std::slice::from_raw_parts(textures, count as usize) };
    let context = super::CURRENT.get() as usize;
    let mut bindings = BINDINGS.lock().unwrap_or_else(|e| e.into_inner());
    bindings.retain(|b| {
        let gone = b.context == context && names.contains(&b.texture);
        if gone {
            // SAFETY: the reference this module took.
            unsafe { corefoundation::api::CFRelease(b.surface as CFTypeRef) };
        }
        !gone
    });
}
