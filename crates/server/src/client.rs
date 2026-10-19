//! A guest process's side of the server.
//!
//! One connection per process, opened on first use and kept: the connection
//! is how the server knows the process is its own (the socket names its
//! peer), so a process that checked in keeps it for life. A child made by
//! `fork` inherits the parent's descriptor, which still names the parent, so
//! the child opens its own and checks in again as itself.
//!
//! Nothing here is fatal. Without a server a process sees itself and nothing
//! else, which is the most a process could see before the server existed.

use std::cell::UnsafeCell;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crate::codec;
use crate::credentials::{Change, Credentials, Denied};
use crate::location;
use crate::protocol::{Checkin, Process, Reply, Request, Thread, Usage, VERSION};

/// One connection.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(socket: &Path) -> io::Result<Client> {
        let stream = UnixStream::connect(socket)?;
        // A server that stops answering must not hang the guest forever.
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Client { stream })
    }

    pub fn call(&mut self, request: &Request) -> io::Result<Reply> {
        codec::write_frame(&mut self.stream, &codec::encode(request))?;
        let frame = codec::read_frame(&mut self.stream)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        Ok(codec::decode(&frame)?)
    }
}

/// Starts a server for a prefix directory. The runtime supplies this - it is
/// the binary that knows how to run itself as one.
pub type Launcher = fn(&Path) -> io::Result<()>;

pub struct Config {
    /// The directory the prefix's server listens in; see [`location`].
    pub directory: PathBuf,
    pub launch: Option<Launcher>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Point this process at a server. Until this is called nothing here connects,
/// which is what a test binary or a host without the server wants.
pub fn configure(config: Config) {
    if CONFIG.set(config).is_ok() {
        // SAFETY: nothing has locked it yet: every lock is behind
        // `configured`, which only now becomes true.
        unsafe { make_recursive(SESSION.mutex.get()) };
        // SAFETY: the three handlers are `extern "C"` functions with no
        // arguments that touch only `SESSION`'s mutex.
        unsafe { libc::pthread_atfork(Some(before_fork), Some(after_fork), Some(in_child)) };
    }
}

pub fn configured() -> bool {
    CONFIG.get().is_some()
}

struct Session {
    client: Option<Client>,
    /// The pid the client was opened by; a different `getpid` is a fork child
    /// holding its parent's connection.
    pid: i32,
    checkin: Option<Checkin>,
    /// When connecting last failed, so a process without a server does not
    /// try - and launch - on every question.
    failed_at: Option<Instant>,
    warned: bool,
}

/// A mutex `fork` cannot leave held in the child.
///
/// Held across `fork` by the `pthread_atfork` handlers, so a child never
/// inherits it mid-request from a thread that does not exist in the child.
/// A `std` mutex cannot be locked and unlocked from two separate callbacks.
struct ForkSafe<T> {
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    value: UnsafeCell<T>,
}

// SAFETY: `value` is only reached through `lock`, which holds the mutex.
unsafe impl<T: Send> Sync for ForkSafe<T> {}

struct Guard<'a, T> {
    owner: &'a ForkSafe<T>,
}

impl<T> ForkSafe<T> {
    const fn new(value: T) -> Self {
        Self {
            mutex: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            value: UnsafeCell::new(value),
        }
    }

    fn lock(&self) -> Guard<'_, T> {
        // SAFETY: an initialised mutex this value owns.
        unsafe { libc::pthread_mutex_lock(self.mutex.get()) };
        Guard { owner: self }
    }
}

impl<T> std::ops::Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard holds the mutex.
        unsafe { &*self.owner.value.get() }
    }
}

impl<T> std::ops::DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard holds the mutex.
        unsafe { &mut *self.owner.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: locked by this guard.
        unsafe { libc::pthread_mutex_unlock(self.owner.mutex.get()) };
    }
}

static SESSION: ForkSafe<Session> = ForkSafe::new(Session {
    client: None,
    pid: 0,
    checkin: None,
    failed_at: None,
    warned: false,
});

extern "C" fn before_fork() {
    // SAFETY: the session's own mutex; released by one of the two below.
    unsafe { libc::pthread_mutex_lock(SESSION.mutex.get()) };
}

extern "C" fn after_fork() {
    // SAFETY: locked by `before_fork` on this thread.
    unsafe { libc::pthread_mutex_unlock(SESSION.mutex.get()) };
}

extern "C" fn in_child() {
    // Made afresh rather than unlocked: the owner it records is a thread id
    // the child no longer has, and a recursive lock taken twice - a launch
    // from inside a call - would need two unlocks besides.
    // SAFETY: the child has one thread, and it is this one.
    unsafe { make_recursive(SESSION.mutex.get()) };
}

/// Recursive, because launching a server happens inside a call and spawning
/// the launcher may `fork`, whose prepare handler takes the lock again.
///
/// # Safety
///
/// Nothing else holds or waits on `mutex`.
unsafe fn make_recursive(mutex: *mut libc::pthread_mutex_t) {
    let mut attr = std::mem::MaybeUninit::<libc::pthread_mutexattr_t>::uninit();
    // SAFETY: an attribute object initialised before use and destroyed after,
    // and a mutex the caller states is free.
    unsafe {
        libc::pthread_mutexattr_init(attr.as_mut_ptr());
        libc::pthread_mutexattr_settype(attr.as_mut_ptr(), libc::PTHREAD_MUTEX_RECURSIVE);
        libc::pthread_mutex_init(mutex, attr.as_ptr());
        libc::pthread_mutexattr_destroy(attr.as_mut_ptr());
    }
}

fn pid() -> i32 {
    starnix::process::me()
}

/// How long a process waits for a server it launched to start listening.
const STARTUP: Duration = Duration::from_secs(5);
/// How long after a failure before trying again.
const RETRY: Duration = Duration::from_secs(2);

fn open(config: &Config, launch: bool) -> io::Result<Client> {
    let socket = location::socket_in(&config.directory);
    match Client::connect(&socket) {
        Ok(client) => return Ok(client),
        Err(e) if !launch => return Err(e),
        Err(_) => {}
    }
    let Some(start) = config.launch else {
        return Client::connect(&socket);
    };
    start(&config.directory)?;
    let deadline = Instant::now() + STARTUP;
    let mut pause = Duration::from_millis(2);
    loop {
        match Client::connect(&socket) {
            Ok(client) => return Ok(client),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
        }
    }
}

/// Make sure `session` has a connection that is this process's own, checking
/// in on a fresh one.
fn ensure(session: &mut Session, launch: bool) -> Option<()> {
    let config = CONFIG.get()?;
    if session.pid != pid() {
        // Closes this process's copy of the parent's descriptor only.
        session.client = None;
        session.pid = pid();
        session.failed_at = None;
    }
    if session.client.is_some() {
        return Some(());
    }
    if session.failed_at.is_some_and(|at| at.elapsed() < RETRY) {
        return None;
    }
    // A server that is winding down refuses new check-ins, and goes away soon
    // after; this process waits for it to, and then launches its successor,
    // rather than running without one. Two runs back to back meet exactly
    // that: the first's server lingers a few seconds and then stops just as
    // the second starts.
    let deadline = Instant::now() + SHUTDOWN_WAIT;
    let mut attempt = connect_and_check_in(session, config, launch);
    while let Err(e) = &attempt
        && e.to_string() == SHUTTING_DOWN
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(100));
        attempt = connect_and_check_in(session, config, launch);
    }
    match attempt {
        Ok(client) => {
            session.client = Some(client);
            session.failed_at = None;
            Some(())
        }
        Err(e) => {
            session.failed_at = Some(Instant::now());
            if !session.warned {
                session.warned = true;
                eprintln!(
                    "rosemary: the process server is unavailable ({e}); this process cannot \
                     see any other"
                );
            }
            None
        }
    }
}

/// The refusal a server that is shutting down gives a new check-in.
const SHUTTING_DOWN: &str = "the prefix is shutting down";

/// How long a process waits for a server that is shutting down to go.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// Connect, launching a server if `launch` allows, and check in.
fn connect_and_check_in(session: &mut Session, config: &Config, launch: bool) -> io::Result<Client> {
    open(config, launch).and_then(|mut client| {
        if let Some(checkin) = &mut session.checkin {
            checkin.pid = pid();
            match client.call(&Request::Checkin(checkin.clone()))? {
                // From now on this process is who the server says, and a later
                // checkin - a `fork` child's, or one after the server restarted
                // - says so rather than inheriting afresh.
                Reply::CheckedIn {
                    credentials,
                    launchd,
                    ..
                } => {
                    LAUNCHD.store(launchd, Ordering::Relaxed);
                    checkin.credentials = credentials;
                    checkin.fresh = false;
                    checkin.setid = Default::default();
                }
                Reply::Refused(why) => return Err(io::Error::other(why)),
                _ => return Err(io::Error::other("unexpected reply to a checkin")),
            }
        }
        Ok(client)
    })
}

/// Send `request`, reconnecting once if the connection broke.
pub fn call(request: &Request) -> Option<Reply> {
    if !configured() {
        return None;
    }
    let mut session = SESSION.lock();
    for _ in 0..2 {
        ensure(&mut session, true)?;
        match session.client.as_mut()?.call(request) {
            Ok(reply) => return Some(reply),
            // The server went away - restarted, or killed - and the next
            // attempt checks in with whichever replaced it.
            Err(_) => session.client = None,
        }
    }
    None
}

/// Introduce this process to the server; it is listed from now on.
///
/// Returns who the process is, as the server decided from what it inherits,
/// or `None` without a server - and then the caller decides by the same rules.
pub fn check_in(mut checkin: Checkin) -> Option<Credentials> {
    if !configured() {
        return None;
    }
    checkin.version = VERSION;
    checkin.pid = pid();
    let mut session = SESSION.lock();
    session.checkin = Some(checkin);
    // A connection opened before the checkin carried none; start over so the
    // server hears it on the connection that will stay open.
    session.client = None;
    ensure(&mut session, true)?;
    let checkin = session.checkin.as_ref()?;
    (!checkin.fresh).then(|| checkin.credentials.clone())
}

/// The host pid of the prefix's launchd, as of this process's last check-in.
static LAUNCHD: AtomicI32 = AtomicI32::new(0);

/// The host pid of the prefix's launchd, which guests know as pid 1. `None`
/// without a server, or when the prefix has no launchd running.
pub fn launchd() -> Option<i32> {
    match LAUNCHD.load(Ordering::Relaxed) {
        0 => None,
        pid => Some(pid),
    }
}

/// Change this process's ids. `None` means there is no server to ask, and the
/// caller applies the change itself and tells [`remember`].
pub fn change_credentials(change: Change) -> Option<Result<Credentials, Denied>> {
    let reply = call(&Request::Credentials(change))?;
    let Reply::Credentials(result) = reply else {
        return None;
    };
    if let Ok(credentials) = &result {
        remember(credentials.clone());
    }
    Some(result)
}

/// Record this process's ids for its next checkin: a `fork` child's, or one
/// after the server went away and came back.
pub fn remember(credentials: Credentials) {
    if !configured() {
        return;
    }
    let mut session = SESSION.lock();
    if let Some(checkin) = &mut session.checkin {
        checkin.credentials = credentials;
        checkin.fresh = false;
        checkin.setid = Default::default();
    }
}

/// Called in a guest's `fork` child: check in as the new process straight
/// away, so it is listed even if it never asks anything itself.
///
/// No launch from here: a server the parent reached is still there, and one
/// it could not reach will not appear for the child either.
pub fn forked() {
    if !configured() {
        return;
    }
    let mut session = SESSION.lock();
    if session.checkin.is_none() {
        return;
    }
    ensure(&mut session, false);
}

pub fn processes() -> Option<Vec<Process>> {
    match call(&Request::Processes)? {
        Reply::Processes(list) => Some(list),
        _ => None,
    }
}

pub fn process(pid: i32) -> Option<Process> {
    match call(&Request::Process { pid })? {
        Reply::Process(process) => process,
        _ => None,
    }
}

pub fn usage(pid: i32) -> Option<Usage> {
    match call(&Request::Usage { pid })? {
        Reply::Usage(usage) => usage,
        _ => None,
    }
}

pub fn threads(pid: i32) -> Option<Vec<Thread>> {
    match call(&Request::Threads { pid })? {
        Reply::Threads(threads) => threads,
        _ => None,
    }
}

pub fn name_thread(tid: i32, name: &str) {
    let _ = call(&Request::ThreadName {
        tid,
        name: name.to_owned(),
    });
}
