//! A `CAOpenGLLayer` frame against a real GL driver.
//!
//! A binary of its own: SDL is one per process, and these make GL windows and
//! contexts, which it does not take kindly to being asked for from every thread
//! of a test run at once. Each skips where there is no display to make one on.

use std::cell::Cell;
use std::ffi::{CStr, c_void};
use std::sync::Mutex;

use foundation::geometry::{NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

/// `GL_NONE` and `GL_RENDERBUFFER`, as a framebuffer attachment's type.
const NONE: i32 = 0;
const RENDERBUFFER: i32 = 0x8D41;

/// One test at a time makes windows and contexts.
static SERIAL: Mutex<()> = Mutex::new(());

thread_local! {
    /// Whether the pixel format the subclass answers asks for a depth buffer.
    static WANTS_DEPTH: Cell<bool> = const { Cell::new(false) };
    /// What kind of object the drawable's depth attachment was while drawing.
    static DEPTH_ATTACHMENT: Cell<i32> = const { Cell::new(-1) };
}

/// Whether a GL window can be made here, which SDL is brought up for.
fn has_display() -> bool {
    if let Err(why) = sdl3::init(sdl3::init::VIDEO) {
        eprintln!("skipped: no SDL video: {why}");
        return false;
    }
    let Some(probe) = sdl3::video::create_window(
        c"rosemary probe",
        1,
        1,
        sdl3::video::flags::OPENGL | sdl3::video::flags::HIDDEN,
    ) else {
        eprintln!("skipped: no GL window: {}", sdl3::error());
        return false;
    };
    sdl3::video::destroy_window(probe);
    true
}

fn get<T>(name: &CStr) -> T {
    let found = sdl3::video::gl_proc_address(name).expect("a GL entry point");
    assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
    // SAFETY: the caller names the prototype GL declares for it.
    unsafe { std::mem::transmute_copy(&found) }
}

/// What `CAOpenGLLayer` itself does for `selector`.
fn base(selector: &str) -> *const c_void {
    let class = objc::class::lookup(c"CAOpenGLLayer").expect("CAOpenGLLayer");
    objc::class::method_implementation(class, objc::sel::get(selector))
}

/// A `CAOpenGLLayer` subclass that answers the pixel format `WANTS_DEPTH` says
/// and draws with `draw`, and takes the rest from the base class.
fn subclass(name: &CStr, draw: *const c_void) -> &'static objc::class::Class {
    appkit::bootstrap();
    let base_class = objc::class::lookup(c"CAOpenGLLayer").expect("CAOpenGLLayer");
    let class = objc::class::allocate_pair(Some(base_class), name, 0).expect("a fresh class");
    assert!(class.add_method(
        objc::sel::get("copyCGLPixelFormatForDisplayMask:"),
        copy_format as objc::class::Imp,
        c"^v20@0:8I16",
    ));
    assert!(class.add_method(
        objc::sel::get("drawInCGLContext:pixelFormat:forLayerTime:displayTime:"),
        draw,
        c"v48@0:8^v16^v24d32^v40",
    ));
    objc::class::register_pair(class);
    class
}

unsafe extern "C" fn copy_format(layer: Id, sel: Sel, mask: u32) -> *mut c_void {
    if WANTS_DEPTH.get() {
        // kCGLPFAColorSize 32, kCGLPFADepthSize 24, kCGLPFAStencilSize 8.
        let attributes = [8u32, 32, 12, 24, 13, 8, 0];
        let mut format = std::ptr::null_mut();
        // SAFETY: a zero-terminated list, and a writable out-parameter.
        unsafe {
            appkit::opengl::CGLChoosePixelFormat(
                attributes.as_ptr(),
                &mut format,
                std::ptr::null_mut(),
            )
        };
        return format;
    }
    // SAFETY: `-copyCGLPixelFormatForDisplayMask:` is
    // `CGLPixelFormatObj (id, SEL, uint32_t)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, u32) -> *mut c_void =
            std::mem::transmute(base("copyCGLPixelFormatForDisplayMask:"));
        f(layer, sel, mask)
    }
}

/// A layer of `side` points square, made as a program does: `[[Class alloc] init]`.
fn layer(class: &'static objc::class::Class, side: f64) -> Id {
    let layer = objc::object::create_instance(class, 0);
    // SAFETY: `-init` is `id (id, SEL)`; `-setBounds:` is `void (id, SEL, CGRect)`.
    unsafe {
        let init: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(
            objc::class::method_implementation(class, objc::sel::get("init")),
        );
        let layer = init(layer, objc::sel::get("init"));
        let set_bounds: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(
            objc::class::method_implementation(class, objc::sel::get("setBounds:")),
        );
        set_bounds(
            layer,
            objc::sel::get("setBounds:"),
            NSRect {
                size: NSSize {
                    width: side,
                    height: side,
                },
                ..Default::default()
            },
        );
        layer
    }
}

fn display(layer: Id) {
    // SAFETY: `-display` is `void (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("display"));
    }
}

fn contents(layer: Id) -> Id {
    // SAFETY: `-contents` is `id (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("contents"))
    }
}

/// Draws the top half of the drawable red and the bottom half blue with the
/// driver's own calls, as a program's `-drawInCGLContext:` would, and records
/// what is attached to the drawable's depth attachment point.
unsafe extern "C" fn draw_halves(
    _: Id,
    _: Sel,
    _: *mut c_void,
    _: *mut c_void,
    _: f64,
    _: *const c_void,
) {
    let clear_color: unsafe extern "C" fn(f32, f32, f32, f32) = get(c"glClearColor");
    let clear: unsafe extern "C" fn(u32) = get(c"glClear");
    let enable: unsafe extern "C" fn(u32) = get(c"glEnable");
    let disable: unsafe extern "C" fn(u32) = get(c"glDisable");
    let scissor: unsafe extern "C" fn(i32, i32, i32, i32) = get(c"glScissor");
    let query: unsafe extern "C" fn(u32, u32, u32, *mut i32) =
        get(c"glGetFramebufferAttachmentParameteriv");
    const SCISSOR_TEST: u32 = 0x0C11;
    const COLOR_BUFFER_BIT: u32 = 0x4000;
    const FRAMEBUFFER: u32 = 0x8D40;
    const DEPTH_ATTACHMENT_POINT: u32 = 0x8D00;
    const OBJECT_TYPE: u32 = 0x8CD0;
    let mut kind = -1;
    // SAFETY: GL calls on the layer's context, which is current.
    unsafe {
        enable(SCISSOR_TEST);
        scissor(0, 2, 4, 2);
        clear_color(1.0, 0.0, 0.0, 1.0);
        clear(COLOR_BUFFER_BIT);
        scissor(0, 0, 4, 2);
        clear_color(0.0, 0.0, 1.0, 1.0);
        clear(COLOR_BUFFER_BIT);
        disable(SCISSOR_TEST);
        query(FRAMEBUFFER, DEPTH_ATTACHMENT_POINT, OBJECT_TYPE, &mut kind);
    }
    DEPTH_ATTACHMENT.set(kind);
}

/// The whole path against a real driver: a layer's own context, a framebuffer of
/// the layer's size, a subclass that draws with GL, and the read back.
#[test]
fn a_frame_drawn_with_a_real_driver_comes_back_the_right_way_up() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if !has_display() {
        return;
    }
    let class = subclass(c"RosemaryHalvesOpenGLLayer", draw_halves as *const c_void);
    let layer = layer(class, 4.0);
    display(layer);

    let surface = contents(layer);
    assert!(!surface.is_null(), "a frame was published");
    let pixels = iosurface::pixels(surface as corefoundation::base::CFTypeRef).expect("a surface");
    assert_eq!((pixels.width, pixels.height), (4, 4));
    let texel = |x: usize, y: usize| {
        // SAFETY: a texel of the surface the layer holds.
        unsafe {
            std::slice::from_raw_parts(pixels.data.add(y * pixels.bytes_per_row + x * 4), 4)
                .to_vec()
        }
    };
    // BGRA: red is [0, 0, 255, 255].
    assert_eq!(texel(0, 0), [0, 0, 255, 255], "the top is red");
    assert_eq!(texel(3, 1), [0, 0, 255, 255]);
    assert_eq!(texel(0, 2), [255, 0, 0, 255], "the bottom is blue");
    assert_eq!(texel(3, 3), [255, 0, 0, 255]);
    assert_eq!(appkit::opengllayer::default_framebuffer(), 0);
    assert_eq!(
        DEPTH_ATTACHMENT.get(),
        NONE,
        "a pixel format that asks for no depth buffer gets none"
    );
}

/// A pixel format that asks for a depth buffer gets a drawable with one.
///
/// Its own pixel format, and so its own test: SDL chooses an EGL config when it
/// makes a window, and a context made later takes the last one chosen, so two
/// formats in turn on one thread is not something every driver binds.
#[test]
fn the_drawable_has_a_depth_buffer_when_the_pixel_format_asks_for_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    if !has_display() {
        return;
    }
    WANTS_DEPTH.set(true);
    let class = subclass(c"RosemaryDepthOpenGLLayer", draw_halves as *const c_void);
    display(layer(class, 4.0));
    assert_eq!(DEPTH_ATTACHMENT.get(), RENDERBUFFER);
}
