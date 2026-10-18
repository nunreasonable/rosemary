//! `tlv_get_addr`'s slow path: a thread's first touch of an image's thread-locals.

use core::ffi::c_void;
use std::sync::Mutex;

use super::registry::{self, Image};

/// A `tlv_descriptor`: `{thunk, key, offset}`.
#[repr(C)]
struct Descriptor {
    thunk: u64,
    key: u64,
    offset: u64,
}

/// This thread's address for the variable `descriptor` names, allocating and
/// constructing the image's block if this is the thread's first touch of it.
///
/// Reached only when `%gs:[key*8]` is null, or on every access where keys cannot be
/// read off the thread pointer. Guests call it through a register-preserving wrapper.
pub extern "C" fn tlv_allocate(descriptor: *const u8) -> *mut u8 {
    match address_with(descriptor, call_guest_init) {
        Ok(address) => address,
        Err(error) => {
            eprintln!("rosemary: tlv_get_addr({descriptor:p}): {error}");
            std::ptr::null_mut()
        }
    }
}

/// [`tlv_allocate`], with how a constructor is called left to the caller.
pub(super) fn address_with(
    descriptor: *const u8,
    call: impl FnMut(u64) -> Result<(), String>,
) -> Result<*mut u8, String> {
    if descriptor.is_null() {
        return Ok(std::ptr::null_mut());
    }
    // SAFETY: a descriptor in the guest's `__thread_vars`, identity-mapped and
    // eight-byte aligned, which `register` rewrote and which lives as long as
    // the image.
    let d = unsafe { &*(descriptor as *const Descriptor) };
    let Some(image) = u32::try_from(d.key).ok().and_then(registry::by_key) else {
        say_unregistered(descriptor as usize as u64);
        return Ok(std::ptr::null_mut());
    };
    let block = block_for(&image, call)?;
    if block.is_null() {
        return Ok(block);
    }
    // SAFETY: `register` rewrote only descriptors whose offset is inside the block.
    Ok(unsafe { block.add(d.offset as usize) })
}

/// This thread's block for `image`, made on first touch.
///
/// The key is set **before** the constructors run, as dyld's
/// `tlv_allocate_and_initialize_for_key` does, so a constructor reading another
/// variable of the same image takes the fast path to this same block.
fn block_for(
    image: &Image,
    mut call: impl FnMut(u64) -> Result<(), String>,
) -> Result<*mut u8, String> {
    let existing = image.key.get() as *mut u8;
    if !existing.is_null() {
        return Ok(existing);
    }
    let index = image.key.index();
    if super::state::failed(index) {
        return Ok(std::ptr::null_mut());
    }
    let block = allocate(image).ok_or("out of memory for a thread-local block")?;
    if std::env::var_os("ROSEMARY_TRACE_TLV").is_some() {
        eprintln!(
            "rosemary: tlv: thread {} key {index} gets block {block:p} ({} bytes)",
            darwin::pthread::id(),
            image.total_size,
        );
    }
    image.key.set(block as *mut c_void);

    let constructed = image.init_funcs.iter().rev().try_for_each(|&f| call(f));
    if let Err(error) = constructed {
        // Taken back rather than left half-built: a later access on this thread
        // reads null, and nothing reruns a constructor that failed.
        image.key.set(std::ptr::null_mut());
        // SAFETY: allocated above and no longer reachable from the key.
        unsafe { libc::free(block as *mut c_void) };
        super::state::mark_failed(index);
        return Err(error);
    }
    Ok(block)
}

/// A block for `image`: the initialised prefix copied from the image, the rest zero.
fn allocate(image: &Image) -> Option<*mut u8> {
    let mut block: *mut c_void = std::ptr::null_mut();
    let align = image.align.max(size_of::<usize>());
    // SAFETY: a live out-pointer; `align` is a power of two and a multiple of the
    // pointer size, and `total_size` is non-zero - `registry::layout` refuses zero.
    if unsafe { libc::posix_memalign(&mut block, align, image.total_size) } != 0 {
        return None;
    }
    let block = block as *mut u8;
    // SAFETY: `block` has `total_size` bytes. `initial_va` is a slid VA inside the
    // guest's own mapped `__thread_data`, live while the image is loaded, and guest
    // addresses are host addresses; `initial_size` was clamped to `total_size`.
    unsafe {
        std::ptr::write_bytes(block, 0, image.total_size);
        if image.initial_size > 0 {
            std::ptr::copy_nonoverlapping(
                image.initial_va as usize as *const u8,
                block,
                image.initial_size,
            );
        }
    }
    Some(block)
}

/// The active backend runs guest constructors with no arguments.
fn call_guest_init(at: u64) -> Result<(), String> {
    let exec = crate::exec::active().ok_or("no execution backend is active")?;
    exec.call(at, &[])
        .map(|_| ())
        .map_err(|error| format!("thread-local constructor at {at:#x}: {error}"))
}

/// `__tlv_bootstrap`: what a descriptor `register` declined still points at.
///
/// Null is the only value a C ABI with no error channel can return, and the guest's
/// very next instruction dereferences it - so the descriptor is named here, or the
/// failure arrives as a segfault inside guest code with nothing naming a thread-local.
pub extern "C" fn tlv_bootstrap(descriptor: *const u8) -> *mut u8 {
    say_unregistered(descriptor as usize as u64);
    std::ptr::null_mut()
}

/// Name a descriptor this runtime has no block for, once each.
fn say_unregistered(at: u64) {
    static SAID: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&at) {
        return;
    }
    said.push(at);
    eprintln!(
        "rosemary: tlv_get_addr({at:#x}): no thread-local registered at that descriptor \
         - the guest will read address zero"
    );
}
