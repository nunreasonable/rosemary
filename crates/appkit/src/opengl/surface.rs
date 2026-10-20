//! `CGLTexImageIOSurface2D`: a texture whose storage is an IOSurface.
//!
//! On a Mac the texture and the surface are the same memory, so what one
//! texture draws is what every other texture on that surface, and every
//! process holding it, reads. The host GL cannot adopt foreign memory as
//! texture storage, so each texture is a copy, kept in step at the guest's
//! flushes - the point at which a Mac program must flush anyway for another
//! context to see its writes: a texture whose pixels changed since it last
//! matched its surface is copied into the surface, and then every other
//! texture of the flushing context on a surface that moved on is reloaded.
//!
//! A texture is bound to one plane of its surface: plane 0 of a surface that
//! is one plane is all of it, and a planar surface's planes - the luma and
//! the chroma of a video frame - each have textures of their own, which are
//! in step with their plane and no other.
//!
//! FIXME (ACCURACY): a texture in another context catches up only at that
//! context's next flush, and a texture given new storage with `glTexImage2D`
//! after binding is still treated as the surface's.

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
    GetFramebufferAttachmentParameteriv: unsafe extern "C" fn(u32, u32, u32, *mut i32),
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
            GetFramebufferAttachmentParameteriv: get(c"glGetFramebufferAttachmentParameteriv")?,
        })
    })
    .as_ref()
}

/// A texture backed by a surface: what to copy back, and how.
struct Binding {
    context: usize,
    target: u32,
    texture: u32,
    internal_format: u32,
    width: i32,
    height: i32,
    format: u32,
    kind: u32,
    /// Retained for as long as the binding lasts.
    surface: usize,
    plane: usize,
    /// The plane's generation the texture last matched, and a hash of the
    /// pixels both held then.
    generation: u64,
    hash: u64,
}

struct State {
    bindings: Vec<Binding>,
    /// How many times each plane of each surface, by id, has been written from
    /// a texture.
    generations: Vec<(PlaneId, u64)>,
}

/// A surface's id and the index of one of its planes.
type PlaneId = (u32, usize);

thread_local! {
    /// Whether this thread's context drew since its last flush. A flush that
    /// follows no drawing has nothing new to copy into any surface.
    static DREW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Textures the guest attached to a framebuffer or wrote with an upload, by
/// context: the only ones a draw can have changed.
static WRITTEN: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

/// The guest drew, cleared or blitted in this thread's context.
pub fn drew() {
    DREW.with(|d| d.set(true));
}

/// `texture` became something drawing writes to in the current context.
pub fn written(texture: u32) {
    let context = super::CURRENT.get() as usize;
    let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if !written.contains(&(context, texture)) {
        written.push((context, texture));
    }
    drop(written);
    drew();
}

/// A framebuffer was bound for drawing: the texture it draws into is
/// written, whichever call attached it.
pub fn framebuffer_bound(target: u32) {
    const FRAMEBUFFER: u32 = 0x8D40;
    const DRAW_FRAMEBUFFER: u32 = 0x8CA9;
    const DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
    const COLOR_ATTACHMENT0: u32 = 0x8CE0;
    const OBJECT_TYPE: u32 = 0x8CD0;
    const OBJECT_NAME: u32 = 0x8CD1;
    const TEXTURE: i32 = 0x1702;
    if !matches!(target, FRAMEBUFFER | DRAW_FRAMEBUFFER) {
        return;
    }
    let Some(gl) = gl() else {
        return;
    };
    let (mut bound, mut kind, mut name) = (0, 0, 0);
    // SAFETY: state queries on the current context.
    unsafe {
        (gl.GetIntegerv)(DRAW_FRAMEBUFFER_BINDING, &mut bound);
        if bound == 0 {
            return;
        }
        (gl.GetFramebufferAttachmentParameteriv)(DRAW_FRAMEBUFFER, COLOR_ATTACHMENT0, OBJECT_TYPE, &mut kind);
        if kind != TEXTURE {
            return;
        }
        (gl.GetFramebufferAttachmentParameteriv)(DRAW_FRAMEBUFFER, COLOR_ATTACHMENT0, OBJECT_NAME, &mut name);
    }
    written(name as u32);
}

/// The texture bound to `target` in the current context, as an upload to it
/// writes it.
pub fn uploaded(target: u32) {
    let (Some(gl), Some(query)) = (gl(), binding_query(target)) else {
        return;
    };
    let mut texture = 0;
    // SAFETY: a state query on the current context.
    unsafe { (gl.GetIntegerv)(query, &mut texture) };
    written(texture as u32);
}

static STATE: Mutex<State> = Mutex::new(State {
    bindings: Vec::new(),
    generations: Vec::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// FNV-1a over the visible bytes of each row, eight at a time.
fn hash(bytes: &[u8], stride: usize, row: usize, height: usize) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for y in 0..height {
        let line = &bytes[y * stride..y * stride + row];
        let (words, rest) = line.as_chunks::<8>();
        for word in words {
            h = (h ^ u64::from_le_bytes(*word)).wrapping_mul(0x100_0000_01b3);
        }
        for &b in rest {
            h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn generation_of(generations: &[(PlaneId, u64)], plane: PlaneId) -> u64 {
    generations.iter().find(|(p, _)| *p == plane).map_or(0, |(_, g)| *g)
}

fn surface_hash(pixels: &iosurface::Pixels) -> u64 {
    // SAFETY: the surface's pixels, `bytes_per_row * height` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(pixels.data, pixels.bytes_per_row * pixels.height) };
    hash(
        bytes,
        pixels.bytes_per_row,
        pixels.width * pixels.bytes_per_element,
        pixels.height,
    )
}

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
    let plane = plane as usize;
    let (Some(gl), Some(query)) = (gl(), binding_query(target)) else {
        return super::kCGLBadValue;
    };
    let Some(pixels) = iosurface::pixels_of_plane(surface, plane) else {
        if iosurface::is_surface(surface) {
            eprintln!("rosemary: appkit: CGLTexImageIOSurface2D: the surface has no plane {plane}");
        }
        return super::kCGLBadValue;
    };
    if pixels.bytes_per_element == 0
        || width as usize > pixels.width
        || height as usize > pixels.height
    {
        eprintln!(
            "rosemary: appkit: CGLTexImageIOSurface2D: {width}x{height} does not \
             fit plane {plane}, which is {}x{}",
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
    // SAFETY: an IOSurface, per the caller.
    let id = unsafe { iosurface::IOSurfaceGetID(surface) };
    let mut state = state();
    let generation = generation_of(&state.generations, (id, plane));
    let context = object as usize;
    if let Some(at) = state
        .bindings
        .iter()
        .position(|b| b.context == context && b.texture == texture)
    {
        let old = state.bindings.remove(at);
        // SAFETY: the reference this module took.
        unsafe { corefoundation::api::CFRelease(old.surface as CFTypeRef) };
    }
    // SAFETY: an IOSurface, per the caller.
    unsafe { corefoundation::api::CFRetain(surface) };
    state.bindings.push(Binding {
        context,
        target,
        texture,
        internal_format,
        width,
        height,
        format,
        kind,
        surface: surface as usize,
        plane,
        generation,
        hash: surface_hash(&pixels),
    });
    0
}

/// Bring the current context's surface-backed textures and their surfaces
/// into step; the guest is about to hand the surfaces on.
pub fn write_back() {
    let context = super::CURRENT.get() as usize;
    if context == 0 {
        return;
    }
    let Some(gl) = gl() else {
        return;
    };
    let mut guard = state();
    let State {
        bindings,
        generations,
    } = &mut *guard;
    let pack = (
        PACK_ROW_LENGTH,
        PACK_ALIGNMENT,
        PIXEL_PACK_BUFFER,
        PIXEL_PACK_BUFFER_BINDING,
    );
    let unpack = (
        UNPACK_ROW_LENGTH,
        UNPACK_ALIGNMENT,
        PIXEL_UNPACK_BUFFER,
        PIXEL_UNPACK_BUFFER_BINDING,
    );
    let mut scratch = Vec::new();
    let drew = DREW.with(|d| d.replace(false));
    let written: Vec<u32> = match drew {
        true => WRITTEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(c, _)| *c == context)
            .map(|(_, t)| *t)
            .collect(),
        false => Vec::new(),
    };
    // Textures drawn into since they last matched: their pixels become the
    // surface's. Only one drawing can have reached is read back at all, which
    // is what keeps an idle or unrelated flush from copying every surface.
    for binding in bindings
        .iter_mut()
        .filter(|b| b.context == context && written.contains(&b.texture))
    {
        let (Some(pixels), Some(query)) = (
            iosurface::pixels_of_plane(binding.surface as CFTypeRef, binding.plane),
            binding_query(binding.target),
        ) else {
            continue;
        };
        let row = (pixels.bytes_per_row / pixels.bytes_per_element.max(1)) as i32;
        scratch.resize(pixels.bytes_per_row * pixels.height, 0);
        let mut previous = 0;
        // SAFETY: the current context, reading into a buffer of the
        // surface's size.
        unsafe {
            (gl.GetIntegerv)(query, &mut previous);
            (gl.BindTexture)(binding.target, binding.texture);
            with_store(gl, pack, row, || {
                (gl.GetTexImage)(
                    binding.target,
                    0,
                    binding.format,
                    binding.kind,
                    scratch.as_mut_ptr().cast(),
                )
            });
            (gl.BindTexture)(binding.target, previous as u32);
        }
        let now = hash(
            &scratch,
            pixels.bytes_per_row,
            pixels.width * pixels.bytes_per_element,
            pixels.height,
        );
        if now == binding.hash {
            continue;
        }
        // SAFETY: the surface's pixels, writable shared memory of this size.
        unsafe {
            std::ptr::copy_nonoverlapping(scratch.as_ptr(), pixels.data.cast_mut(), scratch.len())
        };
        // SAFETY: a surface this module holds.
        let id = unsafe { iosurface::IOSurfaceGetID(binding.surface as CFTypeRef) };
        let plane_id = (id, binding.plane);
        let generation = match generations.iter_mut().find(|(p, _)| *p == plane_id) {
            Some((_, g)) => g,
            None => {
                generations.push((plane_id, 0));
                &mut generations.last_mut().expect("just pushed").1
            }
        };
        *generation += 1;
        binding.generation = *generation;
        binding.hash = now;
    }
    // Textures on a plane something else has written since: reload them.
    for binding in bindings.iter_mut().filter(|b| b.context == context) {
        // SAFETY: a surface this module holds.
        let id = unsafe { iosurface::IOSurfaceGetID(binding.surface as CFTypeRef) };
        let current = generation_of(generations, (id, binding.plane));
        if binding.generation == current {
            continue;
        }
        let (Some(pixels), Some(query)) = (
            iosurface::pixels_of_plane(binding.surface as CFTypeRef, binding.plane),
            binding_query(binding.target),
        ) else {
            continue;
        };
        let row = (pixels.bytes_per_row / pixels.bytes_per_element.max(1)) as i32;
        let mut previous = 0;
        // SAFETY: the current context, and the surface's own pixels.
        unsafe {
            (gl.GetIntegerv)(query, &mut previous);
            (gl.BindTexture)(binding.target, binding.texture);
            with_store(gl, unpack, row, || {
                (gl.TexImage2D)(
                    binding.target,
                    0,
                    binding.internal_format as i32,
                    binding.width,
                    binding.height,
                    0,
                    binding.format,
                    binding.kind,
                    pixels.data.cast(),
                )
            });
            (gl.BindTexture)(binding.target, previous as u32);
        }
        binding.generation = current;
        binding.hash = surface_hash(&pixels);
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
    WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(c, t)| !(*c == context && names.contains(t)));
    state().bindings.retain(|b| {
        let gone = b.context == context && names.contains(&b.texture);
        if gone {
            // SAFETY: the reference this module took.
            unsafe { corefoundation::api::CFRelease(b.surface as CFTypeRef) };
        }
        !gone
    });
}
