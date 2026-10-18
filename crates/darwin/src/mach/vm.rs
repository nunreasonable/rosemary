use std::ffi::c_void;

use starnix::process::Region;

use super::kern::*;
use super::space::{MACH_PORT_NULL, Name, TASK_SELF};

/// `VM_REGION_BASIC_INFO_64`, the only flavour with a layout this tree can
/// promise. The 32-bit `VM_REGION_BASIC_INFO` is a different structure and is
/// refused rather than filled with this one.
const VM_REGION_BASIC_INFO_64: i32 = 9;
/// `VM_REGION_EXTENDED_INFO`, what `ps` and `vmmap` read sharing and residency
/// from.
const VM_REGION_EXTENDED_INFO: i32 = 10;
/// `VM_REGION_TOP_INFO`, what `top` sorts a task's memory into private and
/// shared with.
const VM_REGION_TOP_INFO: i32 = 12;

/// `SM_PRIVATE` and `SM_SHARED`, the two sharing modes a mapping here can be.
const SM_PRIVATE: u8 = 2;
const SM_SHARED: u8 = 3;

/// `VM_PROT_*`, which happen to be the same bits as `PROT_*` - stated rather
/// than relied on, because nothing makes two operating systems agree.
const VM_PROT_READ: i32 = 1;
const VM_PROT_WRITE: i32 = 2;
const VM_PROT_EXECUTE: i32 = 4;

/// `VM_INHERIT_COPY`, which is `VM_INHERIT_DEFAULT` and what a private mapping
/// on this host does across a fork.
const VM_INHERIT_COPY: u32 = 1;
/// `VM_INHERIT_SHARE`.
const VM_INHERIT_SHARE: u32 = 0;

/// `VM_BEHAVIOR_DEFAULT`.
const VM_BEHAVIOR_DEFAULT: i32 = 0;

/// `struct vm_region_basic_info_64`, whose layout is ABI: the guest declared
/// one on its stack and reads the fields back by name.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BasicInfo64 {
    protection: i32,
    max_protection: i32,
    inheritance: u32,
    shared: i32,
    reserved: i32,
    offset: u64,
    behavior: i32,
    user_wired_count: u16,
}

const _: () = {
    // `VM_REGION_BASIC_INFO_COUNT_64` is this size in 32-bit words, and a
    // caller passes it in as the capacity of its buffer.
    assert!(size_of::<BasicInfo64>() == 40);
};

/// `struct vm_region_top_info`. Resident page counts are not tracked and read
/// zero, as in the submap flavour below.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct TopInfo {
    obj_id: u32,
    ref_count: u32,
    private_pages_resident: u32,
    shared_pages_resident: u32,
    share_mode: u8,
}

const _: () = {
    // `VM_REGION_TOP_INFO_COUNT` is five words, and rounds the last byte up.
    assert!(size_of::<TopInfo>() == 20);
};

/// `struct vm_region_extended_info`, without the trailing `pages_reusable`
/// that newer SDKs append. Resident page counts are not tracked and read zero.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
struct ExtendedInfo {
    protection: i32,
    user_tag: u32,
    pages_resident: u32,
    pages_shared_now_private: u32,
    pages_swapped_out: u32,
    pages_dirtied: u32,
    ref_count: u32,
    shadow_depth: u16,
    external_pager: u8,
    share_mode: u8,
}

const _: () = {
    // 8 words, and `VM_REGION_EXTENDED_INFO_COUNT` is 9 with `pages_reusable`.
    assert!(size_of::<ExtendedInfo>() == 32);
};

/// The first mapping ending above `address`, which is Mach's own search: the
/// one containing it, or the next one up if there is a hole there.
fn region_at_or_after(address: u64) -> Option<Region> {
    starnix::process::regions()
        .ok()?
        .into_iter()
        .find(|region| region.end > address)
}

/// A region's `VM_PROT_*` bits.
fn protection(region: &Region) -> i32 {
    let mut protection = 0;
    if region.read {
        protection |= VM_PROT_READ;
    }
    if region.write {
        protection |= VM_PROT_WRITE;
    }
    if region.execute {
        protection |= VM_PROT_EXECUTE;
    }
    protection
}

/// Say once that a flavour went unanswered, because the alternative is a
/// caller quietly told its own task has no regions.
fn unsupported(flavor: i32) {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        eprintln!(
            "rosemary: vm_region_64: flavour {flavor} has a layout this tree does not \
             publish; only VM_REGION_BASIC_INFO_64 is answered"
        );
    });
}

/// `kern_return_t vm_region_64(vm_map_t target_task, vm_address_t *address,
/// vm_size_t *size, vm_region_flavor_t flavor, vm_region_info_t info,
/// mach_msg_type_number_t *infoCnt, mach_port_t *object_name)`
///
/// # Safety
///
/// `address` and `size` are writable words, `info` is writable for `*count`
/// 32-bit words, `count` is writable, and `object_name` is null or writable.
pub unsafe extern "C" fn vm_region_64(
    task: Name,
    address: *mut u64,
    size: *mut u64,
    flavor: i32,
    info: *mut c_void,
    count: *mut u32,
    object_name: *mut Name,
) -> i32 {
    if address.is_null() || size.is_null() || info.is_null() || count.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    if task != TASK_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    if flavor == VM_REGION_TOP_INFO {
        // SAFETY: the caller's obligations, as below.
        return unsafe { top_info(address, size, info, count, object_name) };
    }
    if flavor == VM_REGION_EXTENDED_INFO {
        // SAFETY: the caller's obligations, as below.
        return unsafe { extended_info(address, size, info, count, object_name) };
    }
    if flavor != VM_REGION_BASIC_INFO_64 {
        unsupported(flavor);
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller states `count` is readable.
    if (unsafe { *count } as usize) * size_of::<u32>() < size_of::<BasicInfo64>() {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller states `address` is readable.
    let Some(region) = region_at_or_after(unsafe { *address }) else {
        // Past the last mapping. Mach says this with `KERN_INVALID_ADDRESS`,
        // and a walk stops on it.
        return KERN_INVALID_ADDRESS;
    };
    let basic = BasicInfo64 {
        protection: protection(&region),
        // Nothing here records what a mapping *could* be made, and reporting
        // less than the current protection would be a contradiction.
        max_protection: protection(&region),
        inheritance: match region.shared {
            true => VM_INHERIT_SHARE,
            false => VM_INHERIT_COPY,
        },
        shared: i32::from(region.shared),
        reserved: 0,
        offset: region.offset,
        behavior: VM_BEHAVIOR_DEFAULT,
        user_wired_count: 0,
    };
    // SAFETY: the caller's obligations, with the buffer's capacity checked
    // above. Unaligned because Mach's structures are packed to four bytes.
    unsafe {
        *address = region.start;
        *size = region.end - region.start;
        info.cast::<BasicInfo64>().write_unaligned(basic);
        *count = (size_of::<BasicInfo64>() / size_of::<u32>()) as u32;
        if !object_name.is_null() {
            // A name for the memory object behind the region. There is no port
            // namespace entry for one, and null is what Mach returns for a
            // region with no pager.
            *object_name = MACH_PORT_NULL;
        }
    }
    KERN_SUCCESS
}

/// [`vm_region_64`]'s `VM_REGION_TOP_INFO`.
///
/// # Safety
///
/// As [`vm_region_64`].
unsafe fn top_info(
    address: *mut u64,
    size: *mut u64,
    info: *mut c_void,
    count: *mut u32,
    object_name: *mut Name,
) -> i32 {
    // SAFETY: the caller states `count` is readable.
    if (unsafe { *count } as usize) * size_of::<u32>() < size_of::<TopInfo>() {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller states `address` is readable.
    let Some(region) = region_at_or_after(unsafe { *address }) else {
        return KERN_INVALID_ADDRESS;
    };
    let top = TopInfo {
        ref_count: 1,
        share_mode: match region.shared {
            true => SM_SHARED,
            false => SM_PRIVATE,
        },
        ..TopInfo::default()
    };
    // SAFETY: the caller's obligations, with the capacity checked above.
    unsafe {
        *address = region.start;
        *size = region.end - region.start;
        info.cast::<TopInfo>().write_unaligned(top);
        *count = (size_of::<TopInfo>() / size_of::<u32>()) as u32;
        if !object_name.is_null() {
            *object_name = MACH_PORT_NULL;
        }
    }
    KERN_SUCCESS
}

/// [`vm_region_64`]'s `VM_REGION_EXTENDED_INFO`. A caller built against a newer
/// SDK offers one more word for `pages_reusable`, which is zeroed.
///
/// # Safety
///
/// As [`vm_region_64`].
unsafe fn extended_info(
    address: *mut u64,
    size: *mut u64,
    info: *mut c_void,
    count: *mut u32,
    object_name: *mut Name,
) -> i32 {
    const WORDS: usize = size_of::<ExtendedInfo>() / size_of::<u32>();
    // SAFETY: the caller states `count` is readable.
    let capacity = unsafe { *count } as usize;
    if capacity < WORDS {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller states `address` is readable.
    let Some(region) = region_at_or_after(unsafe { *address }) else {
        return KERN_INVALID_ADDRESS;
    };
    let extended = ExtendedInfo {
        protection: protection(&region),
        ref_count: 1,
        share_mode: match region.shared {
            true => SM_SHARED,
            false => SM_PRIVATE,
        },
        ..ExtendedInfo::default()
    };
    let words = capacity.min(WORDS + 1);
    // SAFETY: the caller's obligations, with the capacity checked above.
    unsafe {
        *address = region.start;
        *size = region.end - region.start;
        if words > WORDS {
            info.cast::<u32>().add(WORDS).write_unaligned(0);
        }
        info.cast::<ExtendedInfo>().write_unaligned(extended);
        *count = words as u32;
        if !object_name.is_null() {
            *object_name = MACH_PORT_NULL;
        }
    }
    KERN_SUCCESS
}

/// `kern_return_t vm_region_recurse_64(vm_map_t target_task, vm_address_t
/// *address, vm_size_t *size, natural_t *nesting_depth,
/// vm_region_recurse_info_t info, mach_msg_type_number_t *infoCnt)`
///
/// Answers `struct vm_region_submap_info_64`, packed to four bytes, as far as
/// the caller's count reaches: the first sixteen words are every version's,
/// the seventeenth (`pages_reusable`) the newer ones'. There are no submaps
/// here, so every region is at depth zero and none is one. Page counts are
/// not tracked and read zero.
///
/// # Safety
///
/// `address`, `size`, `depth` and `count` are writable, and `info` is writable
/// for `*count` 32-bit words.
pub unsafe extern "C" fn vm_region_recurse_64(
    task: Name,
    address: *mut u64,
    size: *mut u64,
    depth: *mut u32,
    info: *mut u32,
    count: *mut u32,
) -> i32 {
    /// `VM_REGION_SUBMAP_INFO_V0_COUNT_64` and `_V1_`.
    const V0_WORDS: usize = 16;
    const V1_WORDS: usize = 17;
    if address.is_null() || size.is_null() || info.is_null() || count.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    if task != TASK_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller states `count` is readable.
    let capacity = unsafe { *count } as usize;
    if capacity < V0_WORDS {
        return KERN_INVALID_ARGUMENT;
    }
    // SAFETY: the caller states `address` is readable.
    let Some(region) = region_at_or_after(unsafe { *address }) else {
        return KERN_INVALID_ADDRESS;
    };
    let words = capacity.min(V1_WORDS);
    let mut out = vec![0u32; words];
    out[0] = protection(&region) as u32;
    out[1] = protection(&region) as u32;
    out[2] = match region.shared {
        true => VM_INHERIT_SHARE,
        false => VM_INHERIT_COPY,
    };
    // `offset`, a 64-bit field at byte 12.
    out[3] = region.offset as u32;
    out[4] = (region.offset >> 32) as u32;
    // user_tag, pages_resident, _shared_now_private, _swapped_out, _dirtied.
    // ref_count, at word 10.
    out[10] = 1;
    // shadow_depth (u16), external_pager (u8), share_mode (u8) at word 11:
    // `SM_PRIVATE` is 1 and `SM_SHARED` 3.
    let share_mode: u32 = if region.shared { 3 } else { 1 };
    out[11] = share_mode << 24;
    // is_submap (12), behavior (13), object_id (14), user_wired_count (15).
    out[13] = VM_BEHAVIOR_DEFAULT as u32;
    // SAFETY: the caller's obligations, with `words` within its capacity.
    unsafe {
        *address = region.start;
        *size = region.end - region.start;
        if !depth.is_null() {
            *depth = 0;
        }
        std::ptr::copy_nonoverlapping(out.as_ptr(), info, words);
        *count = words as u32;
    }
    KERN_SUCCESS
}

/// `VM_FLAGS_*` that `vm_allocate` answers. `FIXED` is zero: the absence of
/// `ANYWHERE`.
const VM_FLAGS_ANYWHERE: i32 = 0x0001;
const VM_FLAGS_RANDOM_ADDR: i32 = 0x0008;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;
/// `VM_FLAGS_ALIAS_MASK`: the `VM_MAKE_TAG` label, which only feeds `vmmap`'s
/// statistics and has nothing to attach to on this host.
const VM_FLAGS_ALIAS_MASK: i32 = 0xff00_0000u32 as i32;

/// `kern_return_t vm_allocate(vm_map_t target_task, vm_address_t *address,
/// vm_size_t size, int flags)`, and `mach_vm_allocate`, which is the same call
/// on a 64-bit task.
///
/// Zero-filled anonymous memory, as Mach's is. Crashpad allocates its message
/// buffers this way; the host's own would be asked about a task name this
/// runtime made up.
///
/// # Safety
///
/// `address` is readable and writable.
pub unsafe extern "C" fn vm_allocate(task: Name, address: *mut u64, size: u64, flags: i32) -> i32 {
    if task != TASK_SELF || address.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    // Purgeable, superpage, 4GB-chunk and the rest change what the memory is,
    // and pretending to honour them would hand back something else.
    if flags
        & !(VM_FLAGS_ANYWHERE | VM_FLAGS_RANDOM_ADDR | VM_FLAGS_OVERWRITE | VM_FLAGS_ALIAS_MASK)
        != 0
    {
        eprintln!("rosemary: vm_allocate: unsupported flags {flags:#x}");
        return KERN_INVALID_ARGUMENT;
    }
    if size == 0 {
        // SAFETY: the caller states `address` is writable.
        unsafe { *address = 0 };
        return KERN_SUCCESS;
    }
    let page = super::info::page_size();
    let Some(length) = size.checked_next_multiple_of(page) else {
        return KERN_INVALID_ARGUMENT;
    };
    let anywhere = flags & VM_FLAGS_ANYWHERE != 0;
    // SAFETY: the caller states `address` is readable.
    let hint = match anywhere {
        true => 0,
        false => (unsafe { *address }) & !(page - 1),
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
        return KERN_NO_SPACE;
    }
    // A fixed request without `OVERWRITE` must not land anywhere else, and a
    // hint is all `mmap` will take without displacing a neighbour.
    if !anywhere && mapped as u64 != hint {
        // SAFETY: the mapping made above, which nothing else has seen.
        unsafe { libc::munmap(mapped, length as usize) };
        return KERN_NO_SPACE;
    }
    // SAFETY: the caller states `address` is writable.
    unsafe { *address = mapped as u64 };
    KERN_SUCCESS
}

/// `kern_return_t vm_deallocate(vm_map_t target_task, vm_address_t address,
/// vm_size_t size)`
///
/// # Safety
///
/// `address` and `size` describe a mapping this process made and is done with.
pub unsafe extern "C" fn vm_deallocate(task: Name, address: u64, size: u64) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    // Mach accepts a zero-length deallocation and does nothing; `munmap` calls
    // it an error.
    if size == 0 {
        return KERN_SUCCESS;
    }
    // SAFETY: the caller states this is its own mapping.
    let out = unsafe { libc::munmap(address as *mut c_void, size as usize) };
    match out {
        0 => {
            super::memory::forget(address, size);
            KERN_SUCCESS
        }
        _ => KERN_INVALID_ADDRESS,
    }
}

/// `kern_return_t vm_read_overwrite(vm_map_t target_task, vm_address_t address,
/// vm_size_t size, vm_address_t data, vm_size_t *outsize)`
///
/// Within one address space this is a copy, which is what it is on Darwin too
/// when the task is the caller's own.
///
/// # Safety
///
/// `address` is readable for `size` bytes and `data` writable for `size`; they
/// do not overlap. `out_size` is null or writable.
pub unsafe extern "C" fn vm_read_overwrite(
    task: Name,
    address: u64,
    size: u64,
    data: u64,
    out_size: *mut u64,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_ARGUMENT;
    }
    if address == 0 || data == 0 {
        return KERN_INVALID_ADDRESS;
    }
    // SAFETY: the caller's obligation, including that the two do not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            address as *const u8,
            data as *mut u8,
            usize::try_from(size).unwrap_or(0),
        );
        if !out_size.is_null() {
            *out_size = size;
        }
    }
    KERN_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The call names the region a known address is in, which is the whole of
    /// what a memory walk reads out of it.
    #[test]
    fn names_the_region_an_address_is_in() {
        let page = vec![0_u8; 4096];
        let mut address = page.as_ptr() as u64;
        let mut size = 0_u64;
        let mut info = [0_u32; 16];
        let mut count = (size_of::<BasicInfo64>() / size_of::<u32>()) as u32;
        let mut object = 1_u32;
        // SAFETY: writable out-parameters, and a buffer whose capacity is what
        // `count` says.
        let status = unsafe {
            vm_region_64(
                TASK_SELF,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO_64,
                info.as_mut_ptr().cast(),
                &mut count,
                &mut object,
            )
        };
        assert_eq!(status, KERN_SUCCESS);
        assert!(address <= page.as_ptr() as u64);
        assert!(address + size > page.as_ptr() as u64);
        assert_eq!(object, MACH_PORT_NULL);
        // SAFETY: the call above filled it.
        let basic = unsafe { info.as_ptr().cast::<BasicInfo64>().read_unaligned() };
        assert_eq!(basic.protection & (VM_PROT_READ | VM_PROT_WRITE), 3);
    }

    /// Past the last mapping the walk is told to stop, rather than being handed
    /// a region that is not there.
    #[test]
    fn the_end_of_the_map_is_an_invalid_address() {
        let mut address = u64::MAX - 4096;
        let mut size = 0_u64;
        let mut info = [0_u32; 16];
        let mut count = (size_of::<BasicInfo64>() / size_of::<u32>()) as u32;
        // SAFETY: writable out-parameters; `object_name` is allowed to be null.
        let status = unsafe {
            vm_region_64(
                TASK_SELF,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO_64,
                info.as_mut_ptr().cast(),
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, KERN_INVALID_ADDRESS);
    }

    /// Another task is refused rather than answered about with this one's map.
    #[test]
    fn another_task_is_refused() {
        let mut address = 0_u64;
        let mut size = 0_u64;
        let mut info = [0_u32; 16];
        let mut count = 16_u32;
        // SAFETY: writable out-parameters.
        let status = unsafe {
            vm_region_64(
                TASK_SELF + 4,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO_64,
                info.as_mut_ptr().cast(),
                &mut count,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, KERN_INVALID_ARGUMENT);
    }

    /// A read of this task's own memory is a copy.
    #[test]
    fn reads_this_tasks_own_memory() {
        let from = [1_u8, 2, 3, 4];
        let mut into = [0_u8; 4];
        let mut wrote = 0_u64;
        // SAFETY: two live, non-overlapping buffers of the stated length.
        let status = unsafe {
            vm_read_overwrite(
                TASK_SELF,
                from.as_ptr() as u64,
                4,
                into.as_mut_ptr() as u64,
                &mut wrote,
            )
        };
        assert_eq!(status, KERN_SUCCESS);
        assert_eq!(into, from);
        assert_eq!(wrote, 4);
    }

    /// The buffer Crashpad's message server receives into.
    #[test]
    fn allocates_zeroed_memory_anywhere_and_gives_it_back() {
        let mut address = 0_u64;
        // `VM_FLAGS_ANYWHERE | VM_MAKE_TAG(VM_MEMORY_MACH_MSG)`.
        // SAFETY: a writable out-parameter.
        let status = unsafe { vm_allocate(TASK_SELF, &mut address, 100, 0x1400_0001) };
        assert_eq!(status, KERN_SUCCESS);
        assert_ne!(address, 0);
        assert_eq!(address % super::super::info::page_size(), 0);
        // SAFETY: the 100 bytes just allocated.
        let bytes = unsafe { std::slice::from_raw_parts_mut(address as *mut u8, 100) };
        assert!(bytes.iter().all(|byte| *byte == 0));
        bytes[99] = 1;
        // SAFETY: the allocation above.
        assert_eq!(
            unsafe { vm_deallocate(TASK_SELF, address, 100) },
            KERN_SUCCESS
        );
    }

    /// A fixed request over memory already in use is refused, not moved.
    #[test]
    fn a_fixed_allocation_over_a_mapping_is_no_space() {
        let mut taken = 0_u64;
        // SAFETY: a writable out-parameter.
        assert_eq!(
            unsafe { vm_allocate(TASK_SELF, &mut taken, 4096, VM_FLAGS_ANYWHERE) },
            KERN_SUCCESS
        );
        let mut address = taken;
        // SAFETY: a writable out-parameter.
        let status = unsafe { vm_allocate(TASK_SELF, &mut address, 4096, 0) };
        assert_eq!(status, KERN_NO_SPACE);
        assert_eq!(address, taken);
        // SAFETY: the allocation above.
        unsafe { vm_deallocate(TASK_SELF, taken, 4096) };
    }
}
