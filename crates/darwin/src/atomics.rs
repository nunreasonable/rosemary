use std::ffi::c_void;
use std::hint::spin_loop;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicPtr, Ordering, fence};

#[inline]
unsafe fn atomic_i32<'a>(ptr: *mut c_void) -> &'a AtomicI32 {
    // Darwin requires these values to be naturally aligned.
    //
    // SAFETY:
    // - caller guarantees `ptr` points to a valid writable int32_t
    // - OSAtomic requires natural alignment
    // - accesses to this storage must obey atomic-access rules
    unsafe { AtomicI32::from_ptr(ptr.cast::<i32>()) }
}

#[inline]
unsafe fn atomic_i64<'a>(ptr: *mut c_void) -> &'a AtomicI64 {
    // SAFETY: same requirements as above, for int64_t.
    unsafe { AtomicI64::from_ptr(ptr.cast::<i64>()) }
}

#[inline]
unsafe fn atomic_ptr<'a>(ptr: *mut c_void) -> &'a AtomicPtr<c_void> {
    // `ptr` is the address of the guest's pointer-sized storage, i.e.
    // void * volatile * from the Darwin API's perspective.
    //
    // SAFETY:
    // - caller guarantees suitably aligned, writable pointer storage
    // - all conflicting accesses obey atomic-access rules
    unsafe { AtomicPtr::from_ptr(ptr.cast::<*mut c_void>()) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicAdd32(amount: i32, value: *mut c_void) -> i32 {
    // fetch_add returns the old value; Darwin returns the new value.
    unsafe { atomic_i32(value) }
        .fetch_add(amount, Ordering::Relaxed)
        .wrapping_add(amount)
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicAdd32Barrier(amount: i32, value: *mut c_void) -> i32 {
    unsafe { atomic_i32(value) }
        .fetch_add(amount, Ordering::SeqCst)
        .wrapping_add(amount)
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicIncrement32(value: *mut c_void) -> i32 {
    unsafe { OSAtomicAdd32(1, value) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicIncrement32Barrier(value: *mut c_void) -> i32 {
    unsafe { OSAtomicAdd32Barrier(1, value) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicDecrement32(value: *mut c_void) -> i32 {
    unsafe { OSAtomicAdd32(-1, value) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicDecrement32Barrier(value: *mut c_void) -> i32 {
    unsafe { OSAtomicAdd32Barrier(-1, value) }
}

/// `int32_t OSAtomicAnd32Barrier(uint32_t mask, volatile uint32_t *)`: the new value.
#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicAnd32Barrier(mask: u32, value: *mut c_void) -> i32 {
    unsafe { atomic_i32(value) }.fetch_and(mask as i32, Ordering::SeqCst) & mask as i32
}

/// `int32_t OSAtomicOr32Barrier(uint32_t mask, volatile uint32_t *)`: the new value.
#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicOr32Barrier(mask: u32, value: *mut c_void) -> i32 {
    unsafe { atomic_i32(value) }.fetch_or(mask as i32, Ordering::SeqCst) | mask as i32
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicAdd64(amount: i64, value: *mut c_void) -> i64 {
    unsafe { atomic_i64(value) }
        .fetch_add(amount, Ordering::Relaxed)
        .wrapping_add(amount)
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicAdd64Barrier(amount: i64, value: *mut c_void) -> i64 {
    unsafe { atomic_i64(value) }
        .fetch_add(amount, Ordering::SeqCst)
        .wrapping_add(amount)
}

#[inline]
unsafe fn compare_and_swap32(old: i32, new: i32, value: *mut c_void, ordering: Ordering) -> bool {
    let failure = match ordering {
        Ordering::SeqCst => Ordering::SeqCst,
        _ => Ordering::Relaxed,
    };

    unsafe { atomic_i32(value) }
        .compare_exchange(old, new, ordering, failure)
        .is_ok()
}

#[inline]
unsafe fn compare_and_swap64(old: i64, new: i64, value: *mut c_void, ordering: Ordering) -> bool {
    let failure = match ordering {
        Ordering::SeqCst => Ordering::SeqCst,
        _ => Ordering::Relaxed,
    };

    unsafe { atomic_i64(value) }
        .compare_exchange(old, new, ordering, failure)
        .is_ok()
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwap32(old: i32, new: i32, value: *mut c_void) -> bool {
    unsafe { compare_and_swap32(old, new, value, Ordering::Relaxed) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwap32Barrier(
    old: i32,
    new: i32,
    value: *mut c_void,
) -> bool {
    unsafe { compare_and_swap32(old, new, value, Ordering::SeqCst) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwap64(old: i64, new: i64, value: *mut c_void) -> bool {
    unsafe { compare_and_swap64(old, new, value, Ordering::Relaxed) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwap64Barrier(
    old: i64,
    new: i64,
    value: *mut c_void,
) -> bool {
    unsafe { compare_and_swap64(old, new, value, Ordering::SeqCst) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwapInt(old: i32, new: i32, value: *mut c_void) -> bool {
    unsafe { OSAtomicCompareAndSwap32(old, new, value) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwapIntBarrier(
    old: i32,
    new: i32,
    value: *mut c_void,
) -> bool {
    unsafe { OSAtomicCompareAndSwap32Barrier(old, new, value) }
}

#[inline]
unsafe fn compare_and_swap_ptr(
    old: *mut c_void,
    new: *mut c_void,
    value: *mut c_void,
    ordering: Ordering,
) -> bool {
    let failure = match ordering {
        Ordering::SeqCst => Ordering::SeqCst,
        _ => Ordering::Relaxed,
    };

    unsafe { atomic_ptr(value) }
        .compare_exchange(old, new, ordering, failure)
        .is_ok()
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwapPtr(
    old: *mut c_void,
    new: *mut c_void,
    value: *mut c_void,
) -> bool {
    unsafe { compare_and_swap_ptr(old, new, value, Ordering::Relaxed) }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSAtomicCompareAndSwapPtrBarrier(
    old: *mut c_void,
    new: *mut c_void,
    value: *mut c_void,
) -> bool {
    unsafe { compare_and_swap_ptr(old, new, value, Ordering::SeqCst) }
}

#[allow(non_snake_case)]
pub extern "C" fn OSMemoryBarrier() {
    fence(Ordering::SeqCst);
}

/// OSSpinLock
///
/// Darwin defines OSSpinLock as an int32_t.
///
/// 0       => unlocked
/// nonzero => locked
///
/// Apple explicitly documents only the zero/nonzero distinction, so using 1 as
/// the locked representation is sufficient.
#[allow(non_snake_case)]
pub unsafe extern "C" fn OSSpinLockTry(lock: *mut c_void) -> bool {
    unsafe { atomic_i32(lock) }
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSSpinLockLock(lock: *mut c_void) {
    let lock = unsafe { atomic_i32(lock) };

    // Do a normal CAS first. After contention is observed, spin mostly on
    // relaxed loads so we don't continuously issue expensive cache-line
    // ownership requests.
    loop {
        if lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }

        while lock.load(Ordering::Relaxed) != 0 {
            spin_loop();
        }
    }
}

#[allow(non_snake_case)]
pub unsafe extern "C" fn OSSpinLockUnlock(lock: *mut c_void) {
    unsafe { atomic_i32(lock) }.store(0, Ordering::Release);
}

exports::exports! {
    functions {
        "OSAtomicAdd32" => OSAtomicAdd32,
        "OSAtomicAdd32Barrier" => OSAtomicAdd32Barrier,
        "OSAtomicIncrement32" => OSAtomicIncrement32,
        "OSAtomicIncrement32Barrier" => OSAtomicIncrement32Barrier,
        "OSAtomicDecrement32" => OSAtomicDecrement32,
        "OSAtomicDecrement32Barrier" => OSAtomicDecrement32Barrier,
        "OSAtomicAnd32Barrier" => OSAtomicAnd32Barrier,
        "OSAtomicOr32Barrier" => OSAtomicOr32Barrier,
        "OSAtomicAdd64" => OSAtomicAdd64,
        "OSAtomicAdd64Barrier" => OSAtomicAdd64Barrier,
        "OSAtomicCompareAndSwap32" => OSAtomicCompareAndSwap32,
        "OSAtomicCompareAndSwap32Barrier" => OSAtomicCompareAndSwap32Barrier,
        "OSAtomicCompareAndSwap64" => OSAtomicCompareAndSwap64,
        "OSAtomicCompareAndSwap64Barrier" => OSAtomicCompareAndSwap64Barrier,
        "OSAtomicCompareAndSwapInt" => OSAtomicCompareAndSwapInt,
        "OSAtomicCompareAndSwapIntBarrier" => OSAtomicCompareAndSwapIntBarrier,
        "OSAtomicCompareAndSwapPtr" => OSAtomicCompareAndSwapPtr,
        "OSAtomicCompareAndSwapPtrBarrier" => OSAtomicCompareAndSwapPtrBarrier,
        "OSMemoryBarrier" => OSMemoryBarrier,
        "OSSpinLockLock" => OSSpinLockLock,
        "OSSpinLockTry" => OSSpinLockTry,
        "OSSpinLockUnlock" => OSSpinLockUnlock,
    }
}
