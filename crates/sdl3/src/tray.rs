//! The desktop's notification area (`SDL_CreateTray` and friends), which is
//! what an `NSStatusItem` becomes off macOS.
//!
//! Every call here must be made on the thread that initialized video, as SDL
//! requires. A menu entry's callback runs inside SDL's own event servicing, so
//! [`clicked`] only queues the click: the caller drains it at a point where
//! running a program's code cannot re-enter SDL.

use std::ffi::{CString, c_char, c_void};
use std::sync::{Mutex, OnceLock};

use super::{entry, library};

pub type Tray = *mut c_void;
pub type Menu = *mut c_void;
pub type Entry = *mut c_void;

/// `SDL_TrayEntryFlags`.
pub mod flags {
    pub const BUTTON: u32 = 0x0000_0001;
    pub const CHECKBOX: u32 = 0x0000_0002;
    pub const SUBMENU: u32 = 0x0000_0004;
    pub const DISABLED: u32 = 0x8000_0000;
    pub const CHECKED: u32 = 0x4000_0000;
}

/// `SDL_PIXELFORMAT_RGBA32` on a little-endian host, as in `video`.
const PIXELFORMAT_RGBA: u32 = 0x1676_2004;

type Callback = unsafe extern "C" fn(*mut c_void, Entry);

#[allow(non_snake_case)]
struct Api {
    SDL_CreateTray: unsafe extern "C" fn(*mut c_void, *const c_char) -> Tray,
    SDL_SetTrayIcon: unsafe extern "C" fn(Tray, *mut c_void),
    SDL_SetTrayTooltip: unsafe extern "C" fn(Tray, *const c_char),
    SDL_CreateTrayMenu: unsafe extern "C" fn(Tray) -> Menu,
    SDL_GetTrayMenu: unsafe extern "C" fn(Tray) -> Menu,
    SDL_CreateTraySubmenu: unsafe extern "C" fn(Entry) -> Menu,
    SDL_GetTrayEntries: unsafe extern "C" fn(Menu, *mut i32) -> *const Entry,
    SDL_RemoveTrayEntry: unsafe extern "C" fn(Entry),
    SDL_InsertTrayEntryAt: unsafe extern "C" fn(Menu, i32, *const c_char, u32) -> Entry,
    SDL_SetTrayEntryCallback: unsafe extern "C" fn(Entry, Option<Callback>, *mut c_void),
    SDL_DestroyTray: unsafe extern "C" fn(Tray),
    SDL_CreateSurfaceFrom: unsafe extern "C" fn(i32, i32, u32, *mut c_void, i32) -> *mut c_void,
    SDL_DestroySurface: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        let handle = library()?;
        macro_rules! resolve {
            ($($name:ident),* $(,)?) => {
                Api {
                    // SAFETY: each field's type is the prototype SDL3's
                    // `SDL_tray.h` and `SDL_surface.h` declare for that name.
                    $($name: unsafe { entry(handle, stringify!($name))? },)*
                }
            };
        }
        Some(resolve!(
            SDL_CreateTray,
            SDL_SetTrayIcon,
            SDL_SetTrayTooltip,
            SDL_CreateTrayMenu,
            SDL_GetTrayMenu,
            SDL_CreateTraySubmenu,
            SDL_GetTrayEntries,
            SDL_RemoveTrayEntry,
            SDL_InsertTrayEntryAt,
            SDL_SetTrayEntryCallback,
            SDL_DestroyTray,
            SDL_CreateSurfaceFrom,
            SDL_DestroySurface,
        ))
    })
    .as_ref()
}

/// Whether this SDL has trays at all; one older than 3.2 does not.
pub fn available() -> bool {
    api().is_some()
}

/// Run `f` with a surface over `width * height` RGBA pixels, or with null for
/// no pixels. SDL copies what it keeps, so the surface lives only for the call.
fn with_surface<R>(
    api: &Api,
    icon: Option<(u32, u32, &[u8])>,
    f: impl FnOnce(*mut c_void) -> R,
) -> R {
    let Some((width, height, pixels)) =
        icon.filter(|(w, h, p)| p.len() == (*w as usize) * (*h as usize) * 4 && *w > 0 && *h > 0)
    else {
        return f(std::ptr::null_mut());
    };
    let mut pixels = pixels.to_vec();
    // SAFETY: the buffer is `width * height * 4` bytes, as the format and
    // pitch say, and it outlives the surface.
    let surface = unsafe {
        (api.SDL_CreateSurfaceFrom)(
            width as i32,
            height as i32,
            PIXELFORMAT_RGBA,
            pixels.as_mut_ptr().cast(),
            width as i32 * 4,
        )
    };
    let out = f(surface);
    if !surface.is_null() {
        // SAFETY: made just above.
        unsafe { (api.SDL_DestroySurface)(surface) };
    }
    out
}

/// Put an icon in the notification area, with a menu ready to fill.
pub fn create(icon: Option<(u32, u32, &[u8])>, tooltip: &str) -> Option<Tray> {
    let api = api()?;
    let tooltip = CString::new(tooltip).unwrap_or_default();
    // SAFETY: a surface or null, and a NUL-terminated tooltip SDL copies.
    let tray = with_surface(api, icon, |s| unsafe {
        (api.SDL_CreateTray)(s, tooltip.as_ptr())
    });
    if tray.is_null() {
        return None;
    }
    // A tray with no menu is not shown by every desktop.
    // SAFETY: a tray just made.
    unsafe { (api.SDL_CreateTrayMenu)(tray) };
    Some(tray)
}

pub fn set_icon(tray: Tray, icon: Option<(u32, u32, &[u8])>) {
    let Some(api) = api() else { return };
    // SAFETY: a tray of [`create`]'s and a surface or null.
    with_surface(api, icon, |s| unsafe { (api.SDL_SetTrayIcon)(tray, s) });
}

pub fn set_tooltip(tray: Tray, tooltip: &str) {
    let Some(api) = api() else { return };
    let tooltip = CString::new(tooltip).unwrap_or_default();
    // SAFETY: a tray of [`create`]'s; SDL copies the text.
    unsafe { (api.SDL_SetTrayTooltip)(tray, tooltip.as_ptr()) };
}

pub fn destroy(tray: Tray) {
    let Some(api) = api() else { return };
    // SAFETY: a tray of [`create`]'s, not used again.
    unsafe { (api.SDL_DestroyTray)(tray) };
}

/// The tray's own menu.
pub fn menu(tray: Tray) -> Menu {
    let Some(api) = api() else {
        return std::ptr::null_mut();
    };
    // SAFETY: a tray of [`create`]'s, which made its menu.
    unsafe { (api.SDL_GetTrayMenu)(tray) }
}

/// Remove every entry of `menu`, submenus with them.
pub fn clear(menu: Menu) {
    let Some(api) = api() else { return };
    if menu.is_null() {
        return;
    }
    loop {
        let mut count = 0;
        // SAFETY: a live menu; the array is SDL's and read before the removal
        // that invalidates it.
        let first = unsafe {
            let entries = (api.SDL_GetTrayEntries)(menu, &mut count);
            if entries.is_null() || count <= 0 {
                return;
            }
            *entries
        };
        // SAFETY: an entry of this menu.
        unsafe { (api.SDL_RemoveTrayEntry)(first) };
    }
}

/// Append an entry; `None` for the label is a separator. A clicked entry is
/// queued with `tag`, for [`clicked`] to hand back.
pub fn append(menu: Menu, label: Option<&str>, flags: u32, tag: u64) -> Entry {
    let Some(api) = api() else {
        return std::ptr::null_mut();
    };
    if menu.is_null() {
        return std::ptr::null_mut();
    }
    let label = label.map(|l| CString::new(l.replace('\0', "")).unwrap_or_default());
    let text = label.as_ref().map_or(std::ptr::null(), |l| l.as_ptr());
    // SAFETY: a live menu, and a label SDL copies or null for a separator.
    let entry = unsafe { (api.SDL_InsertTrayEntryAt)(menu, -1, text, flags) };
    if !entry.is_null() && label.is_some() && flags & flags::SUBMENU == 0 {
        // SAFETY: an entry just made; the userdata is the tag itself.
        unsafe {
            (api.SDL_SetTrayEntryCallback)(entry, Some(on_click), tag as usize as *mut c_void)
        };
    }
    entry
}

/// The submenu of an entry made with [`flags::SUBMENU`].
pub fn submenu(entry: Entry) -> Menu {
    let Some(api) = api() else {
        return std::ptr::null_mut();
    };
    if entry.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a submenu entry of [`append`]'s.
    unsafe { (api.SDL_CreateTraySubmenu)(entry) }
}

static CLICKED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

unsafe extern "C" fn on_click(tag: *mut c_void, _entry: Entry) {
    CLICKED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tag as usize as u64);
}

/// The tags of the entries clicked since the last call, oldest first.
pub fn clicked() -> Vec<u64> {
    std::mem::take(&mut *CLICKED.lock().unwrap_or_else(|e| e.into_inner()))
}
