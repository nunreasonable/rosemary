//! `os_unfair_lock`, from `<os/lock.h>`.
//!
//! The lock is a `uint32_t` the guest allocates (`OS_UNFAIR_LOCK_INIT` is zero)
//! and Apple documents it as opaque, so the encoding is ours: zero is unlocked,
//! otherwise `owner << 1`, with bit 0 set once a thread has parked on it. The
//! owner is what makes `os_unfair_lock_assert_owner` and the "unlock from the
//! wrong thread" crash answerable.
//!
//! Like Darwin's, misuse is fatal rather than an error code: the header says
//! recursive locking and unlocking from another thread are undefined, and the
//! real implementation aborts on both.

use std::cell::Cell;
use std::ffi::c_void;
use std::hint::spin_loop;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::wait_word::{self, Scope, WakeCount};

const WAITERS: u32 = 1;

/// `OS_UNFAIR_LOCK_ADAPTIVE_SPIN`: spin a little before parking.
const ADAPTIVE_SPIN: u32 = 0x0004_0000;

const SPINS: u32 = 100;

#[derive(Debug, PartialEq, Eq)]
enum Misuse {
    Recursive,
    NotOwner,
    Owned,
}

impl Misuse {
    fn message(&self) -> &'static str {
        match self {
            Misuse::Recursive => {
                "BUG IN CLIENT OF LIBPLATFORM: Trying to recursively lock an os_unfair_lock"
            }
            Misuse::NotOwner => "BUG IN CLIENT OF LIBPLATFORM: Not locked by current thread",
            Misuse::Owned => "BUG IN CLIENT OF LIBPLATFORM: Lock is owned by current thread",
        }
    }
}

#[cold]
fn die(misuse: Misuse) -> ! {
    eprintln!("{}", misuse.message());
    std::process::abort()
}

fn me() -> u32 {
    // The owner only has to be unique among this process's live threads. A
    // kernel tid costs a syscall per call, and a cached one goes stale in a
    // fork child, where the kernel can hand it to a new thread.
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! {
        static ME: Cell<u32> = const { Cell::new(0) };
    }
    ME.with(|me| {
        if me.get() == 0 {
            // Zero would read as unlocked, and the shift must not lose the top bit.
            let id = NEXT.fetch_add(1, Ordering::Relaxed) & 0x7fff_ffff;
            me.set(id.max(1) << 1);
        }
        me.get()
    })
}

/// # Safety
///
/// `lock` is a writable, aligned `os_unfair_lock`.
#[inline(always)]
unsafe fn word<'a>(lock: *mut c_void) -> &'a AtomicU32 {
    unsafe { AtomicU32::from_ptr(lock.cast()) }
}

#[inline]
fn try_lock(word: &AtomicU32, owner: u32) -> bool {
    word.compare_exchange(0, owner, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

fn lock(word: &AtomicU32, owner: u32, spin: bool) -> Result<(), Misuse> {
    if try_lock(word, owner) {
        return Ok(());
    }
    if spin {
        for _ in 0..SPINS {
            if word.load(Ordering::Relaxed) == 0 && try_lock(word, owner) {
                return Ok(());
            }
            spin_loop();
        }
    }
    loop {
        let state = word.load(Ordering::Relaxed);
        if state == 0 {
            // Others may be parked behind a holder we never saw, so keep the
            // waiters bit: a spurious wake on unlock is harmless.
            if word
                .compare_exchange_weak(0, owner | WAITERS, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
            continue;
        }
        if state & !WAITERS == owner {
            return Err(Misuse::Recursive);
        }
        let marked = state | WAITERS;
        if state != marked
            && word
                .compare_exchange_weak(state, marked, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            continue;
        }
        // SAFETY: `word` is the caller's lock, which outlives the call.
        // A changed word or a signal just sends us round again.
        let _ = unsafe { wait_word::wait_u32(word, marked, None, Scope::Private) };
    }
}

fn unlock(word: &AtomicU32, owner: u32) -> Result<(), Misuse> {
    let state = word.load(Ordering::Relaxed);
    if state & !WAITERS != owner {
        return Err(Misuse::NotOwner);
    }
    // Only the owner writes while it holds the lock apart from the waiters
    // bit, so a failed exchange means a waiter marked it and we retry.
    let before = word.swap(0, Ordering::Release);
    if before & WAITERS != 0 {
        // SAFETY: as in `lock`.
        let _ = unsafe { wait_word::wake_u32(word, WakeCount::One, Scope::Private) };
    }
    Ok(())
}

fn assert_owner(word: &AtomicU32, owner: u32, held: bool) -> Result<(), Misuse> {
    let owned = word.load(Ordering::Relaxed) & !WAITERS == owner;
    match (held, owned) {
        (true, false) => Err(Misuse::NotOwner),
        (false, true) => Err(Misuse::Owned),
        _ => Ok(()),
    }
}

#[inline]
fn check(result: Result<(), Misuse>) {
    if let Err(misuse) = result {
        die(misuse);
    }
}

/// `void os_unfair_lock_lock(os_unfair_lock_t lock)`
///
/// # Safety
///
/// `lock` is a writable, aligned `os_unfair_lock`.
pub unsafe extern "C" fn os_unfair_lock_lock(lock_ptr: *mut c_void) {
    check(lock(unsafe { word(lock_ptr) }, me(), false));
}

/// `void os_unfair_lock_lock_with_options(os_unfair_lock_t, os_unfair_lock_options_t)`
///
/// `OS_UNFAIR_LOCK_DATA_SYNCHRONIZATION` is a hint to the kernel's
/// priority-donation machinery and changes nothing observable here.
///
/// # Safety
///
/// As [`os_unfair_lock_lock`].
pub unsafe extern "C" fn os_unfair_lock_lock_with_options(lock_ptr: *mut c_void, options: u32) {
    check(lock(
        unsafe { word(lock_ptr) },
        me(),
        options & ADAPTIVE_SPIN != 0,
    ));
}

/// `bool os_unfair_lock_trylock(os_unfair_lock_t lock)`
///
/// # Safety
///
/// As [`os_unfair_lock_lock`].
pub unsafe extern "C" fn os_unfair_lock_trylock(lock_ptr: *mut c_void) -> bool {
    try_lock(unsafe { word(lock_ptr) }, me())
}

/// `void os_unfair_lock_unlock(os_unfair_lock_t lock)`
///
/// # Safety
///
/// As [`os_unfair_lock_lock`].
pub unsafe extern "C" fn os_unfair_lock_unlock(lock_ptr: *mut c_void) {
    check(unlock(unsafe { word(lock_ptr) }, me()));
}

/// `void os_unfair_lock_assert_owner(const os_unfair_lock *lock)`
///
/// # Safety
///
/// As [`os_unfair_lock_lock`].
pub unsafe extern "C" fn os_unfair_lock_assert_owner(lock_ptr: *mut c_void) {
    check(assert_owner(unsafe { word(lock_ptr) }, me(), true));
}

/// `void os_unfair_lock_assert_not_owner(const os_unfair_lock *lock)`
///
/// # Safety
///
/// As [`os_unfair_lock_lock`].
pub unsafe extern "C" fn os_unfair_lock_assert_not_owner(lock_ptr: *mut c_void) {
    check(assert_owner(unsafe { word(lock_ptr) }, me(), false));
}

exports::exports! {
    functions {
        "os_unfair_lock_lock" => os_unfair_lock_lock,
        "os_unfair_lock_lock_with_options" => os_unfair_lock_lock_with_options,
        "os_unfair_lock_trylock" => os_unfair_lock_trylock,
        "os_unfair_lock_unlock" => os_unfair_lock_unlock,
        "os_unfair_lock_assert_owner" => os_unfair_lock_assert_owner,
        "os_unfair_lock_assert_not_owner" => os_unfair_lock_assert_not_owner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn trylock_fails_while_held_and_succeeds_after_unlock() {
        let w = AtomicU32::new(0);
        let a = me();
        assert!(try_lock(&w, a));
        assert!(!try_lock(&w, a));
        assert_eq!(unlock(&w, a), Ok(()));
        assert_eq!(w.load(Ordering::Relaxed), 0);
        assert!(try_lock(&w, a));
    }

    #[test]
    fn recursive_lock_is_reported_not_deadlocked() {
        let w = AtomicU32::new(0);
        let a = me();
        assert_eq!(lock(&w, a, false), Ok(()));
        assert_eq!(lock(&w, a, false), Err(Misuse::Recursive));
    }

    #[test]
    fn unlock_by_a_non_owner_is_misuse() {
        let w = AtomicU32::new(0);
        assert_eq!(unlock(&w, me()), Err(Misuse::NotOwner));
        assert!(try_lock(&w, me()));
        assert_eq!(unlock(&w, me() ^ 0x10), Err(Misuse::NotOwner));
    }

    #[test]
    fn owner_assertions_follow_the_holder() {
        let w = AtomicU32::new(0);
        let a = me();
        assert_eq!(assert_owner(&w, a, true), Err(Misuse::NotOwner));
        assert_eq!(assert_owner(&w, a, false), Ok(()));
        assert!(try_lock(&w, a));
        assert_eq!(assert_owner(&w, a, true), Ok(()));
        assert_eq!(assert_owner(&w, a, false), Err(Misuse::Owned));
    }

    #[test]
    fn exported_entry_points_lock_a_guest_word() {
        let mut guest = 0u32;
        let p = (&mut guest as *mut u32).cast::<c_void>();
        unsafe {
            os_unfair_lock_lock(p);
            assert_ne!(guest, 0);
            assert!(!os_unfair_lock_trylock(p));
            os_unfair_lock_assert_owner(p);
            os_unfair_lock_unlock(p);
            assert_eq!(guest, 0);
            os_unfair_lock_assert_not_owner(p);
            os_unfair_lock_lock_with_options(p, ADAPTIVE_SPIN | 0x10000);
            os_unfair_lock_unlock(p);
        }
        assert_eq!(guest, 0);
    }

    #[test]
    fn contended_threads_exclude_each_other() {
        let w = Arc::new(AtomicU32::new(0));
        let count = Arc::new(AtomicUsize::new(0));
        let inside = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|i| {
                let (w, count, inside) = (w.clone(), count.clone(), inside.clone());
                std::thread::spawn(move || {
                    let me = me();
                    for _ in 0..2000 {
                        lock(&w, me, i % 2 == 0).unwrap();
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0);
                        count.fetch_add(1, Ordering::Relaxed);
                        inside.fetch_sub(1, Ordering::SeqCst);
                        unlock(&w, me).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(count.load(Ordering::Relaxed), 8000);
        assert_eq!(w.load(Ordering::Relaxed), 0);
    }
}
