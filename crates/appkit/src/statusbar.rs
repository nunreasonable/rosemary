//! `NSStatusBar` and `NSStatusItem`: the icons at the right of the menu bar.
//!
//! There is no menu bar to put them in, so an item is kept - its menu, length
//! and visibility read back as set - and shown nowhere. Its `button` is nil,
//! which a program configuring the icon messages harmlessly.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct StatusItem {
    isa: *const c_void,
    menu: Id,
    length: f64,
    visible: usize,
}

pub const ITEM_INSTANCE_SIZE: usize = size_of::<StatusItem>();

fn item_of<'a>(receiver: Id) -> &'a mut StatusItem {
    // SAFETY: an item of this module's.
    unsafe { &mut *(receiver as *mut StatusItem) }
}

/// `+[NSStatusBar systemStatusBar]`: one bar for the process.
unsafe extern "C" fn system_status_bar(_class: Id, _sel: Sel) -> Id {
    static BAR: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BAR.get_or_init(|| {
        let class = objc::class::lookup(c"NSStatusBar").expect("declared before its methods");
        let bar = objc::object::create_instance(class, 0);
        objc::arc::make_immortal(bar);
        bar as usize
    }) as Id
}

/// `-statusItemWithLength:`: an item the bar owns, so the caller does not.
unsafe extern "C" fn status_item_with_length(_receiver: Id, _sel: Sel, length: f64) -> Id {
    let class = objc::class::lookup(c"NSStatusItem").expect("declared before its methods");
    let item = objc::object::create_instance(class, 0);
    let state = item_of(item);
    state.length = length;
    state.visible = 1;
    // Kept by the bar until `-removeStatusItem:`, as Apple's bar keeps it.
    item
}

unsafe extern "C" fn remove_status_item(_receiver: Id, _sel: Sel, item: Id) {
    objc::arc::release(item);
}

/// `-thickness`: the menu bar's height on a standard display.
unsafe extern "C" fn thickness(_receiver: Id, _sel: Sel) -> f64 {
    22.0
}

unsafe extern "C" fn nil(_receiver: Id, _sel: Sel) -> Id {
    std::ptr::null_mut()
}

unsafe extern "C" fn ignore_object(_receiver: Id, _sel: Sel, _object: Id) {}

unsafe extern "C" fn menu(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).menu
}

unsafe extern "C" fn set_menu(receiver: Id, _sel: Sel, menu: Id) {
    let item = item_of(receiver);
    let old = std::mem::replace(&mut item.menu, objc::arc::retain(menu));
    objc::arc::release(old);
}

unsafe extern "C" fn length(receiver: Id, _sel: Sel) -> f64 {
    item_of(receiver).length
}

unsafe extern "C" fn set_length(receiver: Id, _sel: Sel, length: f64) {
    item_of(receiver).length = length;
}

unsafe extern "C" fn is_visible(receiver: Id, _sel: Sel) -> u8 {
    u8::from(item_of(receiver).visible != 0)
}

unsafe extern "C" fn set_visible(receiver: Id, _sel: Sel, visible: u8) {
    item_of(receiver).visible = usize::from(visible);
}

unsafe extern "C" fn ignore_flag(_receiver: Id, _sel: Sel, _flag: u8) {}

unsafe extern "C" fn status_bar(_receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the class method ignores its receiver.
    unsafe { system_status_bar(std::ptr::null_mut(), _sel) }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    objc::arc::release(item_of(receiver).menu);
    objc::arc::finalize(receiver);
}

pub(crate) fn install() {
    let bar = objc::class::lookup(c"NSStatusBar").expect("declared before its methods");
    class_methods!(bar, {
        "systemStatusBar" => system_status_bar, c"@16@0:8";
    });
    methods!(bar, {
        "statusItemWithLength:" => status_item_with_length, c"@24@0:8d16";
        "removeStatusItem:" => remove_status_item, c"v24@0:8@16";
        "thickness" => thickness, c"d16@0:8";
    });

    let item = objc::class::lookup(c"NSStatusItem").expect("declared before its methods");
    methods!(item, {
        "button" => nil, c"@16@0:8";
        "statusBar" => status_bar, c"@16@0:8";
        "menu" => menu, c"@16@0:8";
        "setMenu:" => set_menu, c"v24@0:8@16";
        "length" => length, c"d16@0:8";
        "setLength:" => set_length, c"v24@0:8d16";
        "isVisible" => is_visible, c"c16@0:8";
        "setVisible:" => set_visible, c"v20@0:8c16";
        "setHighlightMode:" => ignore_flag, c"v20@0:8c16";
        "setImage:" => ignore_object, c"v24@0:8@16";
        "setAlternateImage:" => ignore_object, c"v24@0:8@16";
        "setTitle:" => ignore_object, c"v24@0:8@16";
        "setToolTip:" => ignore_object, c"v24@0:8@16";
        "setTarget:" => ignore_object, c"v24@0:8@16";
        "setAction:" => ignore_object, c"v24@0:8:16";
        "setAutosaveName:" => ignore_object, c"v24@0:8@16";
        "dealloc" => dealloc, c"v16@0:8";
    });
}
