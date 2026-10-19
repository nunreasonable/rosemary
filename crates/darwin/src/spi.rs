//! Apple's system SPI that Chromium-based programs call: the sandbox, the
//! notification center, fileports, per-thread working directories, and the
//! odd Mach call with nothing else to sit beside.
//!
//! None of this process is sandboxed, which is what the sandbox calls say:
//! every check passes and an extension, which grants access beyond a sandbox,
//! has nothing to grant. Darwin notifications are never posted here, so a
//! registration is accepted and never delivers.

use std::ffi::{c_char, c_int, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::mach::kern::*;
use crate::mach::space::{self, Name, Right, TASK_SELF};

/// Whether this process has asked to be put in a sandbox.
static SANDBOXED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `int sandbox_init_with_parameters(const char *profile, uint64_t flags,
/// const char *const parameters[], char **errorbuf)`.
///
/// FIXME (ACCURACY): the profile is accepted and **not enforced** - there is
/// no Seatbelt on this host. The process is what it was, with the access it
/// had; what changes is that it is told it is sandboxed, which is the state
/// a program that just sandboxed itself goes on to check for.
pub unsafe extern "C" fn sandbox_init_with_parameters(
    _profile: *const c_char,
    _flags: u64,
    _parameters: *const *const c_char,
    errorbuf: *mut *mut c_char,
) -> c_int {
    if !errorbuf.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { errorbuf.write(std::ptr::null_mut()) };
    }
    if !SANDBOXED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "rosemary: sandbox_init_with_parameters: accepted, not enforced - this host has no \
             Seatbelt"
        );
    }
    0
}

/// `void sandbox_free_error(char *)`.
pub unsafe extern "C" fn sandbox_free_error(error: *mut c_char) {
    if !error.is_null() {
        // SAFETY: nothing here allocates an error, so this is the caller's own.
        unsafe { libc::free(error.cast()) };
    }
}

/// `int sandbox_check(pid_t, const char *operation, enum sandbox_filter_type, ...)`.
///
/// With no operation it asks whether the process is sandboxed at all: yes
/// for this process once it has called `sandbox_init_with_parameters`, and no
/// otherwise. Any operation is allowed, which is the truth here.
pub unsafe extern "C" fn sandbox_check(pid: i32, operation: *const c_char, _kind: i32) -> c_int {
    let me = pid == 0 || pid == std::process::id() as i32;
    c_int::from(operation.is_null() && me && SANDBOXED.load(Ordering::Relaxed))
}

/// `char *sandbox_extension_issue_file(const char *class, const char *path,
/// uint32_t flags)`: null, with nothing to extend.
pub unsafe extern "C" fn sandbox_extension_issue_file(
    _class: *const c_char,
    _path: *const c_char,
    _flags: u32,
) -> *mut c_char {
    crate::errno::set(libc::ENOTSUP);
    std::ptr::null_mut()
}

/// `int64_t sandbox_extension_consume(const char *token)`: a handle that
/// releases nothing.
pub unsafe extern "C" fn sandbox_extension_consume(_token: *const c_char) -> i64 {
    0
}

/// `int sandbox_extension_release(int64_t handle)`.
pub unsafe extern "C" fn sandbox_extension_release(_handle: i64) -> c_int {
    0
}

/// `int __sandbox_ms(const char *policy, int call, void *arg)`: the MAC policy
/// system call, for a policy that is not loaded.
pub unsafe extern "C" fn sandbox_ms(_policy: *const c_char, _call: c_int, _arg: *mut c_void) -> c_int {
    crate::errno::set(libc::ENOSYS);
    -1
}

/// `const char *const APP_SANDBOX_READ` and `APP_SANDBOX_READ_WRITE`: the
/// extension classes, by their documented values.
#[repr(transparent)]
struct Text(*const u8);
// SAFETY: a pointer to a constant string.
unsafe impl Sync for Text {}

static SANDBOX_READ: Text = Text(c"com.apple.app-sandbox.read".as_ptr().cast());
static SANDBOX_READ_WRITE: Text = Text(c"com.apple.app-sandbox.read-write".as_ptr().cast());

/// `void abort_report_np(const char *format, ...)`: the format alone, since
/// its arguments are the caller's varargs, and then `abort`.
pub unsafe extern "C" fn abort_report_np(format: *const c_char) -> ! {
    if !format.is_null() {
        // SAFETY: a NUL-terminated format, per the caller.
        let text = unsafe { std::ffi::CStr::from_ptr(format) };
        eprintln!("abort_report_np: {}", text.to_string_lossy());
    }
    std::process::abort()
}

/// `int change_fdguard_np(int fd, const guardid_t *, u_int, const guardid_t *,
/// u_int, int *fdflags)`: descriptors are not guarded here, so every guard
/// change succeeds and none is enforced.
pub unsafe extern "C" fn change_fdguard_np(
    _fd: c_int,
    _guard: *const u64,
    _flags: u32,
    _new_guard: *const u64,
    _new_flags: u32,
    _fdflags: *mut c_int,
) -> c_int {
    0
}

/// `int guarded_close_np(int fd, const guardid_t *)`.
pub unsafe extern "C" fn guarded_close_np(fd: c_int, _guard: *const u64) -> c_int {
    // SAFETY: closing the caller's descriptor, which is the call's purpose.
    unsafe { libc::close(fd) }
}

/// `int fileport_makeport(int fd, fileport_t *port)`: a send right standing
/// for the open file, which `fileport_makefd` turns back into a descriptor.
///
/// FIXME (ACCURACY): the right is this process's only - one sent in a
/// message does not take the file with it.
pub unsafe extern "C" fn fileport_makeport(fd: c_int, port: *mut Name) -> c_int {
    if port.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    // SAFETY: a duplicate the fileport owns.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return -1;
    }
    // SAFETY: just made, and owned by nothing else.
    let file = Arc::new(unsafe { OwnedFd::from_raw_fd(copy) });
    let name = space::insert(Right::File { file, refs: 1 });
    // SAFETY: writable, per the caller.
    unsafe { port.write(name) };
    0
}

/// `int fileport_makefd(fileport_t)`: a new descriptor for the file.
pub unsafe extern "C" fn fileport_makefd(port: Name) -> c_int {
    match space::file(port) {
        // SAFETY: duplicating a descriptor the fileport holds open.
        Some(file) => unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) },
        None => {
            crate::errno::set(libc::EINVAL);
            -1
        }
    }
}

std::thread_local! {
    /// Whether this thread has a working directory of its own yet.
    static OWN_DIRECTORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Give this thread a working directory of its own, as `pthread_chdir_np`
/// does on Darwin, so that changing it leaves every other thread's alone.
fn own_directory() -> bool {
    OWN_DIRECTORY.with(|own| {
        if !own.get() {
            // SAFETY: unsharing only this thread's filesystem attributes.
            own.set(unsafe { libc::unshare(libc::CLONE_FS) } == 0);
        }
        own.get()
    })
}

/// `int pthread_chdir_np(const char *)`.
pub unsafe extern "C" fn pthread_chdir_np(path: *const c_char) -> c_int {
    if !own_directory() {
        crate::errno::set(libc::ENOTSUP);
        return -1;
    }
    // SAFETY: the caller's path; the directory changed is this thread's.
    unsafe { crate::fs::chdir(path) }
}

/// `int pthread_fchdir_np(int fd)`: `-1` puts the thread back in the
/// process's directory.
pub unsafe extern "C" fn pthread_fchdir_np(fd: c_int) -> c_int {
    if !own_directory() {
        crate::errno::set(libc::ENOTSUP);
        return -1;
    }
    if fd == -1 {
        // The main thread kept the process's directory.
        let main = format!("/proc/{}/cwd", std::process::id());
        return match std::env::set_current_dir(main) {
            Ok(()) => 0,
            Err(e) => {
                crate::errno::set(e.raw_os_error().unwrap_or(libc::EIO));
                -1
            }
        };
    }
    // SAFETY: the caller's descriptor.
    unsafe { libc::fchdir(fd) }
}

/// `pid_t responsibility_get_pid_responsible_for_pid(pid_t)`: every process
/// here is responsible for itself.
pub unsafe extern "C" fn responsibility_get_pid_responsible_for_pid(pid: i32) -> i32 {
    pid
}

static NEXT_TOKEN: AtomicI32 = AtomicI32::new(1);

/// `uint32_t notify_register_dispatch(const char *name, int *out_token,
/// dispatch_queue_t, notify_handler_t)`.
pub unsafe extern "C" fn notify_register_dispatch(
    _name: *const c_char,
    token: *mut c_int,
    _queue: u64,
    _handler: u64,
) -> u32 {
    if !token.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { token.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    0
}

/// `NOTIFY_REUSE`: deliver on the descriptor the caller passed in.
const NOTIFY_REUSE: c_int = 0x1;

/// `uint32_t notify_register_file_descriptor(const char *name, int *notify_fd,
/// int flags, int *out_token)`: the read end of a pipe nothing writes to.
pub unsafe extern "C" fn notify_register_file_descriptor(
    _name: *const c_char,
    notify_fd: *mut c_int,
    flags: c_int,
    token: *mut c_int,
) -> u32 {
    /// `NOTIFY_STATUS_FAILED`.
    const FAILED: u32 = 1000000;
    if notify_fd.is_null() {
        return FAILED;
    }
    if flags & NOTIFY_REUSE == 0 {
        let mut ends = [0 as c_int; 2];
        // SAFETY: two descriptors for this call to hand out.
        if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return FAILED;
        }
        // The write end is kept open, unused, so the read end never reads EOF.
        std::mem::forget(unsafe { OwnedFd::from_raw_fd(ends[1]) });
        // SAFETY: writable, per the caller.
        unsafe { notify_fd.write(ends[0]) };
    }
    if !token.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { token.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    0
}

/// `uint32_t notify_cancel(int token)`.
pub unsafe extern "C" fn notify_cancel(_token: c_int) -> u32 {
    0
}

/// `mach_port_name_t task_self_trap(void)`.
pub extern "C" fn task_self_trap() -> Name {
    TASK_SELF
}

/// `kern_return_t thread_switch(mach_port_name_t, int option, mach_msg_timeout_t)`:
/// a yield, or for the depressing and waiting options a sleep of the time
/// given, which is the longest Darwin would stay off the processor.
pub extern "C" fn thread_switch(_thread: Name, option: c_int, time_ms: u32) -> i32 {
    match option {
        0 => {
            // SAFETY: a plain yield.
            unsafe { libc::sched_yield() };
        }
        _ => std::thread::sleep(std::time::Duration::from_millis(time_ms.min(1000) as u64)),
    }
    KERN_SUCCESS
}

/// `kern_return_t thread_policy_get(thread_act_t, thread_policy_flavor_t,
/// thread_policy_t, mach_msg_type_number_t *, boolean_t *get_default)`: every
/// policy at its default, which is what `thread_policy_set` leaves it at.
pub unsafe extern "C" fn thread_policy_get(
    _thread: Name,
    _flavor: u32,
    policy: *mut u32,
    count: *mut u32,
    get_default: *mut u32,
) -> i32 {
    if !policy.is_null() && !count.is_null() {
        // SAFETY: `*count` words, per the caller.
        unsafe { std::ptr::write_bytes(policy, 0, count.read() as usize) };
    }
    if !get_default.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { get_default.write(1) };
    }
    KERN_SUCCESS
}

/// `kern_return_t task_policy_set(task_t, task_policy_flavor_t, task_policy_t,
/// mach_msg_type_number_t)`: a scheduling hint, accepted and dropped as
/// `thread_policy_set`'s are.
pub extern "C" fn task_policy_set(task: Name, _flavor: u32, _policy: *const u32, _count: u32) -> i32 {
    match task {
        TASK_SELF => KERN_SUCCESS,
        _ => KERN_INVALID_ARGUMENT,
    }
}

/// `kern_return_t host_processor_info(host_t, processor_flavor_t, natural_t
/// *processor_count, processor_info_array_t *, mach_msg_type_number_t *)`:
/// `PROCESSOR_CPU_LOAD_INFO` from the host's own counters, and
/// `PROCESSOR_BASIC_INFO`. The array is the caller's to `vm_deallocate`.
pub unsafe extern "C" fn host_processor_info(
    _host: Name,
    flavor: c_int,
    processor_count: *mut u32,
    info: *mut u64,
    info_count: *mut u32,
) -> i32 {
    const BASIC_INFO: c_int = 1;
    const CPU_LOAD_INFO: c_int = 2;
    if processor_count.is_null() || info.is_null() || info_count.is_null() {
        return KERN_INVALID_ARGUMENT;
    }
    let stat = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let cpus: Vec<Vec<u64>> = stat
        .lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .map(|l| l.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect())
        .collect();
    let words: Vec<u32> = match flavor {
        // `cpu_ticks[CPU_STATE_MAX]`: user, system, idle, nice.
        CPU_LOAD_INFO => cpus
            .iter()
            .flat_map(|c| {
                let at = |i: usize| c.get(i).copied().unwrap_or(0) as u32;
                [at(0), at(2), at(3), at(1)]
            })
            .collect(),
        // cpu_type, cpu_subtype, running, slot_num, is_master.
        BASIC_INFO => (0..cpus.len())
            .flat_map(|i| [0x0100_0007, 3, 1, i as u32, u32::from(i == 0)])
            .collect(),
        _ => return KERN_INVALID_ARGUMENT,
    };
    let mut array = 0u64;
    let bytes = (words.len() * 4).max(1) as u64;
    // SAFETY: fresh pages for the caller.
    let status = unsafe { crate::mach::vm::vm_allocate(TASK_SELF, &mut array, bytes, 1) };
    if status != KERN_SUCCESS {
        return status;
    }
    // SAFETY: the pages just allocated, and the caller's out pointers.
    unsafe {
        std::ptr::copy_nonoverlapping(words.as_ptr(), array as *mut u32, words.len());
        processor_count.write(cpus.len() as u32);
        info.write(array);
        info_count.write(words.len() as u32);
    }
    KERN_SUCCESS
}

/// `pthread_t pthread_from_mach_thread_np(mach_port_t)`: null, which Darwin
/// answers for a thread that is not one of this process's pthreads.
///
/// FIXME (ACCURACY): a thread of this process is not found by its port.
pub extern "C" fn pthread_from_mach_thread_np(_port: Name) -> *mut c_void {
    std::ptr::null_mut()
}

/// `int posix_spawnattr_set_csm_np(posix_spawnattr_t *, uint32_t flags)`:
/// the CPU security mitigations a child should run with. The host applies
/// its own, so the request is accepted.
pub unsafe extern "C" fn posix_spawnattr_set_csm_np(_attr: *mut c_void, _flags: u32) -> c_int {
    0
}

/// `uintptr_t pthread_stack_frame_decode_np(uintptr_t frame, uintptr_t
/// *return_address)`: the frame-pointer record at `frame`, unsigned - there
/// are no signed return addresses on x86-64.
///
/// # Safety
///
/// `frame` addresses a frame record; `return_address` is null or writable.
pub unsafe extern "C" fn pthread_stack_frame_decode_np(frame: usize, return_address: *mut usize) -> usize {
    // SAFETY: a frame record is the saved frame pointer and the return address.
    unsafe {
        if !return_address.is_null() {
            return_address.write(*((frame + 8) as *const usize));
        }
        *(frame as *const usize)
    }
}

/// `boolean_t voucher_mach_msg_set(mach_msg_header_t *)`: no voucher is
/// attached, as for a thread that has adopted none.
pub unsafe extern "C" fn voucher_mach_msg_set(_message: *mut c_void) -> u32 {
    0
}

exports::exports! {
    functions {
        "sandbox_check" => sandbox_check,
        "posix_spawnattr_set_csm_np" => posix_spawnattr_set_csm_np,
        "pthread_stack_frame_decode_np" => pthread_stack_frame_decode_np,
        "voucher_mach_msg_set" => voucher_mach_msg_set,
        "sandbox_init_with_parameters" => sandbox_init_with_parameters,
        "sandbox_free_error" => sandbox_free_error,
        "sandbox_extension_issue_file" => sandbox_extension_issue_file,
        "sandbox_extension_consume" => sandbox_extension_consume,
        "sandbox_extension_release" => sandbox_extension_release,
        "__sandbox_ms" => sandbox_ms,
        "abort_report_np" => abort_report_np,
        "change_fdguard_np" => change_fdguard_np,
        "guarded_close_np" => guarded_close_np,
        "fileport_makeport" => fileport_makeport,
        "fileport_makefd" => fileport_makefd,
        "pthread_chdir_np" => pthread_chdir_np,
        "pthread_fchdir_np" => pthread_fchdir_np,
        "responsibility_get_pid_responsible_for_pid" => responsibility_get_pid_responsible_for_pid,
        "notify_register_dispatch" => notify_register_dispatch,
        "notify_register_file_descriptor" => notify_register_file_descriptor,
        "notify_cancel" => notify_cancel,
        "task_self_trap" => task_self_trap,
        "thread_switch" => thread_switch,
        "thread_policy_get" => thread_policy_get,
        "task_policy_set" => task_policy_set,
        "host_processor_info" => host_processor_info,
        "pthread_from_mach_thread_np" => pthread_from_mach_thread_np,
    }
    data {
        "_APP_SANDBOX_READ" => &SANDBOX_READ as *const Text as u64,
        "_APP_SANDBOX_READ_WRITE" => &SANDBOX_READ_WRITE as *const Text as u64,
    }
}
