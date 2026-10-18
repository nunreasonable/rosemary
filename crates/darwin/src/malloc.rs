#![cfg(not(target_os = "macos"))]

use std::ffi::c_void;

/// Darwin's `malloc_statistics_t`. Four fields, and the layout is ABI: a guest
/// reads `size_in_use` out of a structure it allocated on its own stack.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Statistics {
    pub blocks_in_use: u32,
    _pad: u32,
    pub size_in_use: usize,
    pub max_size_in_use: usize,
    pub size_allocated: usize,
}

const _: () = {
    assert!(size_of::<Statistics>() == 32);
    assert!(std::mem::offset_of!(Statistics, size_in_use) == 8);
};

/// `void malloc_zone_statistics(malloc_zone_t *zone, malloc_statistics_t *stats)`
///
/// # Safety
///
/// `stats` is null or a writable Darwin `malloc_statistics_t`.
pub unsafe extern "C" fn malloc_zone_statistics(_zone: *mut c_void, stats: *mut Statistics) {
    if stats.is_null() {
        return;
    }
    #[cfg(target_env = "gnu")]
    {
        let info = unsafe { libc::mallinfo2() };
        // SAFETY: the caller states `stats` is writable.
        unsafe {
            *stats = Statistics {
                // glibc counts no blocks, and a count invented from the byte total
                // would be a number with no meaning. Zero says "not reported",
                // which is what a caller that divides by it needs to see.
                blocks_in_use: 0,
                _pad: 0,
                // `uordblks` counts only what came out of the heap. A block past
                // `M_MMAP_THRESHOLD` - which is most of a game's memory - is mapped
                // instead and counted in `hblkhd`, so reporting the first alone says
                // a program using two hundred megabytes is using sixteen kilobytes.
                size_in_use: info.uordblks + info.hblkhd,
                // The high-water mark is not kept either. Reporting the current
                // total would say the program has never freed anything.
                max_size_in_use: 0,
                size_allocated: info.arena + info.hblkhd,
            };
        }
    }
    #[cfg(not(target_env = "gnu"))]
    {
        unsafe {
            *stats = Statistics {
                blocks_in_use: 0,
                _pad: 0,
                size_in_use: 0,
                max_size_in_use: 0,
                size_allocated: 0,
            };
        }
    }
}

/// `size_t malloc_size(const void *ptr)`
///
/// Darwin answers the usable size of a block, and zero for a pointer its
/// allocator does not own. glibc and musl answer the first question as
/// `malloc_usable_size` and do not answer the second at all, so a pointer that
/// is not a live block is undefined here where Darwin would have said zero. A
/// guest asks this about its own blocks, which one allocator owns - see
/// [`crate::heap`].
///
/// # Safety
///
/// `ptr` is null or a live block from this host's allocator.
pub unsafe extern "C" fn malloc_size(ptr: *const c_void) -> usize {
    if ptr.is_null() {
        return 0;
    }
    // SAFETY: the caller states `ptr` is a live block of this allocator.
    unsafe { libc::malloc_usable_size(ptr.cast_mut()) }
}

/// `malloc_zone_t`, laid out as Darwin's (version 13). Every zone here is the
/// one heap the guest's `malloc` already uses, but a program - or an allocator
/// it carries - calls through these pointers directly (`zone->size(zone, p)`),
/// so they have to be there and do the right thing.
#[repr(C)]
pub struct Zone {
    reserved: [usize; 2],
    size: unsafe extern "C" fn(*const Zone, *const c_void) -> usize,
    malloc: unsafe extern "C" fn(*const Zone, usize) -> *mut c_void,
    calloc: unsafe extern "C" fn(*const Zone, usize, usize) -> *mut c_void,
    valloc: unsafe extern "C" fn(*const Zone, usize) -> *mut c_void,
    free: unsafe extern "C" fn(*const Zone, *mut c_void),
    realloc: unsafe extern "C" fn(*const Zone, *mut c_void, usize) -> *mut c_void,
    destroy: unsafe extern "C" fn(*const Zone),
    zone_name: *const libc::c_char,
    batch_malloc: unsafe extern "C" fn(*const Zone, usize, *mut *mut c_void, u32) -> u32,
    batch_free: unsafe extern "C" fn(*const Zone, *mut *mut c_void, u32),
    introspect: *const Introspection,
    version: u32,
    memalign: unsafe extern "C" fn(*const Zone, usize, usize) -> *mut c_void,
    free_definite_size: unsafe extern "C" fn(*const Zone, *mut c_void, usize),
    pressure_relief: unsafe extern "C" fn(*const Zone, usize) -> usize,
    claimed_address: unsafe extern "C" fn(*const Zone, *const c_void) -> u32,
    try_free_default: unsafe extern "C" fn(*const Zone, *mut c_void),
}

const _: () = {
    assert!(std::mem::offset_of!(Zone, size) == 16);
    assert!(std::mem::offset_of!(Zone, free) == 48);
    assert!(std::mem::offset_of!(Zone, zone_name) == 72);
    assert!(std::mem::offset_of!(Zone, introspect) == 96);
    assert!(std::mem::offset_of!(Zone, memalign) == 112);
    assert!(std::mem::offset_of!(Zone, try_free_default) == 144);
};

/// `malloc_introspection_t`: the debugging half of a zone, which a program
/// reaches for `good_size` and `check`, and the rest only under a debugger.
#[repr(C)]
pub struct Introspection {
    enumerator: unsafe extern "C" fn() -> usize,
    good_size: unsafe extern "C" fn(*const Zone, usize) -> usize,
    check: unsafe extern "C" fn(*const Zone) -> u32,
    rest: [unsafe extern "C" fn() -> usize; 13],
}

// SAFETY: written once, when made, and only read after.
unsafe impl Sync for Zone {}
unsafe impl Send for Zone {}

unsafe extern "C" fn nothing() -> usize {
    0
}
unsafe extern "C" fn good_size(_zone: *const Zone, size: usize) -> usize {
    size
}
unsafe extern "C" fn check(_zone: *const Zone) -> u32 {
    1
}

static INTROSPECTION: Introspection = Introspection {
    enumerator: nothing,
    good_size,
    check,
    rest: [nothing; 13],
};

unsafe extern "C" fn zone_size(_zone: *const Zone, ptr: *const c_void) -> usize {
    // SAFETY: Darwin answers zero for a pointer the zone does not own; every
    // block a guest has came from this one allocator.
    unsafe { malloc_size(ptr) }
}
unsafe extern "C" fn zone_malloc(_zone: *const Zone, size: usize) -> *mut c_void {
    unsafe { libc::malloc(size) }
}
unsafe extern "C" fn zone_calloc(_zone: *const Zone, count: usize, size: usize) -> *mut c_void {
    unsafe { libc::calloc(count, size) }
}
unsafe extern "C" fn zone_valloc(zone: *const Zone, size: usize) -> *mut c_void {
    // A page-aligned block, which is all `valloc` promises.
    unsafe { zone_memalign(zone, 4096, size) }
}
unsafe extern "C" fn zone_free(_zone: *const Zone, ptr: *mut c_void) {
    unsafe { libc::free(ptr) }
}
unsafe extern "C" fn zone_realloc(_zone: *const Zone, ptr: *mut c_void, size: usize) -> *mut c_void {
    unsafe { libc::realloc(ptr, size) }
}
unsafe extern "C" fn zone_destroy(_zone: *const Zone) {}
unsafe extern "C" fn zone_batch_malloc(_zone: *const Zone, size: usize, out: *mut *mut c_void, count: u32) -> u32 {
    let mut made = 0;
    while made < count {
        let block = unsafe { libc::malloc(size) };
        if block.is_null() {
            break;
        }
        // SAFETY: the caller's array of `count` slots.
        unsafe { *out.add(made as usize) = block };
        made += 1;
    }
    made
}
unsafe extern "C" fn zone_batch_free(_zone: *const Zone, blocks: *mut *mut c_void, count: u32) {
    for i in 0..count as usize {
        // SAFETY: the caller's array of `count` blocks.
        unsafe { libc::free(*blocks.add(i)) };
    }
}
unsafe extern "C" fn zone_memalign(_zone: *const Zone, alignment: usize, size: usize) -> *mut c_void {
    let mut out = std::ptr::null_mut();
    match unsafe { libc::posix_memalign(&mut out, alignment.max(size_of::<usize>()), size) } {
        0 => out,
        _ => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn zone_free_definite_size(_zone: *const Zone, ptr: *mut c_void, _size: usize) {
    unsafe { libc::free(ptr) }
}
unsafe extern "C" fn zone_pressure_relief(_zone: *const Zone, _goal: usize) -> usize {
    0
}
unsafe extern "C" fn zone_claimed_address(_zone: *const Zone, ptr: *const c_void) -> u32 {
    u32::from(!ptr.is_null())
}

const fn zone(name: *const libc::c_char) -> Zone {
    Zone {
        reserved: [0; 2],
        size: zone_size,
        malloc: zone_malloc,
        calloc: zone_calloc,
        valloc: zone_valloc,
        free: zone_free,
        realloc: zone_realloc,
        destroy: zone_destroy,
        zone_name: name,
        batch_malloc: zone_batch_malloc,
        batch_free: zone_batch_free,
        introspect: &INTROSPECTION,
        version: 13,
        memalign: zone_memalign,
        free_definite_size: zone_free_definite_size,
        pressure_relief: zone_pressure_relief,
        claimed_address: zone_claimed_address,
        try_free_default: zone_free,
    }
}

static DEFAULT_ZONE: Zone = zone(c"DefaultMallocZone".as_ptr());

/// `malloc_zone_t *malloc_default_zone(void)`
pub unsafe extern "C" fn malloc_default_zone() -> *mut c_void {
    &DEFAULT_ZONE as *const Zone as *mut c_void
}

/// `malloc_zone_t *malloc_create_zone(vm_size_t start_size, unsigned flags)`
///
/// A zone of its own, so that its name can be set, over the same heap.
///
/// FIXME (ACCURACY): destroying it does not free what was allocated in it,
/// which only a program that leans on `malloc_destroy_zone` to release memory
/// would notice. The zone itself is leaked for the same reason Darwin keeps
/// it: a pointer to it may outlive the call that destroys it.
pub unsafe extern "C" fn malloc_create_zone(_start: usize, _flags: u32) -> *mut c_void {
    Box::into_raw(Box::new(zone(std::ptr::null()))).cast()
}

/// `void malloc_set_zone_name(malloc_zone_t *zone, const char *name)`
///
/// The name is copied and kept, as Darwin's is. The default zone's is fixed.
pub unsafe extern "C" fn malloc_set_zone_name(zone: *mut c_void, name: *const libc::c_char) {
    if zone.is_null() || zone == unsafe { malloc_default_zone() } {
        return;
    }
    let copy = match name.is_null() {
        true => std::ptr::null(),
        // SAFETY: a NUL-terminated string, per the caller; the copy is leaked
        // with the zone.
        false => unsafe { libc::strdup(name) }.cast_const(),
    };
    // SAFETY: a zone `malloc_create_zone` made, which is writable.
    unsafe { (*(zone as *mut Zone)).zone_name = copy };
}

/// `void malloc_destroy_zone(malloc_zone_t *zone)`
pub unsafe extern "C" fn malloc_destroy_zone(_zone: *mut c_void) {}

/// `void *malloc_zone_malloc(malloc_zone_t *zone, size_t size)`
pub unsafe extern "C" fn malloc_zone_malloc(_zone: *mut c_void, size: usize) -> *mut c_void {
    unsafe { libc::malloc(size) }
}

/// `void *malloc_zone_calloc(malloc_zone_t *zone, size_t count, size_t size)`
pub unsafe extern "C" fn malloc_zone_calloc(_zone: *mut c_void, count: usize, size: usize) -> *mut c_void {
    unsafe { libc::calloc(count, size) }
}

/// `void *malloc_zone_realloc(malloc_zone_t *zone, void *ptr, size_t size)`
pub unsafe extern "C" fn malloc_zone_realloc(_zone: *mut c_void, ptr: *mut c_void, size: usize) -> *mut c_void {
    unsafe { libc::realloc(ptr, size) }
}

/// `void malloc_zone_free(malloc_zone_t *zone, void *ptr)`
pub unsafe extern "C" fn malloc_zone_free(_zone: *mut c_void, ptr: *mut c_void) {
    unsafe { libc::free(ptr) }
}

/// `malloc_zone_t *malloc_zone_from_ptr(const void *ptr)`
pub unsafe extern "C" fn malloc_zone_from_ptr(_ptr: *const c_void) -> *mut c_void {
    unsafe { malloc_default_zone() }
}

exports::exports! {
    functions {
        "malloc_default_zone" => malloc_default_zone,
        "malloc_create_zone" => malloc_create_zone,
        "malloc_set_zone_name" => malloc_set_zone_name,
        "malloc_destroy_zone" => malloc_destroy_zone,
        "malloc_zone_malloc" => malloc_zone_malloc,
        "malloc_zone_calloc" => malloc_zone_calloc,
        "malloc_zone_realloc" => malloc_zone_realloc,
        "malloc_zone_free" => malloc_zone_free,
        "malloc_zone_from_ptr" => malloc_zone_from_ptr,
        "malloc_size" => malloc_size,
        "malloc_zone_statistics" => malloc_zone_statistics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block's reported size covers what was asked for, and a null pointer
    /// owns nothing.
    #[test]
    fn reports_the_size_of_a_block() {
        // SAFETY: a block from this host's allocator, freed below.
        let block = unsafe { libc::malloc(4096) };
        assert!(!block.is_null());
        // SAFETY: `block` is live.
        assert!(unsafe { malloc_size(block) } >= 4096);
        // SAFETY: `block` came from `malloc` and is not used again.
        unsafe { libc::free(block) };
        // SAFETY: null is the documented no-block case.
        assert_eq!(unsafe { malloc_size(std::ptr::null()) }, 0);
    }

    /// The total is a real number and the fields nothing can answer are zero,
    /// which is the difference between "not reported" and "nothing allocated".
    #[test]
    fn the_total_in_use_is_reported() {
        // Two allocations, because glibc counts them in different fields: a
        // small one comes out of the heap and a large one is mapped, and a
        // report that missed either would still look plausible.
        // Through `black_box`, because in a release build an allocation whose
        // only use is its own length is one the optimiser deletes - and the test
        // then measures a program holding nothing.
        let small = std::hint::black_box(vec![0u8; 8 << 10]);
        let large = std::hint::black_box(vec![0u8; 4 << 20]);
        let mut stats = Statistics::default();
        // SAFETY: a writable structure of the declared layout.
        unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
        let held = small.len() + large.len();
        assert!(
            stats.size_in_use >= held,
            "{} bytes in use with {held} held",
            stats.size_in_use,
        );
        assert!(stats.size_allocated >= stats.size_in_use);
        assert_eq!(stats.max_size_in_use, 0, "a high-water mark nothing keeps");
        drop(std::hint::black_box((small, large)));
    }
}
