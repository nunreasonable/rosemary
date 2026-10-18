use core::ffi::{c_int, c_void};

type Compare = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;

/// `void qsort(void *base, size_t nel, size_t width, int (*compar)(const void *, const void *))`
///
/// # Safety
///
/// `base` is `nel` elements of `width` bytes and `compar` is callable, which is
/// `qsort`'s own contract.
pub unsafe extern "C" fn qsort(
    base: *mut c_void,
    nel: usize,
    width: usize,
    compar: Option<Compare>,
) {
    // SAFETY: the caller's array and comparator, sorted by the host's own
    // `qsort` - which calls the comparator, so the boundary covers the sort.
    unsafe { crate::call_as_darwin!(libc::qsort(base, nel, width, compar)) }
}

/// `void *bsearch(const void *key, const void *base, size_t nel, size_t width,
/// int (*compar)(const void *, const void *))`
///
/// # Safety
///
/// Darwin's comparator for `qsort_r`, which takes the caller's cookie first.
type ThunkCompare = unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void) -> c_int;

struct QsortR {
    thunk: *mut c_void,
    compar: ThunkCompare,
}

/// glibc's `qsort_r` comparator, which takes the cookie last, calling Darwin's.
unsafe extern "C" fn qsort_r_compare(
    a: *const c_void,
    b: *const c_void,
    arg: *mut c_void,
) -> c_int {
    // SAFETY: `arg` is the `QsortR` on `qsort_r`'s stack, live for the sort.
    let call = unsafe { &*(arg as *const QsortR) };
    unsafe { (call.compar)(call.thunk, a, b) }
}

/// `void qsort_r(void *base, size_t nel, size_t width, void *thunk,
/// int (*compar)(void *, const void *, const void *))`: the BSD order, cookie
/// before comparator and passed first to it, where glibc's puts both last.
///
/// # Safety
///
/// `qsort`'s own contract, with `compar` taking `thunk` first.
pub unsafe extern "C" fn qsort_r(
    base: *mut c_void,
    nel: usize,
    width: usize,
    thunk: *mut c_void,
    compar: Option<ThunkCompare>,
) {
    let Some(compar) = compar else {
        return;
    };
    let mut call = QsortR { thunk, compar };
    unsafe {
        crate::call_as_darwin!(libc::qsort_r(
            base,
            nel,
            width,
            Some(qsort_r_compare),
            (&mut call as *mut QsortR).cast()
        ))
    }
}

/// As [`qsort`], with `key` readable.
pub unsafe extern "C" fn bsearch(
    key: *const c_void,
    base: *const c_void,
    nel: usize,
    width: usize,
    compar: Option<Compare>,
) -> *mut c_void {
    unsafe { crate::call_as_darwin!(libc::bsearch(key, base, nel, width, compar)) }
}

exports::exports! {
    functions {
        "qsort" => qsort,
        "qsort_r" => qsort_r,
        "bsearch" => bsearch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the wrappers exist for: the comparator is entered with the Darwin
    /// thread pointer even though the thread that called `qsort` had the host's.
    ///
    /// Run on a thread of this test's own, because the boundary is free - and so
    /// proves nothing - on a thread that is inside Darwin code already.
    #[test]
    fn a_comparator_runs_with_the_guests_thread_pointer() {
        static SAW_DARWIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

        unsafe extern "C" fn compare(a: *const c_void, b: *const c_void) -> c_int {
            if !crate::tls::entered() {
                SAW_DARWIN.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            // SAFETY: `qsort` below is sorting `i32`s.
            unsafe { *a.cast::<i32>() - *b.cast::<i32>() }
        }

        std::thread::spawn(|| {
            // The premise the wrappers rest on, asserted rather than assumed: a
            // thread this runtime started is not inside Darwin code.
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            assert!(
                !crate::tls::entered(),
                "a fresh thread already had the guest's thread pointer"
            );

            let mut values = [5i32, 3, 9, 1];
            // SAFETY: four `i32`s and a comparator that reads two of them.
            unsafe {
                qsort(
                    values.as_mut_ptr().cast(),
                    values.len(),
                    size_of::<i32>(),
                    Some(compare),
                )
            };
            assert_eq!(values, [1, 3, 5, 9]);

            let key = 9i32;
            // SAFETY: as above, over the array just sorted.
            let found = unsafe {
                bsearch(
                    (&raw const key).cast(),
                    values.as_ptr().cast(),
                    values.len(),
                    size_of::<i32>(),
                    Some(compare),
                )
            };
            assert!(!found.is_null(), "the key is in the array");
        })
        .join()
        .expect("the sorting thread");

        assert!(
            SAW_DARWIN.load(std::sync::atomic::Ordering::Relaxed),
            "the comparator ran with the host's thread pointer"
        );
    }
}
