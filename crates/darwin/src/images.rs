//! What the loader knows, for the framework crates beneath it.
//!
//! Images are loaded by the runtime crate, which depends on every framework
//! crate and so cannot be called by them directly. `CFBundle` still has to load
//! an executable and find a function in it, and the File Manager has to know
//! that `/System/Library/Frameworks/AppKit.framework` exists although no file
//! is there: the frameworks this runtime provides are libraries it *is*, not
//! files it maps. The runtime installs a [`Loader`] at startup to answer both.

use std::sync::OnceLock;

/// The loader's answers, as the runtime installs them.
pub struct Loader {
    /// Whether a guest path names a directory that exists only because a
    /// framework the runtime provides lives there - `/System/Library/Frameworks`
    /// and each `X.framework` in it.
    pub virtual_directory: fn(&str) -> bool,
    /// `dlopen` of a guest path, or of the main program for `None`, as a handle.
    pub open: fn(Option<&str>) -> Option<u64>,
    /// `dlsym` on a handle [`Loader::open`] returned, as a guest-callable
    /// address. Takes the C name, without the Mach-O underscore.
    pub symbol: fn(u64, &str) -> Option<u64>,
    /// The host path of the guest image an address lies in, the main program
    /// included.
    pub image_path: fn(u64) -> Option<String>,
    /// Host paths of loaded guest images, including startup dependencies.
    pub image_paths: fn() -> Vec<String>,
}

static LOADER: OnceLock<Loader> = OnceLock::new();

/// The main program's SDK version, `xxxx.yy.zz` in nibbles; zero for unknown.
static PROGRAM_SDK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Record the SDK the main program was linked against, as its load commands
/// say.
pub fn set_program_sdk(version: u32) {
    PROGRAM_SDK.store(version, std::sync::atomic::Ordering::Release);
}

/// The SDK the main program was linked against, as
/// `dyld_get_program_sdk_version` answers it, or `None` when the runtime did
/// not find one - a unit test, or a program whose linker wrote none.
pub fn program_sdk() -> Option<u32> {
    match PROGRAM_SDK.load(std::sync::atomic::Ordering::Acquire) {
        0 => None,
        version => Some(version),
    }
}

/// Install the runtime's answers. The first call wins.
pub fn install(loader: Loader) {
    let _ = LOADER.set(loader);
}

/// Whether `path` is a directory only a provided framework makes exist. False
/// before a loader is installed, which is what a unit test sees.
pub fn virtual_directory(path: &str) -> bool {
    LOADER
        .get()
        .is_some_and(|loader| (loader.virtual_directory)(path))
}

/// Load the image at the guest path `path`, or name the main program for
/// `None`.
pub fn open(path: Option<&str>) -> Option<u64> {
    (LOADER.get()?.open)(path)
}

/// The host path of the guest image containing `address`.
pub fn image_path(address: u64) -> Option<String> {
    (LOADER.get()?.image_path)(address)
}

pub fn image_paths() -> Vec<String> {
    LOADER.get().map_or_else(Vec::new, |loader| (loader.image_paths)())
}

/// Find `name` in the image `handle` names.
pub fn symbol(handle: u64, name: &str) -> Option<u64> {
    (LOADER.get()?.symbol)(handle, name)
}
