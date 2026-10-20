//! Named memory entries and the mapping calls that use them.
//!
//! A memory entry is Mach's handle to shareable memory: made with
//! `mach_make_memory_entry_64` and mapped with `mach_vm_map`, by this task or
//! by another the right is sent to. Here it is a memory file, which is what
//! Linux shares memory through, held behind a send right in the port space.
//!
//! An entry can also be made for memory already mapped (`MAP_MEM_VM_SHARE`),
//! which is how Chromium turns a writable region into a read-only one: when
//! the range is a mapping of an entry, the new entry names the same file.
//!
//! FIXME (ACCURACY): any other range is refused - anonymous memory on Linux
//! cannot be given a second name after the fact.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use super::kern::*;
use super::space::{self, Name, Right, TASK_SELF};

/// `MAP_MEM_NAMED_CREATE`: make new memory rather than name existing memory.
const MAP_MEM_NAMED_CREATE: i32 = 0x0002_0000;

/// `VM_FLAGS_ANYWHERE`: place the mapping where there is room.
const VM_FLAGS_ANYWHERE: i32 = 0x1;

const VM_PROT_READ: i32 = 0x1;
const VM_PROT_WRITE: i32 = 0x2;
const VM_PROT_EXECUTE: i32 = 0x4;

const PAGE: u64 = 4096;

/// Where each entry was mapped: start, length, the file and its offset there.
static MAPPED: std::sync::Mutex<Vec<(u64, u64, Arc<OwnedFd>, u64)>> =
    std::sync::Mutex::new(Vec::new());

/// Drop what is known of mappings inside `[address, address + size)`, which
/// have just been unmapped.
pub(crate) fn forget(address: u64, size: u64) {
    let end = address.saturating_add(size);
    MAPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(start, length, _, _)| *start + *length <= address || *start >= end);
}

/// The file and offset behind `address`, if it is in a mapping of an entry,
/// and how much of that mapping lies from there on.
fn mapped_at(address: u64) -> Option<(Arc<OwnedFd>, u64, u64)> {
    MAPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(start, length, _, _)| (*start..*start + *length).contains(&address))
        .map(|(start, length, file, offset)| {
            (file.clone(), offset + (address - start), start + length - address)
        })
}

/// The same memory file, open for reading only.
fn reopen_read_only(file: &OwnedFd) -> Option<OwnedFd> {
    let path = format!("/proc/self/fd/{}\0", file.as_raw_fd());
    // SAFETY: a NUL-terminated path naming this process's own descriptor.
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    // SAFETY: just opened, and owned by nothing else.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn round_page(size: u64) -> u64 {
    size.div_ceil(PAGE) * PAGE
}

fn host_prot(prot: i32) -> i32 {
    let mut host = libc::PROT_NONE;
    if prot & VM_PROT_READ != 0 {
        host |= libc::PROT_READ;
    }
    if prot & VM_PROT_WRITE != 0 {
        host |= libc::PROT_WRITE;
    }
    if prot & VM_PROT_EXECUTE != 0 {
        host |= libc::PROT_EXEC;
    }
    host
}

/// `kern_return_t mach_make_memory_entry_64(vm_map_t, memory_object_size_t
/// *size, memory_object_offset_t offset, vm_prot_t permission,
/// mach_port_t *object_handle, mem_entry_name_port_t parent_entry)`.
///
/// # Safety
///
/// `size` and `handle` are writable.
pub unsafe extern "C" fn mach_make_memory_entry_64(
    task: Name,
    size: *mut u64,
    offset: u64,
    permission: i32,
    handle: *mut Name,
    parent: Name,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if size.is_null() || handle.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller's.
    let wanted = round_page(unsafe { size.read() });
    let (file, offset, length) = if permission & MAP_MEM_NAMED_CREATE != 0 {
        // SAFETY: a fresh, unnamed memory file.
        let fd = unsafe { libc::memfd_create(c"mach-memory-entry".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return KERN_RESOURCE_SHORTAGE;
        }
        // SAFETY: a descriptor this call just made and owns.
        let file = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: an open memory file.
        if unsafe { libc::ftruncate(file.as_raw_fd(), wanted as libc::off_t) } != 0 {
            return KERN_RESOURCE_SHORTAGE;
        }
        (Arc::new(file), 0, wanted)
    } else if parent == space::MACH_PORT_NULL
        && let Some((file, base, rest)) = mapped_at(offset)
    {
        // `offset` is an address in this task, inside a mapping of an entry.
        (file, base, if wanted == 0 { rest } else { wanted.min(rest) })
    } else if let Some((file, base, length)) = space::memory(parent) {
        // A sub-range of the parent entry.
        if offset > length {
            return KERN_INVALID_ARGUMENT;
        }
        let rest = length - offset;
        (file, base + offset, if wanted == 0 { rest } else { wanted.min(rest) })
    } else {
        return KERN_INVALID_ARGUMENT;
    };
    // An entry without write permission names the memory read-only, and a
    // read-only descriptor is what makes that hold: a writable mapping of it
    // fails here and, once the right is sent, in any other process too.
    let file = match permission & VM_PROT_WRITE {
        0 => match reopen_read_only(&file) {
            Some(read_only) => Arc::new(read_only),
            None => return KERN_FAILURE,
        },
        _ => file,
    };
    let name = space::insert(Right::Memory {
        file,
        offset,
        size: length,
        refs: 1,
    });
    // SAFETY: both writable, per the caller.
    unsafe {
        size.write(length);
        handle.write(name);
    }
    KERN_SUCCESS
}

/// `kern_return_t mach_vm_map(vm_map_t, mach_vm_address_t *address,
/// mach_vm_size_t size, mach_vm_offset_t mask, int flags, mem_entry_name_port_t
/// object, memory_object_offset_t offset, boolean_t copy, vm_prot_t cur,
/// vm_prot_t max, vm_inherit_t)`, and `vm_map`, which is the same on a 64-bit
/// task.
///
/// A null object maps fresh zeroed memory, aligned to `mask + 1`.
///
/// # Safety
///
/// `address` is writable; a fixed placement replaces what is there, as Mach's
/// `VM_FLAGS_OVERWRITE` does, so the caller must own that range.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn mach_vm_map(
    task: Name,
    address: *mut u64,
    size: u64,
    mask: u64,
    flags: i32,
    object: Name,
    offset: u64,
    copy: u32,
    current: i32,
    _maximum: i32,
    _inheritance: u32,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if address.is_null() || size == 0 {
        return KERN_INVALID_ARGUMENT;
    }
    let size = round_page(size);
    let prot = host_prot(current);
    let anywhere = flags & VM_FLAGS_ANYWHERE != 0;
    // SAFETY: the caller's.
    let requested = unsafe { address.read() };
    let mut backing = None;
    let (fd, file_offset, share) = match object {
        space::MACH_PORT_NULL => (-1, 0, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS),
        name => match space::memory(name) {
            Some((file, base, length)) => {
                if offset + size > length {
                    return KERN_INVALID_ARGUMENT;
                }
                let share = match copy {
                    0 => libc::MAP_SHARED,
                    _ => libc::MAP_PRIVATE,
                };
                // The descriptor outlives the mapping by the `Arc` the right
                // holds; `mmap` keeps the memory itself alive past both.
                let raw = file.as_raw_fd();
                if copy == 0 {
                    backing = Some((file, base + offset));
                }
                (raw, base + offset, share)
            }
            None => return KERN_INVALID_ARGUMENT,
        },
    };
    let alignment = mask.saturating_add(1).max(PAGE);
    let mapped = if anywhere {
        // Over-allocate to align, then trim, when the mask asks for more than
        // a page.
        let span = size + if alignment > PAGE { alignment } else { 0 };
        // SAFETY: a fresh mapping wherever the host puts it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                span as usize,
                prot,
                share,
                fd,
                file_offset as libc::off_t,
            )
        };
        if base == libc::MAP_FAILED {
            return map_failure();
        }
        let base = base as u64;
        if alignment <= PAGE {
            base
        } else if fd < 0 {
            let aligned = base.div_ceil(alignment) * alignment;
            // SAFETY: trimming the over-allocation this call made.
            unsafe {
                if aligned > base {
                    libc::munmap(base as *mut libc::c_void, (aligned - base) as usize);
                }
                let end = base + span;
                if end > aligned + size {
                    libc::munmap((aligned + size) as *mut libc::c_void, (end - aligned - size) as usize);
                }
            }
            aligned
        } else {
            // A file mapping cannot be trimmed onto an aligned start without
            // changing its offset, so it is placed again at the aligned
            // address inside the span reserved above.
            let aligned = base.div_ceil(alignment) * alignment;
            // SAFETY: replacing part of the span this call reserved.
            let placed = unsafe {
                libc::mmap(
                    aligned as *mut libc::c_void,
                    size as usize,
                    prot,
                    share | libc::MAP_FIXED,
                    fd,
                    file_offset as libc::off_t,
                )
            };
            // SAFETY: the rest of the reservation.
            unsafe {
                if aligned > base {
                    libc::munmap(base as *mut libc::c_void, (aligned - base) as usize);
                }
                let end = base + span;
                if end > aligned + size {
                    libc::munmap((aligned + size) as *mut libc::c_void, (end - aligned - size) as usize);
                }
            }
            if placed == libc::MAP_FAILED {
                return KERN_NO_SPACE;
            }
            aligned
        }
    } else {
        // SAFETY: the caller's range, per the contract above.
        let placed = unsafe {
            libc::mmap(
                requested as *mut libc::c_void,
                size as usize,
                prot,
                share | libc::MAP_FIXED,
                fd,
                file_offset as libc::off_t,
            )
        };
        if placed == libc::MAP_FAILED {
            return map_failure();
        }
        requested
    };
    if let Some((file, at)) = backing {
        forget(mapped, size);
        MAPPED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((mapped, size, file, at));
    }
    // SAFETY: writable, per the caller.
    unsafe { address.write(mapped) };
    KERN_SUCCESS
}

/// What a failed `mmap` means to a Mach caller. Asking for more access than
/// an entry grants is `KERN_INVALID_RIGHT`, which is XNU's answer and the one
/// a caller tests to learn whether a handle is read-only; anything else is a
/// lack of room.
fn map_failure() -> i32 {
    match crate::errno::get() {
        libc::EACCES | libc::EPERM => KERN_INVALID_RIGHT,
        _ => KERN_NO_SPACE,
    }
}

/// `kern_return_t mach_vm_protect(vm_map_t, mach_vm_address_t, mach_vm_size_t,
/// boolean_t set_maximum, vm_prot_t)`, and `vm_protect`. A maximum is not
/// kept: lowering it changes nothing until a later call would raise past it.
///
/// # Safety
///
/// The range is the caller's own.
pub unsafe extern "C" fn mach_vm_protect(
    task: Name,
    address: u64,
    size: u64,
    set_maximum: u32,
    protection: i32,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if set_maximum != 0 {
        return KERN_SUCCESS;
    }
    let start = address & !(PAGE - 1);
    let end = round_page(address + size);
    // SAFETY: the caller's range.
    match unsafe {
        libc::mprotect(start as *mut libc::c_void, (end - start) as usize, host_prot(protection))
    } {
        0 => KERN_SUCCESS,
        _ => KERN_PROTECTION_FAILURE,
    }
}

/// `KERN_PROTECTION_FAILURE`.
const KERN_PROTECTION_FAILURE: i32 = 2;

/// Ask the kernel to copy rather than dereferencing untrusted guest addresses.
#[cfg(target_os = "linux")]
fn copy_memory(source: u64, target: u64, size: u64, write: bool) -> i32 {
    if size == 0 {
        return KERN_SUCCESS;
    }
    if size > isize::MAX as u64
        || source.checked_add(size).is_none()
        || target.checked_add(size).is_none()
    {
        return KERN_INVALID_ARGUMENT;
    }
    let local = libc::iovec {
        iov_base: if write { source } else { target } as *mut libc::c_void,
        iov_len: size as usize,
    };
    let remote = libc::iovec {
        iov_base: if write { target } else { source } as *mut libc::c_void,
        iov_len: size as usize,
    };
    // SAFETY: both descriptors are local; the kernel validates their ranges.
    let copied = unsafe {
        if write {
            libc::process_vm_writev(libc::getpid(), &local, 1, &remote, 1, 0)
        } else {
            libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0)
        }
    };
    if copied == size as isize {
        KERN_SUCCESS
    } else {
        KERN_INVALID_ADDRESS
    }
}

#[cfg(not(target_os = "linux"))]
fn copy_memory(_source: u64, _target: u64, _size: u64, _write: bool) -> i32 {
    46 // KERN_NOT_SUPPORTED
}

/// Copy this task's memory into caller-owned storage.
///
/// # Safety
///
/// `out_size` must be writable; source and destination are kernel-validated.
pub unsafe extern "C" fn mach_vm_read_overwrite(
    task: Name,
    address: u64,
    size: u64,
    destination: u64,
    out_size: *mut u64,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if out_size.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    let result = copy_memory(address, destination, size, false);
    if result == KERN_SUCCESS {
        unsafe { out_size.write_unaligned(size) };
    }
    result
}

/// Write bytes into this task without bypassing page write protection.
pub unsafe extern "C" fn mach_vm_write(task: Name, address: u64, data: u64, count: u32) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    copy_memory(data, address, u64::from(count), true)
}

#[cfg(all(test, target_os = "linux"))]
mod transfer_tests {
    use super::*;

    #[test]
    fn memory_transfers_copy_and_report_bytes() {
        let source = [1u8, 2, 3, 4];
        let mut target = [0u8; 4];
        let mut count = 0;
        unsafe {
            assert_eq!(
                mach_vm_read_overwrite(
                    TASK_SELF,
                    source.as_ptr() as u64,
                    4,
                    target.as_mut_ptr() as u64,
                    &mut count,
                ),
                KERN_SUCCESS
            );
            assert_eq!(count, 4);
            assert_eq!(source, target);
            target.fill(0);
            assert_eq!(
                mach_vm_write(
                    TASK_SELF,
                    target.as_mut_ptr() as u64,
                    source.as_ptr() as u64,
                    4,
                ),
                KERN_SUCCESS
            );
            assert_eq!(source, target);
        }
    }

    #[test]
    fn inaccessible_memory_returns_an_error_without_faulting() {
        unsafe {
            let page = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(page, libc::MAP_FAILED);
            let byte = 1u8;
            assert_ne!(
                mach_vm_write(TASK_SELF, page as u64, &byte as *const u8 as u64, 1),
                KERN_SUCCESS
            );
            let mut target = 0u8;
            let mut count = 99;
            assert_ne!(
                mach_vm_read_overwrite(TASK_SELF, 0, 1, &mut target as *mut u8 as u64, &mut count),
                KERN_SUCCESS
            );
            assert_eq!(count, 99);
            assert_eq!(
                mach_vm_read_overwrite(0, 0, 0, 0, &mut count),
                KERN_INVALID_TASK
            );
            libc::munmap(page, 4096);
        }
    }
}

/// `kern_return_t mach_vm_read(vm_map_t, mach_vm_address_t, mach_vm_size_t,
/// vm_offset_t *data, mach_msg_type_number_t *count)`: a copy of this task's
/// own memory, in pages the caller frees with `vm_deallocate`.
///
/// # Safety
///
/// The range is readable; `data` and `count` are writable.
pub unsafe extern "C" fn mach_vm_read(
    task: Name,
    address: u64,
    size: u64,
    data: *mut u64,
    count: *mut u32,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if data.is_null() || count.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    let mut copy = 0u64;
    // SAFETY: a fresh allocation for the copy.
    let status = unsafe { super::vm::vm_allocate(TASK_SELF, &mut copy, size, VM_FLAGS_ANYWHERE) };
    if status != KERN_SUCCESS {
        return status;
    }
    // SAFETY: the caller's readable range, into the pages just allocated.
    unsafe {
        std::ptr::copy_nonoverlapping(address as *const u8, copy as *mut u8, size as usize);
        data.write(copy);
        count.write(size as u32);
    }
    KERN_SUCCESS
}

/// `kern_return_t mach_vm_remap(vm_map_t target, mach_vm_address_t *address,
/// mach_vm_size_t, mach_vm_offset_t mask, int flags, vm_map_t source,
/// mach_vm_address_t source_address, boolean_t copy, vm_prot_t *cur,
/// vm_prot_t *max, vm_inherit_t)`.
///
/// A copy is made by copying. A shared remap is possible only of memory that
/// is already shared - a mapping of a memory entry - which Linux can map a
/// second time; anything else is refused rather than silently copied, since a
/// caller sharing memory would then write into one copy and read the other.
///
/// # Safety
///
/// As `mach_vm_map`; the source range is the caller's.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn mach_vm_remap(
    target: Name,
    address: *mut u64,
    size: u64,
    mask: u64,
    flags: i32,
    source: Name,
    source_address: u64,
    copy: u32,
    current: *mut i32,
    maximum: *mut i32,
    inheritance: u32,
) -> i32 {
    if target != TASK_SELF || source != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if address.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    let prot = VM_PROT_READ | VM_PROT_WRITE;
    if copy == 0 {
        // SAFETY: `mremap` of a shared mapping with an old size of zero makes
        // a second mapping of the same pages; of anything else it fails.
        let moved = unsafe {
            libc::mremap(
                source_address as *mut libc::c_void,
                0,
                round_page(size) as usize,
                libc::MREMAP_MAYMOVE,
            )
        };
        if moved == libc::MAP_FAILED {
            return KERN_FAILURE;
        }
        // SAFETY: writable, per the caller.
        unsafe { address.write(moved as u64) };
    } else {
        // SAFETY: fresh memory, then the caller's range copied into it.
        let status = unsafe {
            mach_vm_map(
                TASK_SELF,
                address,
                size,
                mask,
                flags,
                space::MACH_PORT_NULL,
                0,
                0,
                prot,
                prot,
                inheritance,
            )
        };
        if status != KERN_SUCCESS {
            return status;
        }
        // SAFETY: both ranges are `size` long and do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source_address as *const u8,
                address.read() as *mut u8,
                size as usize,
            )
        };
    }
    // SAFETY: writable when given.
    unsafe {
        if !current.is_null() {
            current.write(prot);
        }
        if !maximum.is_null() {
            maximum.write(prot | VM_PROT_EXECUTE);
        }
    }
    KERN_SUCCESS
}
