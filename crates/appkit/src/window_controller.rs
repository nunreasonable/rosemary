//! `NSWindowController`, for a program that makes its windows itself: it owns
//! one window and shows it. Nib loading and documents are not here.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// The association key for a controller's window, which it retains.
static WINDOW: u8 = 0;

/// The association key for a controller's document, which it does not retain
/// - the document owns the controller (see `crate::document`), and retaining
/// each other would leak both.
static DOCUMENT: u8 = 0;

fn key() -> *const std::ffi::c_void {
    &WINDOW as *const u8 as *const std::ffi::c_void
}

fn document_key() -> *const std::ffi::c_void {
    &DOCUMENT as *const u8 as *const std::ffi::c_void
}

fn window(controller: Id) -> Id {
    objc::arc::associated(controller, key())
}

/// # Safety
///
/// The selector must name a method taking one object and returning nothing.
unsafe fn send_object(receiver: Id, selector: &str, argument: Id) {
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), argument);
    }
}

fn set_window(controller: Id, value: Id) {
    let old = window(controller);
    if old == value {
        return;
    }
    objc::arc::retain(value);
    objc::arc::set_associated(controller, key(), value);
    if !value.is_null() {
        // SAFETY: an `NSWindow`, which answers it.
        unsafe { send_object(value, "setWindowController:", controller) };
    }
    if !old.is_null() {
        // SAFETY: as above; the old window no longer has this controller.
        unsafe { send_object(old, "setWindowController:", std::ptr::null_mut()) };
        objc::arc::release(old);
    }
}

unsafe extern "C" fn init_with_window(receiver: Id, _sel: Sel, value: Id) -> Id {
    set_window(receiver, value);
    receiver
}

unsafe extern "C" fn window_of(receiver: Id, _sel: Sel) -> Id {
    window(receiver)
}

unsafe extern "C" fn set_window_of(receiver: Id, _sel: Sel, value: Id) {
    set_window(receiver, value);
}

unsafe extern "C" fn show_window(receiver: Id, _sel: Sel, _sender: Id) {
    let window = window(receiver);
    if !window.is_null() {
        // SAFETY: an `NSWindow`.
        unsafe { send_object(window, "makeKeyAndOrderFront:", receiver) };
    }
}

unsafe extern "C" fn close(receiver: Id, _sel: Sel) {
    let window = window(receiver);
    if !window.is_null() {
        // SAFETY: `-close` is `void (id, SEL)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
            f(window, objc::sel::get("close"));
        }
    }
}

unsafe extern "C" fn is_window_loaded(receiver: Id, _sel: Sel) -> u8 {
    u8::from(!window(receiver).is_null())
}

unsafe extern "C" fn document_of(receiver: Id, _sel: Sel) -> Id {
    objc::arc::associated(receiver, document_key())
}

/// `-setDocument:`: `-[NSDocument addWindowController:]` and
/// `-removeWindowController:` are the only callers here, so this does not
/// itself touch the document's own list back.
unsafe extern "C" fn set_document(receiver: Id, _sel: Sel, value: Id) {
    objc::arc::set_associated(receiver, document_key(), value);
}

unsafe extern "C" fn nothing(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    set_window(receiver, std::ptr::null_mut());
    objc::arc::set_associated(receiver, document_key(), std::ptr::null_mut());
    objc::arc::finalize(receiver);
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSWindowController").expect("declared before its methods");
    methods!(class, {
        "initWithWindow:" => init_with_window, c"@24@0:8@16";
        "window" => window_of, c"@16@0:8";
        "setWindow:" => set_window_of, c"v24@0:8@16";
        "showWindow:" => show_window, c"v24@0:8@16";
        "close" => close, c"v16@0:8";
        "isWindowLoaded" => is_window_loaded, c"c16@0:8";
        "document" => document_of, c"@16@0:8";
        "setDocument:" => set_document, c"v24@0:8@16";
        "windowDidLoad" => nothing, c"v16@0:8";
        "windowWillLoad" => nothing, c"v16@0:8";
        "dealloc" => dealloc, c"v16@0:8";
    });
}
