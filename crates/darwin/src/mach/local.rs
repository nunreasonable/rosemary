//! What a task does to its own memory, whoever asked: the caller itself, for
//! `mach_task_self()`, or another process through the server, in which case
//! this runs in the service signal's handler ([`super::service`]).
//!
//! So everything here is async-signal-safe - system calls, atomics and the
//! caller's buffers, no allocation and no locks - and answers with Darwin's
//! `kern_return_t`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use starnix::memory::{self, FaultKind};

use super::kern::*;

/// `VM_PROT_*`, the same bits as `PROT_*` on every host this runs on, but
/// stated rather than relied on.
pub const VM_PROT_READ: i32 = 1;
pub const VM_PROT_WRITE: i32 = 2;
pub const VM_PROT_EXECUTE: i32 = 4;
/// `VM_PROT_COPY`: make a private copy to write to. A private mapping here is
/// copied on write already.
const VM_PROT_COPY: i32 = 0x10;

/// `VM_FLAGS_*` that `vm_allocate` answers. `FIXED` is zero: the absence of
/// `ANYWHERE`.
pub const VM_FLAGS_ANYWHERE: i32 = 0x0001;
const VM_FLAGS_RANDOM_ADDR: i32 = 0x0008;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;
/// `VM_FLAGS_ALIAS_MASK`: the `VM_MAKE_TAG` label, which only feeds `vmmap`'s
/// statistics and has nothing to attach to on this host.
const VM_FLAGS_ALIAS_MASK: i32 = 0xff00_0000u32 as i32;

/// Changes the protection of `[address, address + len)` to host `PROT_*`
/// bits, answering 0 or an `errno`. The runtime replaces the plain `mprotect`
/// under emulation, where the code cache has a say in what is executable.
pub type ProtectHook = unsafe fn(address: u64, len: u64, prot: i32) -> i32;

/// Told of a write about to be made to `[address, address + len)`, so the
/// runtime can lift any guard of its own on those pages first.
pub type WriteHook = fn(address: u64, len: u64);

static PROTECT: AtomicUsize = AtomicUsize::new(0);
static BEFORE_WRITE: AtomicUsize = AtomicUsize::new(0);

/// Install the runtime's protection change. Must be async-signal-safe.
pub fn set_protect_hook(hook: ProtectHook) {
    PROTECT.store(hook as usize, Ordering::Release);
}

/// Install the runtime's write barrier. Must be async-signal-safe.
pub fn set_write_hook(hook: WriteHook) {
    BEFORE_WRITE.store(hook as usize, Ordering::Release);
}

fn page() -> u64 {
    super::info::page_size()
}

/// `[address, address + size)` widened to whole pages, as Mach widens every
/// range it maps, unmaps or protects. `None` when the end overflows.
fn pages(address: u64, size: u64) -> Option<(u64, u64)> {
    let mask = page() - 1;
    let start = address & !mask;
    let end = address.checked_add(size)?.checked_add(mask)? & !mask;
    Some((start, end - start))
}

fn fault_code(kind: FaultKind) -> i32 {
    match kind {
        FaultKind::Unmapped => KERN_INVALID_ADDRESS,
        FaultKind::Protection => KERN_PROTECTION_FAILURE,
    }
}

/// Copy `out.len()` bytes from `address`. All or nothing to the caller: a
/// read that stops part-way is a failure, as `vm_map_copyin`'s is.
pub fn read(address: u64, out: &mut [u8]) -> Result<(), i32> {
    if out.is_empty() {
        return Ok(());
    }
    if address.checked_add(out.len() as u64).is_none() {
        return Err(KERN_INVALID_ADDRESS);
    }
    memory::read(address as usize, out).map_err(|fault| fault_code(fault.kind))
}

/// Write `data` at `address`.
///
/// FIXME (ACCURACY): Darwin checks the whole destination before writing any of
/// it. Here the bytes before an unwritable page have been written when the
/// failure is reported.
///
/// # Safety
///
/// Whoever asked has a right to write there: it is guest memory the guest
/// named, and the guest is as entitled to break itself as it is on Darwin.
pub unsafe fn write(address: u64, data: &[u8]) -> Result<(), i32> {
    if data.is_empty() {
        return Ok(());
    }
    if address.checked_add(data.len() as u64).is_none() {
        return Err(KERN_INVALID_ADDRESS);
    }
    match BEFORE_WRITE.load(Ordering::Acquire) {
        0 => {}
        hook => {
            // SAFETY: only ever stored from a `WriteHook`.
            let hook: WriteHook = unsafe { std::mem::transmute::<usize, WriteHook>(hook) };
            hook(address, data.len() as u64);
        }
    }
    // SAFETY: the caller's obligation.
    unsafe { memory::write(address as usize, data) }.map_err(|fault| fault_code(fault.kind))
}

/// `vm_copy` within this task: the source as it was before the call, so an
/// overlapping copy is a `memmove`. A page-sized buffer at a time, from the
/// end that the overlap leaves safe.
///
/// # Safety
///
/// As [`write`].
pub unsafe fn copy(source: u64, size: u64, dest: u64) -> i32 {
    if size == 0 {
        return KERN_SUCCESS;
    }
    if source.checked_add(size).is_none() || dest.checked_add(size).is_none() {
        return KERN_INVALID_ADDRESS;
    }
    let mut buffer = [0u8; 4096];
    let step = buffer.len() as u64;
    let backwards = dest > source && dest < source + size;
    let mut done = 0;
    while done < size {
        let n = (size - done).min(step);
        let at = match backwards {
            true => size - done - n,
            false => done,
        };
        let chunk = &mut buffer[..n as usize];
        if let Err(code) = read(source + at, chunk) {
            return code;
        }
        // SAFETY: the caller's obligation.
        if let Err(code) = unsafe { write(dest + at, chunk) } {
            return code;
        }
        done += n;
    }
    KERN_SUCCESS
}

/// `vm_protect`. `set_maximum` asks to lower the most the range could ever be
/// made, which no host here records.
///
/// FIXME (ACCURACY): a maximum protection is accepted and not kept, so a later
/// `vm_protect` above it succeeds where Darwin's would fail.
pub fn protect(address: u64, size: u64, set_maximum: bool, protection: i32) -> i32 {
    if protection & !(VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE | VM_PROT_COPY) != 0 {
        return KERN_INVALID_ARGUMENT;
    }
    let Some((start, len)) = pages(address, size) else {
        return KERN_INVALID_ADDRESS;
    };
    if len == 0 || set_maximum {
        return KERN_SUCCESS;
    }
    let mut prot = libc::PROT_NONE;
    if protection & VM_PROT_READ != 0 {
        prot |= libc::PROT_READ;
    }
    if protection & VM_PROT_WRITE != 0 {
        prot |= libc::PROT_WRITE;
    }
    if protection & VM_PROT_EXECUTE != 0 {
        prot |= libc::PROT_EXEC;
    }
    let errno = match PROTECT.load(Ordering::Acquire) {
        0 => {
            // SAFETY: a protection change the guest asked for on its own pages.
            match unsafe { libc::mprotect(start as *mut c_void, len as usize, prot) } {
                0 => 0,
                _ => starnix::errno::get(),
            }
        }
        hook => {
            // SAFETY: only ever stored from a `ProtectHook`.
            let hook: ProtectHook = unsafe { std::mem::transmute::<usize, ProtectHook>(hook) };
            // SAFETY: as above.
            unsafe { hook(start, len, prot) }
        }
    };
    match errno {
        0 => KERN_SUCCESS,
        libc::EACCES => KERN_PROTECTION_FAILURE,
        libc::ENOMEM => KERN_INVALID_ADDRESS,
        _ => KERN_INVALID_ARGUMENT,
    }
}

/// `vm_allocate`: zero-filled anonymous memory, anywhere or at `address`.
pub fn allocate(address: u64, size: u64, flags: i32) -> Result<u64, i32> {
    // Purgeable, superpage, 4GB-chunk and the rest change what the memory is,
    // and pretending to honour them would hand back something else.
    if flags
        & !(VM_FLAGS_ANYWHERE | VM_FLAGS_RANDOM_ADDR | VM_FLAGS_OVERWRITE | VM_FLAGS_ALIAS_MASK)
        != 0
    {
        return Err(KERN_INVALID_ARGUMENT);
    }
    if size == 0 {
        return Ok(0);
    }
    let page = page();
    let Some(length) = size.checked_next_multiple_of(page) else {
        return Err(KERN_INVALID_ARGUMENT);
    };
    let anywhere = flags & VM_FLAGS_ANYWHERE != 0;
    let hint = match anywhere {
        true => 0,
        false => address & !(page - 1),
    };
    let fixed = match !anywhere && flags & VM_FLAGS_OVERWRITE != 0 {
        true => libc::MAP_FIXED,
        false => 0,
    };
    // SAFETY: a new anonymous mapping; `MAP_FIXED` only where the caller asked
    // to replace what is there.
    let mapped = unsafe {
        libc::mmap(
            hint as *mut c_void,
            length as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | fixed,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(KERN_NO_SPACE);
    }
    // A fixed request without `OVERWRITE` must not land anywhere else, and a
    // hint is all `mmap` will take without displacing a neighbour.
    if !anywhere && mapped as u64 != hint {
        // SAFETY: the mapping made above, which nothing else has seen.
        unsafe { libc::munmap(mapped, length as usize) };
        return Err(KERN_NO_SPACE);
    }
    Ok(mapped as u64)
}

/// `vm_deallocate`, over whole pages as Mach's is.
///
/// # Safety
///
/// The range is memory whoever asked is done with.
pub unsafe fn deallocate(address: u64, size: u64) -> i32 {
    // Mach accepts a zero-length deallocation and does nothing; `munmap` calls
    // it an error.
    if size == 0 {
        return KERN_SUCCESS;
    }
    let Some((start, len)) = pages(address, size) else {
        return KERN_INVALID_ADDRESS;
    };
    // SAFETY: the caller's obligation.
    match unsafe { libc::munmap(start as *mut c_void, len as usize) } {
        0 => { super::memory::forget(start, len); KERN_SUCCESS },
        _ => KERN_INVALID_ADDRESS,
    }
}

/// Fresh pages to hand a caller `size` bytes in, at the offset into the first
/// page that `address` has: where Mach's out-of-line copy puts the bytes, and
/// what `vm_deallocate(data, size)` frees again.
pub fn out_of_line(address: u64, size: u64) -> Result<(*mut u8, u64), i32> {
    let offset = address & (page() - 1);
    let Some(length) = offset.checked_add(size) else {
        return Err(KERN_INVALID_ARGUMENT);
    };
    let base = allocate(0, length, VM_FLAGS_ANYWHERE).map_err(|_| KERN_RESOURCE_SHORTAGE)?;
    Ok(((base + offset) as *mut u8, base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ranges_pages_cover_its_ends() {
        let page = page();
        assert_eq!(pages(page + 1, 1), Some((page, page)));
        assert_eq!(pages(page - 1, 2), Some((0, 2 * page)));
        assert_eq!(pages(u64::MAX - 1, 4), None);
    }

    #[test]
    fn copying_forwards_and_backwards_keeps_the_source_as_it_was() {
        let mut bytes: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let base = bytes.as_mut_ptr() as u64;
        let before = bytes.clone();
        assert_eq!(unsafe { copy(base, 8_000, base + 1_000) }, KERN_SUCCESS);
        assert_eq!(&bytes[1_000..9_000], &before[..8_000]);
        let before = bytes.clone();
        assert_eq!(unsafe { copy(base + 1_000, 8_000, base) }, KERN_SUCCESS);
        assert_eq!(&bytes[..8_000], &before[1_000..9_000]);
    }

    #[test]
    fn protection_bits_mach_does_not_have_are_refused() {
        assert_eq!(protect(0, 4096, false, 0x40), KERN_INVALID_ARGUMENT);
    }
}
