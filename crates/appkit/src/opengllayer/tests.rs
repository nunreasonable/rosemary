use super::*;

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use foundation::geometry::{NSRect, NSSize};

thread_local! {
    /// What the subclass and the drawable were asked, in order.
    static CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// What the subclass answers.
    static ANSWERS_FORMAT: Cell<bool> = const { Cell::new(true) };
    static CAN_DRAW: Cell<bool> = const { Cell::new(true) };
}

fn record(call: impl Into<String>) {
    CALLS.with(|calls| calls.borrow_mut().push(call.into()));
}

fn calls() -> Vec<String> {
    CALLS.with(|calls| std::mem::take(&mut *calls.borrow_mut()))
}

/// Tests that put a context in the graveyard or empty it: they share it.
static GRAVEYARD_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

unsafe extern "C" fn copy_format(_: Id, _: Sel, mask: u32) -> Format {
    record(format!("copyCGLPixelFormatForDisplayMask:{mask}"));
    match ANSWERS_FORMAT.get() {
        true => crate::opengl::layer_pixel_format(mask),
        false => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn release_format(_: Id, _: Sel, format: Format) {
    record("releaseCGLPixelFormat:");
    // SAFETY: the format `copy_format` answered.
    unsafe { crate::opengl::CGLReleasePixelFormat(format) };
}

unsafe extern "C" fn copy_context_(_: Id, _: Sel, format: Format) -> Context {
    record("copyCGLContextForPixelFormat:");
    let mut context = std::ptr::null_mut();
    // SAFETY: the format `copy_format` answered, and a writable out-parameter.
    unsafe { crate::opengl::CGLCreateContext(format, std::ptr::null_mut(), &mut context) };
    context
}

unsafe extern "C" fn release_context_(_: Id, _: Sel, context: Context) {
    record("releaseCGLContext:");
    // SAFETY: the context `copy_context_` answered.
    unsafe { crate::opengl::CGLReleaseContext(context) };
}

unsafe extern "C" fn can_draw_(
    _: Id,
    _: Sel,
    context: Context,
    format: Format,
    time: f64,
    stamp: *const TimeStamp,
) -> u8 {
    assert!(!context.is_null() && !format.is_null());
    assert!(time > 0.0, "the layer time is on the media clock");
    // SAFETY: the display time, which is never null here.
    let stamp = unsafe { *stamp };
    assert_eq!(stamp.flags & 1, 1, "the video time is valid");
    assert_eq!(stamp.host_time, (time * 1e9) as u64);
    record("canDrawInCGLContext:");
    CAN_DRAW.get() as u8
}

unsafe extern "C" fn draw_(_: Id, _: Sel, _: Context, _: Format, _: f64, _: *const TimeStamp) {
    record(format!(
        "drawInCGLContext: framebuffer {}",
        default_framebuffer()
    ));
}

fn subclass() -> &'static objc::class::Class {
    static REGISTERING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    crate::bootstrap();
    let _guard = REGISTERING.lock().unwrap();
    let name = c"RosemaryTestOpenGLLayer";
    objc::class::lookup(name).unwrap_or_else(|| {
        let base = objc::class::lookup(c"CAOpenGLLayer").expect("CAOpenGLLayer");
        let class = objc::class::allocate_pair(Some(base), name, 0).expect("a fresh class");
        let add = |sel: &str, imp: *const c_void, types: &CStr| {
            assert!(class.add_method(objc::sel::get(sel), imp as objc::class::Imp, types));
        };
        add(
            "copyCGLPixelFormatForDisplayMask:",
            copy_format as *const c_void,
            c"^v20@0:8I16",
        );
        add(
            "releaseCGLPixelFormat:",
            release_format as *const c_void,
            c"v24@0:8^v16",
        );
        add(
            "copyCGLContextForPixelFormat:",
            copy_context_ as *const c_void,
            c"^v24@0:8^v16",
        );
        add(
            "releaseCGLContext:",
            release_context_ as *const c_void,
            c"v24@0:8^v16",
        );
        add(
            "canDrawInCGLContext:pixelFormat:forLayerTime:displayTime:",
            can_draw_ as *const c_void,
            c"c48@0:8^v16^v24d32^v40",
        );
        add(
            "drawInCGLContext:pixelFormat:forLayerTime:displayTime:",
            draw_ as *const c_void,
            c"v48@0:8^v16^v24d32^v40",
        );
        objc::class::register_pair(class);
        class
    })
}

/// A drawable that records what it is asked and reads back a known picture.
struct Fake {
    disposed: Arc<AtomicBool>,
    bound: bool,
}

impl Fake {
    fn boxed() -> (Box<dyn Drawable>, Arc<AtomicBool>) {
        let disposed = Arc::new(AtomicBool::new(false));
        let fake = Fake {
            disposed: disposed.clone(),
            bound: false,
        };
        (Box::new(fake), disposed)
    }
}

impl Drawable for Fake {
    fn enter(&mut self, _: Context) -> bool {
        record("enter");
        true
    }

    fn bind(&mut self, width: usize, height: usize, _: Format) -> Option<u32> {
        record(format!("bind {width}x{height}"));
        self.bound = true;
        Some(7)
    }

    fn read(&mut self, width: usize, height: usize, out: &mut Vec<u8>) -> bool {
        record("read");
        // The bottom row is 0, the next 1, and so on up: a picture whose
        // orientation shows.
        out.clear();
        for row in 0..height {
            out.extend(std::iter::repeat_n([row as u8, 0x10, 0x20, 0x80], width).flatten());
        }
        true
    }

    fn leave(&mut self) {
        record("leave");
    }

    fn is_allocated(&self) -> bool {
        self.bound
    }

    fn dispose(&mut self) {
        self.disposed.store(true, Ordering::SeqCst);
    }
}

fn instance() -> Id {
    crate::bootstrap();
    let layer = objc::object::create_instance(subclass(), 0);
    // SAFETY: `-init` is `id (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("init"))
    }
}

/// A layer of `width` by `height` points whose frames go to a [`Fake`].
fn layer_of_size(width: f64, height: f64) -> (Id, Arc<AtomicBool>) {
    let layer = instance();
    crate::layer::set_bounds_of(
        layer,
        NSRect {
            size: NSSize { width, height },
            ..Default::default()
        },
    );
    let (fake, disposed) = Fake::boxed();
    layers()
        .as_mut()
        .and_then(|layers| layers.get_mut(&(layer as usize)))
        .and_then(Option::as_mut)
        .expect("an initialised layer has a frame")
        .drawable = fake;
    (layer, disposed)
}

fn send_display(layer: Id) {
    // SAFETY: `-display` is `void (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("display"));
    }
}

fn contents_of(layer: Id) -> Id {
    // SAFETY: `-contents` is `id (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("contents"))
    }
}

fn text(calls: &[String]) -> Vec<&str> {
    calls.iter().map(String::as_str).collect()
}

#[test]
fn the_class_is_a_calayer() {
    crate::bootstrap();
    let class = objc::class::lookup(c"CAOpenGLLayer").expect("CAOpenGLLayer is declared");
    let layer = objc::class::lookup(c"CALayer").expect("CALayer");
    assert!(
        class
            .chain()
            .any(|candidate| std::ptr::eq(candidate, layer))
    );
    assert_eq!(class.instance_size(), INSTANCE_SIZE);
    const { assert!(INSTANCE_SIZE > crate::layer::INSTANCE_SIZE) };
}

#[test]
fn the_class_is_published_to_programs_that_import_it() {
    let symbols: Vec<String> = crate::classes::class_data()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for symbol in [
        "_OBJC_CLASS_$_CAOpenGLLayer",
        "_OBJC_METACLASS_$_CAOpenGLLayer",
    ] {
        assert!(symbols.iter().any(|name| name == symbol), "{symbol}");
    }
}

#[test]
fn a_layer_made_by_init_starts_where_one_made_by_layer_does() {
    let layer = instance();
    assert!(is_opengl(layer));
    assert_eq!(crate::layer::contents_scale_of(layer), 1.0);
    assert_eq!(layer_of(layer).asynchronous, 0);
    assert_eq!(layer_of(layer).wants_extended_dynamic_range, 0);
    assert!(layer_of(layer).colorspace.is_null());
    assert!(contents_of(layer).is_null());
    assert!(
        crate::layer::needs_display_of(layer),
        "a layer has nothing to show until it is drawn once"
    );
    // SAFETY: the receiver of a class method is a class object.
    let made = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(objc::class::ptr(subclass()) as Id, objc::sel::get("layer"))
    };
    assert!(is_opengl(made));
    assert_eq!(crate::layer::contents_scale_of(made), 1.0);
}

#[test]
fn the_properties_read_back_and_the_colorspace_is_held() {
    let layer = instance();
    let space = objc::object::create_instance(objc::class::lookup(c"NSObject").unwrap(), 0);
    let before = objc::arc::retain_count(space);
    // SAFETY: the methods are the ones `install` registered, on a layer this
    // test made.
    unsafe {
        let sel = objc::sel::get("x");
        assert_eq!(is_asynchronous(layer, sel), 0);
        set_asynchronous(layer, sel, 1);
        assert_eq!(is_asynchronous(layer, sel), 1);
        set_asynchronous(layer, sel, 0);
        assert_eq!(is_asynchronous(layer, sel), 0);

        assert_eq!(wants_extended_dynamic_range_content(layer, sel), 0);
        set_wants_extended_dynamic_range_content(layer, sel, 1);
        assert_eq!(wants_extended_dynamic_range_content(layer, sel), 1);

        assert!(colorspace(layer, sel).is_null());
        set_colorspace(layer, sel, space.cast());
        assert_eq!(colorspace(layer, sel), space.cast_const().cast());
        assert_eq!(objc::arc::retain_count(space), before + 1, "held");
        // The same colorspace again is not held twice.
        set_colorspace(layer, sel, space.cast());
        assert_eq!(objc::arc::retain_count(space), before + 1);
        set_colorspace(layer, sel, std::ptr::null());
        assert_eq!(objc::arc::retain_count(space), before, "given back");
    }
}

#[test]
fn the_defaults_are_the_ones_apple_documents() {
    crate::bootstrap();
    let layer = instance();
    let sel = objc::sel::get("x");
    // SAFETY: the methods are the ones `install` registered, called with what
    // they take.
    unsafe {
        assert_eq!(
            can_draw(
                layer,
                sel,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0.0,
                std::ptr::null()
            ),
            1,
            "the default always draws"
        );

        let format = copy_pixel_format(layer, sel, 5);
        assert!(!format.is_null());
        let ask = |attribute: u32| {
            let mut value = -1;
            let sel = objc::sel::get("getValues:forAttribute:forVirtualScreen:");
            let f: unsafe extern "C" fn(Id, Sel, *mut i32, u32, i32) =
                std::mem::transmute(objc::msg::entry());
            f(format.cast(), sel, &mut value, attribute, 0);
            value
        };
        assert_eq!(ask(8), 32, "32 bits of colour");
        assert_eq!(ask(72), 1, "NoRecovery");
        assert_eq!(ask(73), 1, "Accelerated");
        assert_eq!(ask(84), 5, "on the display mask asked for");
        assert_eq!(ask(12), 0, "and nothing else");

        let context = copy_context(layer, sel, format);
        assert!(!context.is_null());
        assert_eq!(crate::opengl::CGLGetPixelFormat(context), format);
        assert_eq!(
            crate::opengl::CGLGetShareGroup(context),
            context,
            "shared with nothing"
        );

        let held = objc::arc::retain_count(format.cast());
        crate::opengl::CGLRetainPixelFormat(format);
        assert_eq!(objc::arc::retain_count(format.cast()), held + 1);
        release_pixel_format(layer, sel, format);
        assert_eq!(objc::arc::retain_count(format.cast()), held);
        release_context(layer, sel, context);
    }
}

#[test]
fn a_frame_runs_the_cycle_in_the_order_core_animation_does() {
    let (layer, _) = layer_of_size(4.0, 3.0);
    calls();
    send_display(layer);
    assert_eq!(
        text(&calls()),
        [
            "copyCGLPixelFormatForDisplayMask:1",
            "copyCGLContextForPixelFormat:",
            "enter",
            "canDrawInCGLContext:",
            "bind 4x3",
            "drawInCGLContext: framebuffer 7",
            "read",
            "leave",
        ]
    );
    assert!(!crate::layer::needs_display_of(layer), "it has been drawn");
    assert_eq!(default_framebuffer(), 0, "and is no longer being drawn");

    // The pixel format and context are made once and kept.
    send_display(layer);
    assert_eq!(
        text(&calls()),
        [
            "enter",
            "canDrawInCGLContext:",
            "bind 4x3",
            "drawInCGLContext: framebuffer 7",
            "read",
            "leave",
        ]
    );
}

#[test]
fn what_is_drawn_becomes_the_contents_the_right_way_up() {
    let (layer, _) = layer_of_size(4.0, 3.0);
    send_display(layer);
    let contents = contents_of(layer);
    assert!(!contents.is_null(), "a frame was published");
    let pixels = iosurface::pixels(contents as CFTypeRef).expect("a surface");
    assert_eq!((pixels.width, pixels.height), (4, 3));
    assert_eq!(pixels.pixel_format, iosurface::PIXEL_FORMAT_BGRA);
    // The fake's bottom row is 0 and its top row is 2; a layer's contents start
    // at the top.
    for (y, expected) in [(0usize, 2u8), (1, 1), (2, 0)] {
        // SAFETY: a row of the surface the layer holds.
        let row =
            unsafe { std::slice::from_raw_parts(pixels.data.add(y * pixels.bytes_per_row), 16) };
        assert_eq!(row[..4], [expected, 0x10, 0x20, 0x80], "row {y}");
        assert_eq!(row[12..], [expected, 0x10, 0x20, 0x80], "row {y}");
    }

    // The next frame is written to the other surface, not the one on show.
    send_display(layer);
    assert_ne!(contents_of(layer), contents);
    send_display(layer);
    assert_eq!(contents_of(layer), contents);
}

#[test]
fn an_opaque_layer_shows_no_alpha_it_drew() {
    let (layer, _) = layer_of_size(2.0, 2.0);
    // SAFETY: `-setOpaque:` is `void (id, SEL, BOOL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("setOpaque:"), 1);
    }
    send_display(layer);
    let pixels = iosurface::pixels(contents_of(layer) as CFTypeRef).expect("a surface");
    // SAFETY: the first row of the surface the layer holds.
    let row = unsafe { std::slice::from_raw_parts(pixels.data, 8) };
    assert_eq!([row[3], row[7]], [0xff, 0xff]);
}

#[test]
fn a_frame_is_as_big_as_the_layer_is_in_pixels() {
    let (layer, _) = layer_of_size(4.0, 3.0);
    // SAFETY: `-setContentsScale:` is `void (id, SEL, CGFloat)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, f64) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("setContentsScale:"), 2.0);
    }
    calls();
    send_display(layer);
    assert!(text(&calls()).contains(&"bind 8x6"));
    let pixels = iosurface::pixels(contents_of(layer) as CFTypeRef).expect("a surface");
    assert_eq!((pixels.width, pixels.height), (8, 6));
}

#[test]
fn a_frame_the_subclass_declines_is_skipped() {
    let (layer, _) = layer_of_size(4.0, 3.0);
    CAN_DRAW.set(false);
    calls();
    send_display(layer);
    CAN_DRAW.set(true);
    assert_eq!(
        text(&calls()),
        [
            "copyCGLPixelFormatForDisplayMask:1",
            "copyCGLContextForPixelFormat:",
            "enter",
            "canDrawInCGLContext:",
            "leave",
        ]
    );
    assert!(contents_of(layer).is_null(), "nothing was drawn to show");
}

#[test]
fn a_subclass_that_answers_no_pixel_format_gets_no_frames() {
    let (layer, _) = layer_of_size(4.0, 3.0);
    ANSWERS_FORMAT.set(false);
    calls();
    send_display(layer);
    ANSWERS_FORMAT.set(true);
    assert_eq!(
        text(&calls()),
        ["copyCGLPixelFormatForDisplayMask:1"],
        "no context is asked for without a format, and nothing is drawn"
    );
    assert!(contents_of(layer).is_null());
}

#[test]
fn a_layer_with_nothing_to_draw_into_is_not_drawn() {
    let (layer, _) = layer_of_size(0.0, 3.0);
    calls();
    send_display(layer);
    assert!(calls().is_empty());
}

#[test]
fn deallocating_gives_back_what_the_subclass_made_and_what_was_drawn_into() {
    let _serial = GRAVEYARD_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (layer, disposed) = layer_of_size(4.0, 3.0);
    send_display(layer);
    calls();
    objc::arc::release(layer);
    let after = calls();
    assert_eq!(
        text(&after),
        ["releaseCGLContext:", "releaseCGLPixelFormat:"],
        "the subclass is asked for both back"
    );
    assert!(
        !disposed.load(Ordering::SeqCst),
        "what was drawn into waits for a pump that has the context to delete it in"
    );
    display_due();
    assert!(disposed.load(Ordering::SeqCst));
    assert!(
        layers()
            .as_ref()
            .is_none_or(|layers| !layers.contains_key(&(layer as usize)))
    );
}

#[test]
fn a_layer_that_drew_nothing_leaves_nothing_to_delete() {
    let _serial = GRAVEYARD_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (layer, disposed) = layer_of_size(4.0, 3.0);
    CAN_DRAW.set(false);
    send_display(layer);
    CAN_DRAW.set(true);
    let waiting = GRAVEYARD.lock().unwrap().len();
    calls();
    objc::arc::release(layer);
    assert_eq!(
        text(&calls()),
        ["releaseCGLContext:", "releaseCGLPixelFormat:"],
        "the subclass still gets both back"
    );
    assert_eq!(
        GRAVEYARD.lock().unwrap().len(),
        waiting,
        "but no context is kept to delete what was never made"
    );
    assert!(!disposed.load(Ordering::SeqCst));
}

#[test]
fn a_layer_nothing_is_showing_is_not_offered_frames() {
    let _serial = GRAVEYARD_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (layer, _) = layer_of_size(4.0, 3.0);
    calls();
    display_due();
    assert!(
        calls().is_empty(),
        "a layer under no view is not on any screen"
    );
    assert!(crate::layer::needs_display_of(layer), "and is still due");
    assert!(crate::coreanimation::showing_view(layer).is_null());
}

#[test]
fn a_frame_is_due_for_a_request_a_new_size_or_the_next_refresh() {
    let refresh = Some(REFRESH);
    let soon = Some(REFRESH / 2);
    // Never drawn: due, whichever kind.
    assert!(is_due(false, true, false, None));
    // A synchronous layer is drawn only when asked, or when its size changed.
    assert!(!is_due(false, false, false, refresh));
    assert!(is_due(true, false, false, soon));
    assert!(is_due(false, false, true, soon));
    // An asynchronous one is also offered each refresh, and no more often.
    assert!(is_due(false, true, false, refresh));
    assert!(!is_due(false, true, false, soon));
}

#[test]
fn rows_are_flipped_and_a_row_of_padding_is_left_alone() {
    // Two pixels by two rows, in a surface whose rows are 12 bytes.
    let read = [
        1, 1, 1, 1, 2, 2, 2, 2, // bottom
        3, 3, 3, 3, 4, 4, 4, 4, // top
    ];
    let mut rows = [0xee; 24];
    flip_into(&mut rows, 12, &read, 2, 2, false);
    assert_eq!(rows[..8], [3, 3, 3, 3, 4, 4, 4, 4], "the top row first");
    assert_eq!(rows[8..12], [0xee; 4], "padding is not written");
    assert_eq!(rows[12..20], [1, 1, 1, 1, 2, 2, 2, 2]);

    flip_into(&mut rows, 12, &read, 2, 2, true);
    assert_eq!(rows[..8], [3, 3, 3, 0xff, 4, 4, 4, 0xff]);
}

#[test]
fn a_hidden_layer_is_not_showing() {
    crate::bootstrap();
    let layer = instance();
    // SAFETY: `-setHidden:` is `void (id, SEL, BOOL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
        f(layer, objc::sel::get("setHidden:"), 1);
    }
    assert!(crate::coreanimation::showing_view(layer).is_null());
}

#[test]
fn a_layer_whose_frames_are_declined_is_not_offered_one_every_pump_for_a_resize() {
    let (layer, _) = layer_of_size(4.0, 3.0);
    CAN_DRAW.set(false);
    send_display(layer);
    CAN_DRAW.set(true);
    let offered = layers()
        .as_ref()
        .and_then(|layers| layers.get(&(layer as usize)))
        .and_then(Option::as_ref)
        .map(|frame| frame.offered)
        .expect("a frame");
    assert_eq!(
        offered,
        Some((4, 3)),
        "a declined frame was still offered at the layer's size"
    );
}
