//! `CAOpenGLLayer`: a layer whose pixels an OpenGL context draws.
//!
//! Core Animation owns the cycle and the subclass owns the drawing. A frame is
//! one pass of: ask the subclass for a pixel format and then a context for it
//! (`-copyCGLPixelFormatForDisplayMask:`, `-copyCGLContextForPixelFormat:`),
//! once, and keep them; make the context current; ask whether to draw
//! (`-canDrawInCGLContext:...`, a `NO` skips the frame); draw into a drawable
//! whose viewport is the layer's size (`-drawInCGLContext:...`, whose base
//! implementation flushes). The subclass gives the pixel format and context back
//! when the layer goes (`-releaseCGLPixelFormat:`, `-releaseCGLContext:`).
//!
//! A synchronous layer is drawn when it is sent `-setNeedsDisplay` and when it
//! is first shown or resized; an asynchronous one is also offered a frame once
//! per display refresh, and draws whichever of those `-canDraw` accepts.
//!
//! The drawable is a framebuffer object of the layer's size in the layer's own
//! context. What the subclass draws is read back, flipped to the top-down order a
//! layer's contents have, and becomes the layer's `contents` as a BGRA
//! `IOSurface` - so the compositor in `crate::coreanimation` needs to know
//! nothing of OpenGL. While the subclass draws, binding framebuffer 0 binds that
//! framebuffer, as it would bind the layer's backing store on a Mac; see
//! [`default_framebuffer`].
//!
//! FIXME (ACCURACY): frames are drawn on the thread that pumps events, where
//! Core Animation draws an asynchronous layer on a thread of its own. Neither
//! `colorspace` nor `wantsExtendedDynamicRangeContent` changes the pixels, a
//! multisampled pixel format gets a single-sampled drawable, and the drawable has
//! no `GL_BACK`: a subclass that selects it with `glDrawBuffer` gets the error a
//! framebuffer object gives.

use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use corefoundation::base::CFTypeRef;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// `CAOpenGLLayer`'s storage after `CALayer`'s.
#[repr(C)]
struct OpenGLLayer {
    layer: crate::layer::Layer,
    /// `-colorspace`, retained.
    colorspace: *const c_void,
    asynchronous: u8,
    wants_extended_dynamic_range: u8,
}

pub const INSTANCE_SIZE: usize = size_of::<OpenGLLayer>();

fn layer_of<'a>(receiver: Id) -> &'a mut OpenGLLayer {
    // SAFETY: a CAOpenGLLayer, whose registered layout is OpenGLLayer.
    unsafe { &mut *(receiver as *mut OpenGLLayer) }
}

/// Whether `object` is a `CAOpenGLLayer`, or a subclass of one.
pub(crate) fn is_opengl(object: Id) -> bool {
    objc::class::lookup(c"CAOpenGLLayer").is_some_and(|base| {
        // SAFETY: null or an object the runtime allocated.
        unsafe { objc::object::class_of(object) }
            .is_some_and(|class| class.chain().any(|candidate| std::ptr::eq(candidate, base)))
    })
}

/// `CGLContextObj` and `CGLPixelFormatObj`: handles the subclass hands over.
type Context = *mut c_void;
type Format = *mut c_void;

/// How often an asynchronous layer is offered a frame: a 60 Hz display's.
const REFRESH: Duration = Duration::from_micros(16_667);

/// The most a layer's side can be, in pixels. A drawable past it is one no host
/// GL makes, and the surface that would carry it is refused at the same size.
const MAX_SIDE: f64 = 16384.0;

/// What a layer is drawn into and read back from. The one seam in the cycle that
/// needs a GL driver, so that the rest of it can be exercised without one.
trait Drawable: Send {
    /// Make `context` current, remembering what was. False if it cannot be.
    fn enter(&mut self, context: Context) -> bool;
    /// Make the drawable `width` by `height`, bound as the framebuffer drawing
    /// goes to, with its viewport set, and answer its name. `format` is the
    /// pixel format the context was made from, which says whether it has a
    /// depth or stencil buffer.
    fn bind(&mut self, width: usize, height: usize, format: Format) -> Option<u32>;
    /// What was drawn, as `width * height` BGRA pixels with the bottom row
    /// first, as GL reads them.
    fn read(&mut self, width: usize, height: usize, out: &mut Vec<u8>) -> bool;
    /// Make current again what `enter` found current.
    fn leave(&mut self);
    /// Whether `bind` has made anything that [`dispose`](Drawable::dispose) has
    /// to give back.
    fn is_allocated(&self) -> bool;
    /// Give back what `bind` made, with the context it was made in current.
    fn dispose(&mut self);
}

/// What a layer keeps from one frame to the next.
struct Frame {
    drawable: Box<dyn Drawable>,
    /// What the subclass answered, each a reference of its own to give back.
    format: usize,
    context: usize,
    /// The two surfaces frames alternate between, retained: the compositor may
    /// still be reading the last one when the next is written.
    surfaces: [usize; 2],
    next_surface: usize,
    /// The size the layer was when it was last offered a frame, and when. A
    /// frame the subclass declined was offered, so a layer it keeps declining
    /// is not offered one on every pump for having been resized.
    offered: Option<(usize, usize)>,
    last: Option<Instant>,
    /// Whether a refusal has been said, which is once per layer.
    complained: bool,
    scratch: Vec<u8>,
}

impl Frame {
    fn new(drawable: Box<dyn Drawable>) -> Frame {
        Frame {
            drawable,
            format: 0,
            context: 0,
            surfaces: [0; 2],
            next_surface: 0,
            offered: None,
            last: None,
            complained: false,
            scratch: Vec::new(),
        }
    }
}

/// Every live layer, by address, and its frame state - taken out while a frame
/// is drawn, because drawing runs the subclass's code and must not hold the lock.
static LAYERS: Mutex<Option<HashMap<usize, Option<Frame>>>> = Mutex::new(None);

/// What a layer that has gone leaves to be deleted: the drawable, and the
/// context to delete it in, which is held on to for that.
static GRAVEYARD: Mutex<Vec<(Box<dyn Drawable>, usize)>> = Mutex::new(Vec::new());

fn layers() -> std::sync::MutexGuard<'static, Option<HashMap<usize, Option<Frame>>>> {
    LAYERS.lock().unwrap_or_else(|e| e.into_inner())
}

thread_local! {
    /// The framebuffer standing for the default one while this thread draws a
    /// layer, or zero.
    static DRAWING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The framebuffer that `glBindFramebuffer(_, 0)` binds on this thread now.
///
/// Zero, which is the window's, except while a layer's subclass is drawing: its
/// drawable is the layer's backing store, which a Mac calls framebuffer 0, and
/// what it draws there has to land in the layer rather than in the window.
pub fn default_framebuffer() -> u32 {
    DRAWING.get()
}

/// Whether this thread is drawing a layer now, in which case nothing it does
/// should reach a window.
pub(crate) fn drawing() -> bool {
    DRAWING.get() != 0
}

/// A layer exists: it is to be drawn once it is shown.
pub(crate) fn created(layer: Id) {
    layers()
        .get_or_insert_with(HashMap::new)
        .entry(layer as usize)
        .or_insert_with(|| Some(Frame::new(Box::new(GlDrawable::default()))));
    // A layer has nothing to show until it has been drawn once.
    crate::layer::set_needs_display_of(layer, true);
}

/// `-dealloc`: the subclass is given back what it made, and what was drawn into
/// is deleted when the pump next has a context to delete it in.
pub(crate) fn destroyed(layer: Id) {
    let frame = layers()
        .as_mut()
        .and_then(|layers| layers.remove(&(layer as usize)))
        .flatten();
    let Some(frame) = frame else {
        return;
    };
    let Frame {
        drawable,
        format,
        context,
        surfaces,
        ..
    } = frame;
    if context != 0 {
        if drawable.is_allocated() {
            // The subclass's release may be the last, and what was drawn into
            // has to be deleted in the context it was made in.
            // SAFETY: a context handle the subclass answered.
            unsafe { crate::opengl::CGLRetainContext(context as Context) };
            GRAVEYARD
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((drawable, context));
        }
        send_release_context(layer, context as Context);
    }
    if format != 0 {
        send_release_format(layer, format as Format);
    }
    for surface in surfaces {
        objc::arc::release(surface as Id);
    }
    let colorspace = std::mem::replace(&mut layer_of(layer).colorspace, std::ptr::null());
    objc::arc::release(colorspace as Id);
}

//
// Sending to the subclass
//

/// The display mask of the one display there is, which is the main one's.
const DISPLAY_MASK: u32 = 1;

fn send_copy_format(layer: Id) -> Format {
    // SAFETY: `-copyCGLPixelFormatForDisplayMask:` is
    // `CGLPixelFormatObj (id, SEL, uint32_t)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, u32) -> Format =
            std::mem::transmute(objc::msg::entry());
        f(
            layer,
            objc::sel::get("copyCGLPixelFormatForDisplayMask:"),
            DISPLAY_MASK,
        )
    }
}

fn send_copy_context(layer: Id, format: Format) -> Context {
    // SAFETY: `-copyCGLContextForPixelFormat:` is
    // `CGLContextObj (id, SEL, CGLPixelFormatObj)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Format) -> Context =
            std::mem::transmute(objc::msg::entry());
        f(
            layer,
            objc::sel::get("copyCGLContextForPixelFormat:"),
            format,
        )
    }
}

fn send_release_format(layer: Id, format: Format) {
    // SAFETY: `-releaseCGLPixelFormat:` is `void (id, SEL, CGLPixelFormatObj)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Format) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("releaseCGLPixelFormat:"), format);
    }
}

fn send_release_context(layer: Id, context: Context) {
    // SAFETY: `-releaseCGLContext:` is `void (id, SEL, CGLContextObj)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Context) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("releaseCGLContext:"), context);
    }
}

/// `CVTimeStamp`, which `-canDraw...` and `-draw...` receive as the display time.
#[repr(C)]
#[derive(Clone, Copy)]
struct TimeStamp {
    version: u32,
    video_time_scale: i32,
    video_time: i64,
    host_time: u64,
    rate_scalar: f64,
    video_refresh_period: i64,
    /// `CVSMPTETime`, which nothing here fills in.
    smpte_time: [u8; 24],
    flags: u64,
    reserved: u64,
}

const _: () = {
    assert!(size_of::<TimeStamp>() == 80);
    assert!(std::mem::offset_of!(TimeStamp, host_time) == 16);
    assert!(std::mem::offset_of!(TimeStamp, flags) == 64);
};

impl TimeStamp {
    /// The frame being made, shown at `seconds` on the media clock - which is
    /// `mach_absolute_time` in seconds, so the host time is its nanoseconds.
    fn at(seconds: f64) -> TimeStamp {
        const NANOSECONDS: f64 = 1e9;
        /// `kCVTimeStamp{VideoTime,HostTime,VideoRefreshPeriod,RateScalar}Valid`.
        const VALID: u64 = 1 | 1 << 1 | 1 << 3 | 1 << 4;
        TimeStamp {
            version: 0,
            video_time_scale: NANOSECONDS as i32,
            video_time: (seconds * NANOSECONDS) as i64,
            host_time: (seconds * NANOSECONDS) as u64,
            rate_scalar: 1.0,
            video_refresh_period: REFRESH.as_nanos() as i64,
            smpte_time: [0; 24],
            flags: VALID,
            reserved: 0,
        }
    }
}

type CanDraw = unsafe extern "C" fn(Id, Sel, Context, Format, f64, *const TimeStamp) -> u8;
type Draw = unsafe extern "C" fn(Id, Sel, Context, Format, f64, *const TimeStamp);

fn send_can_draw(
    layer: Id,
    context: Context,
    format: Format,
    time: f64,
    stamp: &TimeStamp,
) -> bool {
    // SAFETY: `-canDrawInCGLContext:pixelFormat:forLayerTime:displayTime:` is
    // `BOOL (id, SEL, CGLContextObj, CGLPixelFormatObj, CFTimeInterval,
    // const CVTimeStamp *)`.
    unsafe {
        let f: CanDraw = std::mem::transmute(objc::msg::entry());
        f(
            layer,
            objc::sel::get("canDrawInCGLContext:pixelFormat:forLayerTime:displayTime:"),
            context,
            format,
            time,
            stamp,
        ) != 0
    }
}

fn send_draw(layer: Id, context: Context, format: Format, time: f64, stamp: &TimeStamp) {
    // SAFETY: `-drawInCGLContext:pixelFormat:forLayerTime:displayTime:` is
    // `void (id, SEL, CGLContextObj, CGLPixelFormatObj, CFTimeInterval,
    // const CVTimeStamp *)`.
    unsafe {
        let f: Draw = std::mem::transmute(objc::msg::entry());
        f(
            layer,
            objc::sel::get("drawInCGLContext:pixelFormat:forLayerTime:displayTime:"),
            context,
            format,
            time,
            stamp,
        );
    }
}

//
// The cycle
//

/// The layer's size in pixels: its bounds, at its scale. `None` for a layer with
/// nothing to draw into.
fn pixel_size(layer: Id) -> Option<(usize, usize)> {
    let bounds = crate::layer::bounds_of(layer);
    let scale = match crate::layer::contents_scale_of(layer) {
        scale if scale > 0.0 => scale,
        _ => 1.0,
    };
    let width = (bounds.size.width * scale).ceil();
    let height = (bounds.size.height * scale).ceil();
    ((1.0..=MAX_SIDE).contains(&width) && (1.0..=MAX_SIDE).contains(&height))
        .then_some((width as usize, height as usize))
}

/// `-display`: one frame, drawn now.
///
/// Whichever way it is reached - the pump finding the layer due, or a subclass
/// that overrides `-display` and calls this one - the frame is the whole cycle,
/// so a subclass sees the same sequence either way.
unsafe extern "C" fn display(receiver: Id, _sel: Sel) {
    crate::layer::set_needs_display_of(receiver, false);
    let frame = layers()
        .as_mut()
        .and_then(|layers| layers.get_mut(&(receiver as usize)))
        .and_then(Option::take);
    // Gone, or already being drawn: a subclass sending `-display` from inside
    // its own drawing asks for a frame inside a frame.
    let Some(mut frame) = frame else {
        return;
    };
    cycle(receiver, &mut frame);
    frame.last = Some(Instant::now());
    if let Some(slot) = layers()
        .as_mut()
        .and_then(|layers| layers.get_mut(&(receiver as usize)))
    {
        *slot = Some(frame);
    }
}

/// Say, once for the layer, why it is not being drawn.
fn refuse(frame: &mut Frame, layer: Id, why: &str) {
    if !std::mem::replace(&mut frame.complained, true) {
        // SAFETY: a live object.
        let name = unsafe { objc::object::class_of(layer) }
            .map_or("CAOpenGLLayer", |class| class.name_str());
        eprintln!("rosemary: appkit: -[{name} display]: {why}");
    }
}

fn cycle(layer: Id, frame: &mut Frame) {
    let Some((width, height)) = pixel_size(layer) else {
        return;
    };
    frame.offered = Some((width, height));
    if frame.format == 0 {
        frame.format = send_copy_format(layer) as usize;
        if frame.format == 0 {
            return refuse(
                frame,
                layer,
                "-copyCGLPixelFormatForDisplayMask: answered no pixel format",
            );
        }
    }
    if frame.context == 0 {
        frame.context = send_copy_context(layer, frame.format as Format) as usize;
        if frame.context == 0 {
            return refuse(
                frame,
                layer,
                "-copyCGLContextForPixelFormat: answered no context",
            );
        }
    }
    let (context, format) = (frame.context as Context, frame.format as Format);
    if !frame.drawable.enter(context) {
        return refuse(
            frame,
            layer,
            "the context it answered cannot be made current",
        );
    }
    let time = darwin::mach::media_time();
    let stamp = TimeStamp::at(time);
    let wanted = send_can_draw(layer, context, format, time, &stamp);
    let framebuffer = match wanted {
        true => frame.drawable.bind(width, height, format),
        false => None,
    };
    if let Some(framebuffer) = framebuffer {
        DRAWING.set(framebuffer);
        send_draw(layer, context, format, time, &stamp);
        DRAWING.set(0);
        if frame.drawable.read(width, height, &mut frame.scratch) {
            if !publish(layer, frame, width, height) {
                refuse(frame, layer, "a surface for the frame could not be made");
            }
        } else {
            refuse(frame, layer, "what it drew could not be read back");
        }
    }
    frame.drawable.leave();
}

/// Make `frame.scratch`, a drawn frame bottom row first, the layer's contents.
fn publish(layer: Id, frame: &mut Frame, width: usize, height: usize) -> bool {
    let slot = frame.next_surface;
    let current = frame.surfaces[slot];
    let fits = current != 0
        && iosurface::pixels(current as CFTypeRef)
            .is_some_and(|pixels| (pixels.width, pixels.height) == (width, height));
    if !fits {
        let Some(made) = iosurface::create(width, height, 4, iosurface::PIXEL_FORMAT_BGRA) else {
            return false;
        };
        objc::arc::release(current as Id);
        frame.surfaces[slot] = made as usize;
    }
    let surface = frame.surfaces[slot] as CFTypeRef;
    let Some(pixels) = iosurface::pixels(surface) else {
        return false;
    };
    let opaque = crate::layer::is_opaque_of(layer);
    // SAFETY: a surface this layer holds; locking it moves its seed when
    // unlocked, which is what tells a texture on it that it changed.
    unsafe { iosurface::IOSurfaceLock(surface, 0, std::ptr::null_mut()) };
    // SAFETY: the surface's rows, `bytes_per_row` bytes each, which nothing else
    // writes: it is not the one on show.
    let rows = unsafe {
        std::slice::from_raw_parts_mut(pixels.data as *mut u8, pixels.bytes_per_row * height)
    };
    flip_into(
        rows,
        pixels.bytes_per_row,
        &frame.scratch,
        width,
        height,
        opaque,
    );
    // SAFETY: as above.
    unsafe { iosurface::IOSurfaceUnlock(surface, 0, std::ptr::null_mut()) };
    frame.next_surface ^= 1;
    crate::coreanimation::publish_contents(layer, surface as Id);
    true
}

/// Write `read`, `width` by `height` pixels with the bottom row first, into
/// `rows` with the top row first - the order GL reads in against the order a
/// layer's contents have. A layer that is opaque shows no alpha it drew.
fn flip_into(
    rows: &mut [u8],
    bytes_per_row: usize,
    read: &[u8],
    width: usize,
    height: usize,
    opaque: bool,
) {
    let row = width * 4;
    for (y, source) in read.chunks_exact(row).take(height).enumerate() {
        let at = (height - 1 - y) * bytes_per_row;
        let target = &mut rows[at..at + row];
        target.copy_from_slice(source);
        if opaque {
            for texel in target.as_chunks_mut::<4>().0 {
                texel[3] = 0xff;
            }
        }
    }
}

/// Whether a layer is to be offered a frame: one asked for, a size it has not
/// been drawn at, or - if it is asynchronous - one more of the refresh's.
fn is_due(
    needs_display: bool,
    asynchronous: bool,
    resized: bool,
    since_last: Option<Duration>,
) -> bool {
    needs_display
        || resized
        || (asynchronous && since_last.is_none_or(|elapsed| elapsed >= REFRESH))
}

/// What the pump reads of a layer's frame before it decides to draw it.
struct Candidate {
    layer: usize,
    /// When it was last offered a frame, and at what size.
    last: Option<Instant>,
    offered: Option<(usize, usize)>,
}

/// Draw each layer that is shown and due. Called from the event pump, before the
/// windows are composed: it is the one place that is not inside the compositor,
/// whose locks the subclass's drawing must not run under.
pub(crate) fn display_due() {
    drain_graveyard();
    let candidates: Vec<Candidate> = match layers().as_ref() {
        Some(layers) if !layers.is_empty() => layers
            .iter()
            .filter_map(|(layer, frame)| {
                frame.as_ref().map(|frame| Candidate {
                    layer: *layer,
                    last: frame.last,
                    offered: frame.offered,
                })
            })
            .collect(),
        _ => return,
    };
    for Candidate {
        layer,
        last,
        offered,
    } in candidates
    {
        let layer = layer as Id;
        if !is_shown(layer) {
            continue;
        }
        let resized = pixel_size(layer).is_some_and(|size| offered != Some(size));
        if !is_due(
            crate::layer::needs_display_of(layer),
            layer_of(layer).asynchronous != 0,
            resized,
            last.map(|at| at.elapsed()),
        ) {
            continue;
        }
        // Held for the frame: the subclass's drawing may be what releases it.
        objc::arc::retain(layer);
        send_display(layer);
        objc::arc::release(layer);
    }
}

fn send_display(layer: Id) {
    // SAFETY: `-display` is `void (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("display"));
    }
}

/// Whether a window is showing `layer` now.
fn is_shown(layer: Id) -> bool {
    let view = crate::coreanimation::showing_view(layer);
    if view.is_null() {
        return false;
    }
    // SAFETY: a view this crate made, which holds the layer's tree.
    let window = unsafe { crate::view::window_of(view) };
    crate::window::is_shown(window) && !crate::view::is_hidden_or_has_hidden_ancestor(view)
}

/// Delete what layers that have gone drew into.
fn drain_graveyard() {
    let gone = std::mem::take(&mut *GRAVEYARD.lock().unwrap_or_else(|e| e.into_inner()));
    for (mut drawable, context) in gone {
        if drawable.enter(context as Context) {
            drawable.dispose();
            drawable.leave();
        }
        // SAFETY: the reference `destroyed` took.
        unsafe { crate::opengl::CGLReleaseContext(context as Context) };
    }
}

//
// The class
//

unsafe extern "C" fn is_asynchronous(receiver: Id, _sel: Sel) -> u8 {
    layer_of(receiver).asynchronous
}

unsafe extern "C" fn set_asynchronous(receiver: Id, _sel: Sel, value: u8) {
    layer_of(receiver).asynchronous = (value != 0) as u8;
}

unsafe extern "C" fn colorspace(receiver: Id, _sel: Sel) -> *const c_void {
    layer_of(receiver).colorspace
}

unsafe extern "C" fn set_colorspace(receiver: Id, _sel: Sel, colorspace: *const c_void) {
    let layer = layer_of(receiver);
    if layer.colorspace != colorspace {
        let old = std::mem::replace(
            &mut layer.colorspace,
            objc::arc::retain(colorspace as Id) as _,
        );
        objc::arc::release(old as Id);
    }
}

unsafe extern "C" fn wants_extended_dynamic_range_content(receiver: Id, _sel: Sel) -> u8 {
    layer_of(receiver).wants_extended_dynamic_range
}

unsafe extern "C" fn set_wants_extended_dynamic_range_content(receiver: Id, _sel: Sel, value: u8) {
    layer_of(receiver).wants_extended_dynamic_range = (value != 0) as u8;
}

/// `-copyCGLPixelFormatForDisplayMask:`.
unsafe extern "C" fn copy_pixel_format(_receiver: Id, _sel: Sel, mask: u32) -> Format {
    crate::opengl::layer_pixel_format(mask)
}

/// `-releaseCGLPixelFormat:`.
unsafe extern "C" fn release_pixel_format(_receiver: Id, _sel: Sel, format: Format) {
    // SAFETY: a pixel format `-copyCGLPixelFormatForDisplayMask:` answered.
    unsafe { crate::opengl::CGLReleasePixelFormat(format) };
}

/// `-copyCGLContextForPixelFormat:`: "a new context with a null share context".
unsafe extern "C" fn copy_context(_receiver: Id, _sel: Sel, format: Format) -> Context {
    let mut context = std::ptr::null_mut();
    // SAFETY: a pixel format, and a writable out-parameter.
    if unsafe { crate::opengl::CGLCreateContext(format, std::ptr::null_mut(), &mut context) } != 0 {
        return std::ptr::null_mut();
    }
    context
}

/// `-releaseCGLContext:`.
unsafe extern "C" fn release_context(_receiver: Id, _sel: Sel, context: Context) {
    // SAFETY: a context `-copyCGLContextForPixelFormat:` answered.
    unsafe { crate::opengl::CGLReleaseContext(context) };
}

/// `-canDrawInCGLContext:...`: "the default implementation always returns YES".
unsafe extern "C" fn can_draw(
    _receiver: Id,
    _sel: Sel,
    _context: Context,
    _format: Format,
    _time: f64,
    _stamp: *const TimeStamp,
) -> u8 {
    1
}

/// `-drawInCGLContext:...`: "the default implementation of the method flushes the
/// context" - which is what a subclass reaches by calling `super` once it has
/// drawn.
///
/// The flush is the driver's own, not a guest's `glFlush`: that one presents a
/// single-buffered context's window, and this context has none.
unsafe extern "C" fn draw(
    _receiver: Id,
    _sel: Sel,
    context: Context,
    _format: Format,
    _time: f64,
    _stamp: *const TimeStamp,
) {
    // SAFETY: a context handle, which is refused if it is not one.
    unsafe {
        if crate::opengl::CGLGetCurrentContext() != context
            && crate::opengl::CGLSetCurrentContext(context) != 0
        {
            return;
        }
    }
    if let Some(flush) = sdl3::video::gl_proc_address(c"glFlush") {
        // SAFETY: `void glFlush(void)`.
        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(flush)() };
    }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"CAOpenGLLayer").expect("declared before its methods");
    methods!(class, {
        "isAsynchronous" => is_asynchronous, c"c16@0:8";
        "setAsynchronous:" => set_asynchronous, c"v20@0:8c16";
        "colorspace" => colorspace, c"^{CGColorSpace=}16@0:8";
        "setColorspace:" => set_colorspace, c"v24@0:8^{CGColorSpace=}16";
        "wantsExtendedDynamicRangeContent" => wants_extended_dynamic_range_content, c"c16@0:8";
        "setWantsExtendedDynamicRangeContent:" => set_wants_extended_dynamic_range_content, c"v20@0:8c16";
        "copyCGLPixelFormatForDisplayMask:" => copy_pixel_format, c"^{_CGLPixelFormatObject=}20@0:8I16";
        "releaseCGLPixelFormat:" => release_pixel_format, c"v24@0:8^{_CGLPixelFormatObject=}16";
        "copyCGLContextForPixelFormat:" => copy_context, c"^{_CGLContextObject=}24@0:8^{_CGLPixelFormatObject=}16";
        "releaseCGLContext:" => release_context, c"v24@0:8^{_CGLContextObject=}16";
        "canDrawInCGLContext:pixelFormat:forLayerTime:displayTime:" => can_draw, c"c48@0:8^{_CGLContextObject=}16^{_CGLPixelFormatObject=}24d32^{CVTimeStamp=}40";
        "drawInCGLContext:pixelFormat:forLayerTime:displayTime:" => draw, c"v48@0:8^{_CGLContextObject=}16^{_CGLPixelFormatObject=}24d32^{CVTimeStamp=}40";
        "display" => display, c"v16@0:8";
    });
}

//
// The GL drawable
//

const FRAMEBUFFER: u32 = 0x8D40;
const READ_FRAMEBUFFER: u32 = 0x8CA8;
const READ_FRAMEBUFFER_BINDING: u32 = 0x8CAA;
const RENDERBUFFER: u32 = 0x8D41;
const COLOR_ATTACHMENT0: u32 = 0x8CE0;
const DEPTH_STENCIL_ATTACHMENT: u32 = 0x821A;
const FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const RGBA8: u32 = 0x8058;
const DEPTH24_STENCIL8: u32 = 0x88F0;
const BGRA: u32 = 0x80E1;
const UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const PACK_ROW_LENGTH: u32 = 0x0D02;
const PACK_SKIP_ROWS: u32 = 0x0D03;
const PACK_SKIP_PIXELS: u32 = 0x0D04;
const PACK_ALIGNMENT: u32 = 0x0D05;
const PIXEL_PACK_BUFFER: u32 = 0x88EB;
const PIXEL_PACK_BUFFER_BINDING: u32 = 0x88ED;

#[allow(non_snake_case)]
struct Gl {
    GetIntegerv: unsafe extern "C" fn(u32, *mut i32),
    GenFramebuffers: unsafe extern "C" fn(i32, *mut u32),
    DeleteFramebuffers: unsafe extern "C" fn(i32, *const u32),
    BindFramebuffer: unsafe extern "C" fn(u32, u32),
    CheckFramebufferStatus: unsafe extern "C" fn(u32) -> u32,
    GenRenderbuffers: unsafe extern "C" fn(i32, *mut u32),
    DeleteRenderbuffers: unsafe extern "C" fn(i32, *const u32),
    BindRenderbuffer: unsafe extern "C" fn(u32, u32),
    RenderbufferStorage: unsafe extern "C" fn(u32, u32, i32, i32),
    FramebufferRenderbuffer: unsafe extern "C" fn(u32, u32, u32, u32),
    Viewport: unsafe extern "C" fn(i32, i32, i32, i32),
    ReadBuffer: unsafe extern "C" fn(u32),
    ReadPixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
    PixelStorei: unsafe extern "C" fn(u32, i32),
    BindBuffer: unsafe extern "C" fn(u32, u32),
}

/// The driver's entry points, found once - which has to be with a context
/// current.
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
            GenFramebuffers: get(c"glGenFramebuffers")?,
            DeleteFramebuffers: get(c"glDeleteFramebuffers")?,
            BindFramebuffer: get(c"glBindFramebuffer")?,
            CheckFramebufferStatus: get(c"glCheckFramebufferStatus")?,
            GenRenderbuffers: get(c"glGenRenderbuffers")?,
            DeleteRenderbuffers: get(c"glDeleteRenderbuffers")?,
            BindRenderbuffer: get(c"glBindRenderbuffer")?,
            RenderbufferStorage: get(c"glRenderbufferStorage")?,
            FramebufferRenderbuffer: get(c"glFramebufferRenderbuffer")?,
            Viewport: get(c"glViewport")?,
            ReadBuffer: get(c"glReadBuffer")?,
            ReadPixels: get(c"glReadPixels")?,
            PixelStorei: get(c"glPixelStorei")?,
            BindBuffer: get(c"glBindBuffer")?,
        })
    })
    .as_ref()
}

/// A framebuffer object of the layer's size, in the layer's context.
#[derive(Default)]
struct GlDrawable {
    framebuffer: u32,
    color: u32,
    /// Zero for a pixel format with no depth or stencil buffer.
    depth: u32,
    width: usize,
    height: usize,
    /// The context current before `enter`.
    saved: Option<Context>,
}

// SAFETY: names of GL objects, and a context handle that is only compared and
// handed back to the CGL calls that validate it.
unsafe impl Send for GlDrawable {}

impl GlDrawable {
    fn release_objects(&mut self, gl: &Gl) {
        // SAFETY: names this drawable made in the context that is current.
        unsafe {
            if self.framebuffer != 0 {
                (gl.DeleteFramebuffers)(1, &self.framebuffer);
            }
            if self.color != 0 {
                (gl.DeleteRenderbuffers)(1, &self.color);
            }
            if self.depth != 0 {
                (gl.DeleteRenderbuffers)(1, &self.depth);
            }
        }
        self.framebuffer = 0;
        self.color = 0;
        self.depth = 0;
    }

    /// Make the objects for `width` by `height`.
    fn allocate(&mut self, gl: &Gl, width: usize, height: usize, with_depth: bool) -> bool {
        self.release_objects(gl);
        // SAFETY: GL calls on the context that is current, with names it just
        // generated.
        unsafe {
            (gl.GenFramebuffers)(1, &mut self.framebuffer);
            (gl.GenRenderbuffers)(1, &mut self.color);
            (gl.BindRenderbuffer)(RENDERBUFFER, self.color);
            (gl.RenderbufferStorage)(RENDERBUFFER, RGBA8, width as i32, height as i32);
            (gl.BindFramebuffer)(FRAMEBUFFER, self.framebuffer);
            (gl.FramebufferRenderbuffer)(FRAMEBUFFER, COLOR_ATTACHMENT0, RENDERBUFFER, self.color);
            if with_depth {
                (gl.GenRenderbuffers)(1, &mut self.depth);
                (gl.BindRenderbuffer)(RENDERBUFFER, self.depth);
                (gl.RenderbufferStorage)(
                    RENDERBUFFER,
                    DEPTH24_STENCIL8,
                    width as i32,
                    height as i32,
                );
                (gl.FramebufferRenderbuffer)(
                    FRAMEBUFFER,
                    DEPTH_STENCIL_ATTACHMENT,
                    RENDERBUFFER,
                    self.depth,
                );
            }
            (gl.BindRenderbuffer)(RENDERBUFFER, 0);
            if (gl.CheckFramebufferStatus)(FRAMEBUFFER) != FRAMEBUFFER_COMPLETE {
                (gl.BindFramebuffer)(FRAMEBUFFER, 0);
                self.release_objects(gl);
                return false;
            }
        }
        (self.width, self.height) = (width, height);
        true
    }
}

impl Drawable for GlDrawable {
    fn enter(&mut self, context: Context) -> bool {
        // SAFETY: a handle, which `CGLSetCurrentContext` refuses if it is not
        // one this crate made.
        unsafe {
            let previous = crate::opengl::CGLGetCurrentContext();
            if previous != context && crate::opengl::CGLSetCurrentContext(context) != 0 {
                return false;
            }
            self.saved = Some(previous);
        }
        true
    }

    fn bind(&mut self, width: usize, height: usize, format: Format) -> Option<u32> {
        let gl = gl()?;
        if self.framebuffer == 0 || (self.width, self.height) != (width, height) {
            let (depth, stencil) = crate::opengl::depth_and_stencil_bits(format);
            if !self.allocate(gl, width, height, depth > 0 || stencil > 0) {
                return None;
            }
        }
        // SAFETY: GL calls on the context that is current, with objects it owns.
        unsafe {
            (gl.BindFramebuffer)(FRAMEBUFFER, self.framebuffer);
            (gl.Viewport)(0, 0, width as i32, height as i32);
        }
        Some(self.framebuffer)
    }

    fn read(&mut self, width: usize, height: usize, out: &mut Vec<u8>) -> bool {
        let Some(gl) = gl() else {
            return false;
        };
        out.resize(width * height * 4, 0);
        let (mut read_binding, mut pack_buffer) = (0, 0);
        let (mut row_length, mut skip_rows, mut skip_pixels, mut alignment) = (0, 0, 0, 0);
        // SAFETY: GL calls on the context that is current; what the guest had
        // bound or set is put back, so reading a frame changes none of it.
        unsafe {
            (gl.GetIntegerv)(READ_FRAMEBUFFER_BINDING, &mut read_binding);
            (gl.GetIntegerv)(PIXEL_PACK_BUFFER_BINDING, &mut pack_buffer);
            (gl.GetIntegerv)(PACK_ROW_LENGTH, &mut row_length);
            (gl.GetIntegerv)(PACK_SKIP_ROWS, &mut skip_rows);
            (gl.GetIntegerv)(PACK_SKIP_PIXELS, &mut skip_pixels);
            (gl.GetIntegerv)(PACK_ALIGNMENT, &mut alignment);
            (gl.BindFramebuffer)(READ_FRAMEBUFFER, self.framebuffer);
            (gl.ReadBuffer)(COLOR_ATTACHMENT0);
            (gl.BindBuffer)(PIXEL_PACK_BUFFER, 0);
            (gl.PixelStorei)(PACK_ROW_LENGTH, 0);
            (gl.PixelStorei)(PACK_SKIP_ROWS, 0);
            (gl.PixelStorei)(PACK_SKIP_PIXELS, 0);
            (gl.PixelStorei)(PACK_ALIGNMENT, 4);
            (gl.ReadPixels)(
                0,
                0,
                width as i32,
                height as i32,
                BGRA,
                UNSIGNED_INT_8_8_8_8_REV,
                out.as_mut_ptr().cast(),
            );
            (gl.PixelStorei)(PACK_ALIGNMENT, alignment);
            (gl.PixelStorei)(PACK_SKIP_PIXELS, skip_pixels);
            (gl.PixelStorei)(PACK_SKIP_ROWS, skip_rows);
            (gl.PixelStorei)(PACK_ROW_LENGTH, row_length);
            (gl.BindBuffer)(PIXEL_PACK_BUFFER, pack_buffer as u32);
            (gl.BindFramebuffer)(READ_FRAMEBUFFER, read_binding as u32);
        }
        true
    }

    fn leave(&mut self) {
        if let Some(previous) = self.saved.take() {
            // SAFETY: the handle that was current, or null.
            unsafe {
                if crate::opengl::CGLGetCurrentContext() != previous {
                    crate::opengl::CGLSetCurrentContext(previous);
                }
            }
        }
    }

    fn is_allocated(&self) -> bool {
        self.framebuffer != 0 || self.color != 0 || self.depth != 0
    }

    fn dispose(&mut self) {
        if let Some(gl) = gl() {
            self.release_objects(gl);
        }
    }
}

#[cfg(test)]
mod tests;
