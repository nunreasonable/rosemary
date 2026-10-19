use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::{Arc, RwLock};

use super::kern::*;
use super::wire::{Address, Receiver};

/// `mach_port_name_t`, and the two values that are not names.
pub type Name = u32;
pub const MACH_PORT_NULL: Name = 0;
pub const MACH_PORT_DEAD: Name = !0;

/// `MACH_PORT_RIGHT_*`, in the order `<mach/port.h>` declares them. Send is
/// zero, which is the one worth reading twice - `mach_port_allocate` is nearly
/// always called with `1`.
pub const RIGHT_SEND: u32 = 0;
pub const RIGHT_RECEIVE: u32 = 1;
pub const RIGHT_SEND_ONCE: u32 = 2;
pub const RIGHT_PORT_SET: u32 = 3;
pub const RIGHT_DEAD_NAME: u32 = 4;

/// `MACH_PORT_TYPE(right)`, which is `1 << (right + 16)`.
pub const fn type_of(right: u32) -> u32 {
    1 << (right + 16)
}

/// The names that exist before anything is allocated.
///
/// Fixed rather than drawn from the counter so that `mach_task_self_` can be a
/// constant: it is a *data* import, bound before a line of guest code runs, and
/// a data import that read zero is the segfault this whole directory exists to
/// stop.
pub const TASK_SELF: Name = 0x103;
pub const HOST_SELF: Name = 0x203;
pub const BOOTSTRAP: Name = 0x303;
/// The two clock services `host_get_clock_service` hands out. Fixed because
/// every caller asking for the same clock gets the same port on a real system.
pub const SYSTEM_CLOCK: Name = 0x403;
pub const CALENDAR_CLOCK: Name = 0x503;
/// The one processor set there is, by its name port and its control port.
/// `host_processor_sets` hands out the first and `host_processor_set_priv`
/// turns it into the second, which is what enumerates tasks.
pub const PSET_NAME: Name = 0x603;
pub const PSET_CONTROL: Name = 0x703;
/// The first name [`allocate`] hands out. Above the fixed ones, and stepped by
/// four the way a real space's names are, so a caller that assumes a name is
/// not a small integer is not surprised.
const FIRST_DYNAMIC: Name = 0x1003;

/// What a name refers to.
pub enum Right {
    /// The receiving end. Held by `Arc` because a port set names its members and
    /// a receive in progress must keep the socket alive.
    Receive(Arc<Receiver>),
    /// A send right: the address of somebody else's receive right, and the user
    /// reference count Mach makes the caller balance.
    ///
    /// `claim` is what makes this right count as a sender: an open connection to
    /// the port's listener, closed when the name is, or when this process exits.
    /// `None` when the port could not be reached, which reports no senders
    /// changes for it.
    Send {
        address: Address,
        refs: u32,
        claim: Option<OwnedFd>,
    },
    /// A send-once right, which is destroyed by the message that uses it.
    SendOnce { address: Address },
    /// A port set, which has no queue of its own - only members.
    Set(Vec<Name>),
    /// A right whose port has been destroyed.
    Dead { refs: u32 },
    /// A Mach semaphore, which is named out of the same space and is not a port.
    Semaphore(Arc<super::sync::Semaphore>),
    /// A send right to a task: this one under a name other than
    /// [`TASK_SELF`], or another guest's, from `task_for_pid`. The pid is the
    /// whole of it - every question about the task goes to the process server.
    Task { pid: i32, refs: u32 },
    /// A send right to a thread of some task, by its host thread id.
    Thread { pid: i32, tid: i32, refs: u32 },
    /// A name this runtime answers for without a port behind it: the task, the
    /// host, the bootstrap server, a clock, the processor set.
    Special,
    /// A named memory entry, from `mach_make_memory_entry_64`: a send right
    /// to a memory object, which here is a memory file. `offset` and `size`
    /// are the part of it the entry covers.
    Memory {
        file: Arc<OwnedFd>,
        offset: u64,
        size: u64,
        refs: u32,
    },
    /// A fileport: a send right standing for an open file, from
    /// `fileport_makeport`.
    File { file: Arc<OwnedFd>, refs: u32 },
}

impl Right {
    fn type_bits(&self) -> u32 {
        match self {
            Right::Receive(_) => type_of(RIGHT_RECEIVE),
            Right::Send { .. } => type_of(RIGHT_SEND),
            Right::SendOnce { .. } => type_of(RIGHT_SEND_ONCE),
            Right::Set(_) => type_of(RIGHT_PORT_SET),
            Right::Dead { .. } => type_of(RIGHT_DEAD_NAME),
            // Neither is a port right. A caller asking after one is asking about
            // something this host does not model, and `mach_port_type` reports
            // it as a send right because that is what the task and host ports
            // are on a real system.
            Right::Semaphore(_) | Right::Special => type_of(RIGHT_SEND),
            Right::Task { .. }
            | Right::Thread { .. }
            | Right::Memory { .. }
            | Right::File { .. } => {
                type_of(RIGHT_SEND)
            }
        }
    }
}

struct Space {
    names: HashMap<Name, Right>,
    next: Name,
    /// User references on the send right a receive right's own name also
    /// holds, which `MAKE_SEND` under that name adds and `mach_port_deallocate`
    /// takes away again without touching the receive right.
    sends: HashMap<Name, u32>,
}

fn space() -> &'static RwLock<Space> {
    static SPACE: std::sync::OnceLock<RwLock<Space>> = std::sync::OnceLock::new();
    SPACE.get_or_init(|| {
        let mut names = HashMap::new();
        names.insert(TASK_SELF, Right::Special);
        names.insert(HOST_SELF, Right::Special);
        names.insert(BOOTSTRAP, Right::Special);
        names.insert(SYSTEM_CLOCK, Right::Special);
        names.insert(CALENDAR_CLOCK, Right::Special);
        names.insert(PSET_NAME, Right::Special);
        names.insert(PSET_CONTROL, Right::Special);
        RwLock::new(Space {
            names,
            next: FIRST_DYNAMIC,
            sends: HashMap::new(),
        })
    })
}

fn with<R>(f: impl FnOnce(&mut Space) -> R) -> R {
    let mut guard = space().write().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Put `right` in the table under a fresh name.
pub fn insert(right: Right) -> Name {
    with(|space| {
        let name = space.next;
        space.next = space.next.wrapping_add(4);
        space.names.insert(name, right);
        name
    })
}

/// The address a send or send-once right names, and whether it was send-once.
///
/// A send-once right is *not* consumed here: a message that fails to send has
/// not used it, so [`super::msg`] consumes it only once the datagram is away.
pub fn destination(name: Name) -> Result<(Address, bool), i32> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::Send { address, .. }) => Ok((*address, false)),
        Some(Right::SendOnce { address }) => Ok((*address, true)),
        // A receive right is a legal destination: a task can send to itself.
        Some(Right::Receive(port)) => Ok((port.address(), false)),
        Some(Right::Dead { .. }) => Err(MACH_SEND_INVALID_DEST),
        Some(_) => Err(MACH_SEND_INVALID_RIGHT),
        None => Err(MACH_SEND_INVALID_DEST),
    }
}

/// The receive right `name` refers to.
pub fn receiver(name: Name) -> Option<Arc<Receiver>> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::Receive(port)) => Some(port.clone()),
        _ => None,
    }
}

/// Look at whatever `name` refers to, without copying it out.
pub fn with_right<R>(name: Name, f: impl FnOnce(&Right) -> R) -> Option<R> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    guard.names.get(&name).map(f)
}

/// The members of a port set, or `None` if `name` is not one.
pub fn members(name: Name) -> Option<Vec<Name>> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::Set(members)) => Some(members.clone()),
        _ => None,
    }
}

/// The address a right names, whatever kind it is - for a descriptor that has
/// to be encoded without regard to what the caller is doing with it.
pub fn address_of(name: Name) -> Option<Address> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::Receive(port)) => Some(port.address()),
        Some(Right::Send { address, .. }) | Some(Right::SendOnce { address }) => Some(*address),
        _ => None,
    }
}

/// Take a receive right out of the space so its descriptor can be sent
/// elsewhere - `MACH_MSG_TYPE_MOVE_RECEIVE`.
///
/// Fails when something else still holds the port: a member of a port set, or a
/// receive in progress on another thread. Mach would refuse the same move for
/// the same reason, and moving it anyway would leave this process reading a
/// socket another process now owns.
pub fn move_receive(name: Name) -> Result<Receiver, i32> {
    let taken = with(|space| match space.names.get(&name) {
        Some(Right::Receive(_)) => space.names.remove(&name),
        Some(_) => None,
        None => None,
    });
    let Some(Right::Receive(port)) = taken else {
        return Err(MACH_SEND_INVALID_RIGHT);
    };
    match Arc::try_unwrap(port) {
        Ok(port) => {
            // The send rights the name also held stay behind as a plain send
            // right to the port that has just left.
            let address = port.address();
            with(|space| {
                if let Some(refs) = space.sends.remove(&name) {
                    let claim = super::wire::claim(address);
                    space.names.insert(name, Right::Send { address, refs, claim });
                }
            });
            Ok(port)
        }
        Err(shared) => {
            // Put it back: the caller still holds the right it asked to move.
            with(|space| space.names.insert(name, Right::Receive(shared)));
            Err(MACH_SEND_INVALID_RIGHT)
        }
    }
}

/// Add a send right for `address`, reusing an existing name for the same port.
///
/// Reuse is what Mach does and what callers depend on: two messages carrying a
/// send right to the same port arrive naming the same integer, and a client that
/// compares them - a crash reporter deciding whether it already knows this
/// sender - would otherwise see two ports where there is one.
pub fn insert_send(address: Address) -> Name {
    insert_send_with(address, None)
}

/// [`insert_send`] for a right that arrived with its claim, which is the one
/// that was made when it was sent and so kept the port's count up while the
/// message was in flight.
pub fn insert_send_with(address: Address, carried: Option<OwnedFd>) -> Name {
    with(|space| {
        for (name, right) in space.names.iter_mut() {
            if let Right::Send {
                address: existing,
                refs,
                claim,
            } = right
                && *existing == address
            {
                *refs = refs.saturating_add(1);
                if claim.is_none() {
                    *claim = carried;
                }
                return *name;
            }
        }
        let claim = carried.or_else(|| super::wire::claim(address));
        let name = space.next;
        space.next = space.next.wrapping_add(4);
        space.names.insert(
            name,
            Right::Send {
                address,
                refs: 1,
                claim,
            },
        );
        name
    })
}

/// A send right to the task `pid`, reusing the name already held for it.
///
/// One name per task for the same reason as [`insert_send`]: `top` compares
/// what two calls handed back, and a task that answered to two names would be
/// two tasks. This task is always [`TASK_SELF`].
pub fn insert_task(pid: i32) -> Name {
    if pid == own_pid() {
        return TASK_SELF;
    }
    with(|space| {
        for (name, right) in space.names.iter_mut() {
            if let Right::Task { pid: held, refs } = right
                && *held == pid
            {
                *refs = refs.saturating_add(1);
                return *name;
            }
        }
        let name = space.next;
        space.next = space.next.wrapping_add(4);
        space.names.insert(name, Right::Task { pid, refs: 1 });
        name
    })
}

/// A send right to thread `tid` of task `pid`, reusing a name already held.
pub fn insert_thread(pid: i32, tid: i32) -> Name {
    thread_name(pid, tid, true)
}

/// A borrowed thread name, as `pthread_mach_thread_np` returns.
pub fn borrow_thread(pid: i32, tid: i32) -> Name {
    thread_name(pid, tid, false)
}

fn thread_name(pid: i32, tid: i32, add_reference: bool) -> Name {
    with(|space| {
        for (name, right) in space.names.iter_mut() {
            if let Right::Thread {
                pid: held_pid,
                tid: held_tid,
                refs,
            } = right
                && (*held_pid, *held_tid) == (pid, tid)
            {
                if add_reference {
                    *refs = refs.saturating_add(1);
                }
                return *name;
            }
        }
        let name = space.next;
        space.next = space.next.wrapping_add(4);
        space
            .names
            .insert(name, Right::Thread { pid, tid, refs: 1 });
        name
    })
}

fn own_pid() -> i32 {
    starnix::process::me()
}

/// The pid of the task `name` is a right to.
pub fn task_pid(name: Name) -> Option<i32> {
    if name == TASK_SELF {
        return Some(own_pid());
    }
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::Task { pid, .. }) => Some(*pid),
        _ => None,
    }
}

/// The task and thread `name` is a right to.
///
/// Other hosts use a kernel thread id as the borrowed pthread port name.
pub fn thread_of(name: Name) -> Option<(i32, i32)> {
    {
        let guard = space().read().unwrap_or_else(|e| e.into_inner());
        match guard.names.get(&name) {
            Some(Right::Thread { pid, tid, .. }) => return Some((*pid, *tid)),
            Some(_) => return None,
            None => {}
        }
    }
    let pid = own_pid();
    let tid = name as i32;
    starnix::process::has_thread(tid).then_some((pid, tid))
}

/// How many names the space holds, for `mach_port_space_basic_info`.
pub fn count() -> usize {
    space()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .names
        .len()
}

/// Add a send-once right, which is never shared with an existing name.
pub fn insert_send_once(address: Address) -> Name {
    insert(Right::SendOnce { address })
}

/// Add a receive right this process has just acquired.
pub fn insert_receive(port: Receiver) -> Name {
    insert(Right::Receive(Arc::new(port)))
}

/// Drop one user reference to `name`, removing it when the last one goes.
///
/// This is `mach_port_deallocate`'s whole behaviour and the tail of several
/// others, so it is spelled once.
fn release(space: &mut Space, name: Name) -> i32 {
    let Some(right) = space.names.get_mut(&name) else {
        return KERN_INVALID_NAME;
    };
    match right {
        Right::Send { refs, .. }
        | Right::Dead { refs }
        | Right::Task { refs, .. }
        | Right::Thread { refs, .. }
        | Right::Memory { refs, .. }
        | Right::File { refs, .. } => {
            *refs -= 1;
            if *refs == 0 {
                space.names.remove(&name);
            }
            KERN_SUCCESS
        }
        Right::SendOnce { .. } => {
            space.names.remove(&name);
            KERN_SUCCESS
        }
        // `mach_port_deallocate` on a receive right is an error on a real
        // system, and the mistake it catches - deallocating the port you are
        // about to listen on - is one worth still catching here.
        Right::Receive(_) => match space.sends.get_mut(&name) {
            Some(refs) if *refs > 0 => {
                *refs -= 1;
                if *refs == 0 {
                    space.sends.remove(&name);
                }
                KERN_SUCCESS
            }
            _ => KERN_INVALID_RIGHT,
        },
        Right::Set(_) => KERN_INVALID_RIGHT,
        Right::Semaphore(_) | Right::Special => KERN_SUCCESS,
    }
}

/// Remove `name` and everything behind it, whatever kind of right it is.
pub fn destroy(name: Name) -> i32 {
    with(|space| match (space.sends.remove(&name), space.names.remove(&name)).1 {
        Some(Right::Set(members)) => {
            let _ = members;
            KERN_SUCCESS
        }
        Some(_) => KERN_SUCCESS,
        None => KERN_INVALID_NAME,
    })
}

/// `kern_return_t mach_port_allocate(ipc_space_t, mach_port_right_t, mach_port_name_t *)`
///
/// # Safety
///
/// `out` is a writable `mach_port_name_t`.
pub unsafe extern "C" fn mach_port_allocate(task: Name, right: u32, out: *mut Name) -> i32 {
    if out.is_null() {
        return KERN_INVALID_ADDRESS;
    }
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let name = match right {
        RIGHT_RECEIVE => match Receiver::bind() {
            Ok(port) => insert_receive(port),
            Err(_) => return KERN_RESOURCE_SHORTAGE,
        },
        RIGHT_PORT_SET => insert(Right::Set(Vec::new())),
        // A dead name with no port behind it; Mach allows this and nothing this
        // runtime has met asks for it, but the alternative is answering an
        // unknown right with a receive right, which is a live port a caller
        // would then listen on.
        RIGHT_DEAD_NAME => insert(Right::Dead { refs: 1 }),
        _ => return KERN_INVALID_VALUE,
    };
    // SAFETY: the caller's `mach_port_name_t`.
    unsafe { out.write(name) };
    KERN_SUCCESS
}

/// `mach_port_options_t`.
#[repr(C)]
pub struct PortOptions {
    flags: u32,
    queue_limit: u32,
    reserved: [u64; 2],
}

/// `MPO_INSERT_SEND_RIGHT`.
const MPO_INSERT_SEND_RIGHT: u32 = 0x10;

/// `kern_return_t mach_port_construct(ipc_space_t, mach_port_options_ptr_t,
/// mach_port_context_t, mach_port_name_t *)`: a receive right, with the
/// options this runtime has a meaning for.
///
/// A send right asked for with `MPO_INSERT_SEND_RIGHT` goes under the receive
/// right's own name, and stays there when the receive right is sent away. The
/// queue limit, the guard and the importance flags change nothing a single
/// process can observe.
///
/// # Safety
///
/// `options` is null or a `mach_port_options_t`; `out` is writable.
pub unsafe extern "C" fn mach_port_construct(
    task: Name,
    options: *const PortOptions,
    _context: u64,
    out: *mut Name,
) -> i32 {
    // SAFETY: null or the caller's options.
    let send_too = !options.is_null() && unsafe { (*options).flags } & MPO_INSERT_SEND_RIGHT != 0;
    // SAFETY: the caller's `out`.
    let made = unsafe { mach_port_allocate(task, RIGHT_RECEIVE, out) };
    if made == KERN_SUCCESS && send_too {
        // SAFETY: just written by the allocation.
        let name = unsafe { out.read() };
        with(|space| *space.sends.entry(name).or_default() += 1);
    }
    made
}

/// `kern_return_t mach_port_destruct(ipc_space_t, mach_port_name_t,
/// mach_port_delta_t srdelta, mach_port_context_t guard)`: the receive right
/// goes, and with it the send rights its name carried.
pub extern "C" fn mach_port_destruct(task: Name, name: Name, _srdelta: i32, _guard: u64) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    with(|space| match space.names.get(&name) {
        Some(Right::Receive(_)) => {
            space.names.remove(&name);
            space.sends.remove(&name);
            KERN_SUCCESS
        }
        Some(_) => KERN_INVALID_RIGHT,
        None => KERN_INVALID_NAME,
    })
}

/// The open file a fileport stands for, if `name` is one.
pub fn file(name: Name) -> Option<Arc<OwnedFd>> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::File { file, .. }) => Some(file.clone()),
        _ => None,
    }
}

/// The memory object `name` is a send right to, if it is one.
pub fn memory(name: Name) -> Option<(Arc<OwnedFd>, u64, u64)> {
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.get(&name) {
        Some(Right::Memory { file, offset, size, .. }) => Some((file.clone(), *offset, *size)),
        _ => None,
    }
}

/// `kern_return_t mach_port_deallocate(ipc_space_t, mach_port_name_t)`
pub extern "C" fn mach_port_deallocate(task: Name, name: Name) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if name == MACH_PORT_NULL || name == MACH_PORT_DEAD {
        return KERN_SUCCESS;
    }
    with(|space| release(space, name))
}

/// `kern_return_t mach_port_destroy(ipc_space_t, mach_port_name_t)`
pub extern "C" fn mach_port_destroy(task: Name, name: Name) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    destroy(name)
}

/// `kern_return_t mach_port_mod_refs(ipc_space_t, mach_port_name_t,
/// mach_port_right_t, mach_port_delta_t)`
pub extern "C" fn mach_port_mod_refs(task: Name, name: Name, right: u32, delta: i32) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    with(|space| {
        let Some(held) = space.names.get_mut(&name) else {
            return KERN_INVALID_NAME;
        };
        if held.type_bits() != type_of(right) {
            return KERN_INVALID_RIGHT;
        }
        match held {
            Right::Send { refs, .. }
            | Right::Dead { refs }
            | Right::Task { refs, .. }
            | Right::Thread { refs, .. }
            | Right::Memory { refs, .. }
            | Right::File { refs, .. } => {
                let updated = *refs as i64 + delta as i64;
                if updated < 0 {
                    return KERN_INVALID_VALUE;
                }
                if updated > u32::MAX as i64 {
                    return KERN_UREFS_OVERFLOW;
                }
                *refs = updated as u32;
                if updated == 0 {
                    space.names.remove(&name);
                }
                KERN_SUCCESS
            }
            // A receive right and a port set have exactly one reference, so the
            // only legal delta is the one that destroys them.
            _ => match delta {
                0 => KERN_SUCCESS,
                -1 => {
                    space.names.remove(&name);
                    KERN_SUCCESS
                }
                _ => KERN_INVALID_VALUE,
            },
        }
    })
}

/// `kern_return_t mach_port_get_refs(ipc_space_t, mach_port_name_t,
/// mach_port_right_t, mach_port_urefs_t *)`
///
/// # Safety
///
/// `out` is a writable `mach_port_urefs_t`.
pub unsafe extern "C" fn mach_port_get_refs(
    task: Name,
    name: Name,
    right: u32,
    out: *mut u32,
) -> i32 {
    if out.is_null() {
        return KERN_INVALID_ADDRESS;
    }
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    let Some(held) = guard.names.get(&name) else {
        return KERN_INVALID_NAME;
    };
    let refs = match (held, held.type_bits() == type_of(right)) {
        (_, false) => 0,
        (Right::Send { refs, .. } | Right::Dead { refs }, true) => *refs,
        (_, true) => 1,
    };
    // SAFETY: the caller's `mach_port_urefs_t`.
    unsafe { out.write(refs) };
    KERN_SUCCESS
}

/// `kern_return_t mach_port_insert_right(ipc_space_t, mach_port_name_t,
/// mach_port_poly_t, mach_msg_type_name_t)`
///
/// The name is the caller's choice, not this space's, which is the one place a
/// caller writes into the namespace rather than reading a name out of it.
pub extern "C" fn mach_port_insert_right(
    task: Name,
    name: Name,
    right: Name,
    disposition: u32,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let Some(address) = address_of(right) else {
        return KERN_INVALID_CAPABILITY;
    };
    with(|space| {
        if space.names.contains_key(&name) && name != right {
            return KERN_RIGHT_EXISTS;
        }
        // A send right made under the receive right's own name: the name
        // already sends to the port, and replacing the entry would lose the
        // receive right the caller is about to listen on.
        if name == right
            && matches!(space.names.get(&name), Some(Right::Receive(_)))
            && disposition == super::msg::MAKE_SEND
        {
            *space.sends.entry(name).or_default() += 1;
            return KERN_SUCCESS;
        }
        let inserted = match disposition {
            super::msg::MAKE_SEND | super::msg::COPY_SEND | super::msg::MOVE_SEND => Right::Send {
                address,
                refs: 1,
                claim: super::wire::claim(address),
            },
            super::msg::MAKE_SEND_ONCE | super::msg::MOVE_SEND_ONCE => Right::SendOnce { address },
            _ => return KERN_INVALID_VALUE,
        };
        space.names.insert(name, inserted);
        KERN_SUCCESS
    })
}

/// `kern_return_t mach_port_extract_right(ipc_space_t, mach_port_name_t,
/// mach_msg_type_name_t, mach_port_poly_t *, mach_msg_type_name_t *)`
///
/// The call a crash reporter makes to keep a send right to the port whose
/// receive right it is about to hand its handler.
///
/// # Safety
///
/// `out` and `out_type` are writable 32-bit words, or null.
pub unsafe extern "C" fn mach_port_extract_right(
    task: Name,
    name: Name,
    disposition: u32,
    out: *mut Name,
    out_type: *mut u32,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let Some(address) = address_of(name) else {
        return KERN_INVALID_NAME;
    };
    let (extracted, kind) = match disposition {
        super::msg::MAKE_SEND | super::msg::COPY_SEND => {
            (insert_send(address), super::msg::MOVE_SEND)
        }
        super::msg::MAKE_SEND_ONCE => (insert_send_once(address), super::msg::MOVE_SEND_ONCE),
        super::msg::MOVE_SEND => (name, super::msg::MOVE_SEND),
        _ => return KERN_INVALID_VALUE,
    };
    if !out.is_null() {
        // SAFETY: the caller's `mach_port_t`.
        unsafe { out.write(extracted) };
    }
    if !out_type.is_null() {
        // SAFETY: the caller's `mach_msg_type_name_t`.
        unsafe { out_type.write(kind) };
    }
    KERN_SUCCESS
}

/// `kern_return_t mach_port_type(ipc_space_t, mach_port_name_t, mach_port_type_t *)`
///
/// # Safety
///
/// `out` is a writable `mach_port_type_t`.
pub unsafe extern "C" fn mach_port_type(task: Name, name: Name, out: *mut u32) -> i32 {
    if out.is_null() {
        return KERN_INVALID_ADDRESS;
    }
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    let Some(held) = guard.names.get(&name) else {
        return KERN_INVALID_NAME;
    };
    // SAFETY: the caller's `mach_port_type_t`.
    unsafe { out.write(held.type_bits()) };
    KERN_SUCCESS
}

/// `kern_return_t mach_port_insert_member(ipc_space_t, mach_port_name_t,
/// mach_port_name_t pset)`
pub extern "C" fn mach_port_insert_member(task: Name, name: Name, pset: Name) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if receiver(name).is_none() {
        return KERN_INVALID_RIGHT;
    }
    with(|space| match space.names.get_mut(&pset) {
        Some(Right::Set(members)) => {
            if !members.contains(&name) {
                members.push(name);
            }
            KERN_SUCCESS
        }
        Some(_) => KERN_INVALID_RIGHT,
        None => KERN_INVALID_NAME,
    })
}

/// `kern_return_t mach_port_move_member(ipc_space_t, mach_port_name_t,
/// mach_port_name_t after)`
///
/// A member belongs to at most one set, so this is a removal from every set
/// followed by an insertion - and `MACH_PORT_NULL` for `after` is the removal
/// on its own.
pub extern "C" fn mach_port_move_member(task: Name, name: Name, after: Name) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    if receiver(name).is_none() {
        return KERN_INVALID_RIGHT;
    }
    with(|space| {
        for right in space.names.values_mut() {
            if let Right::Set(members) = right {
                members.retain(|member| *member != name);
            }
        }
        if after == MACH_PORT_NULL {
            return KERN_SUCCESS;
        }
        match space.names.get_mut(&after) {
            Some(Right::Set(members)) => {
                members.push(name);
                KERN_SUCCESS
            }
            Some(_) => KERN_INVALID_RIGHT,
            None => KERN_INVALID_NAME,
        }
    })
}

/// `kern_return_t mach_port_get_set_status(...)` and the attribute calls beside
/// it, which set queue limits and other things this transport does not model.
///
/// Accepted rather than refused: every one of them is advisory - a caller that
/// is told its queue limit was raised behaves the same either way - and the
/// alternative is failing an initialisation that had nothing wrong with it.
pub extern "C" fn mach_port_set_attributes(
    task: Name,
    name: Name,
    _flavor: i32,
    _info: *mut std::ffi::c_void,
    _count: u32,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let guard = space().read().unwrap_or_else(|e| e.into_inner());
    match guard.names.contains_key(&name) {
        true => KERN_SUCCESS,
        false => KERN_INVALID_NAME,
    }
}

/// `MACH_NOTIFY_NO_SENDERS`, from `<mach/notify.h>`: `MACH_NOTIFY_FIRST + 6`.
pub const NOTIFY_NO_SENDERS: i32 = 0o106;

/// `kern_return_t mach_port_request_notification(ipc_space_t, mach_port_name_t,
/// mach_msg_id_t, mach_port_mscount_t, mach_port_name_t notify,
/// mach_msg_type_name_t, mach_port_name_t *previous)`
///
/// `MACH_NOTIFY_NO_SENDERS` is delivered - see [`super::senders`] - because a
/// process that is the only thing keeping a helper alive is what the helper
/// watches for: a crash handler that is never told its client has exited stays
/// behind forever. Every other notification is recorded and never delivered.
/// Inventing one would be worse than withholding it: a dead-name notification
/// tells a client its server has died, so a spurious one makes a working server
/// look dead, where the withheld direction says "still alive", which is the
/// failure a caller can survive.
///
/// # Safety
///
/// `previous` is a writable `mach_port_name_t`, or null.
pub unsafe extern "C" fn mach_port_request_notification(
    task: Name,
    name: Name,
    id: i32,
    sync: u32,
    notify: Name,
    disposition: u32,
    previous: *mut Name,
) -> i32 {
    if task != TASK_SELF {
        return KERN_INVALID_TASK;
    }
    let held = with_right(name, |right| matches!(right, Right::Receive(_)));
    let Some(is_receive) = held else {
        return KERN_INVALID_NAME;
    };
    if id == NOTIFY_NO_SENDERS && !is_receive {
        return KERN_INVALID_RIGHT;
    }
    let port = receiver(name);
    // Where the notification goes. A `MAKE_` disposition names the receive right
    // it is made from and leaves it alone; a `MOVE_` one hands over a send-once
    // right, which the request consumes whether or not it ever fires.
    let destination = (notify != MACH_PORT_NULL)
        .then(|| address_of(notify))
        .flatten();
    if notify != MACH_PORT_NULL
        && matches!(
            disposition,
            super::msg::MOVE_SEND_ONCE | super::msg::MOVE_SEND
        )
    {
        with(|space| release(space, notify));
    }
    if !previous.is_null() {
        // SAFETY: the caller's `mach_port_name_t`.
        unsafe { previous.write(MACH_PORT_NULL) };
    }
    if id == NOTIFY_NO_SENDERS
        && let Some(port) = port
        && super::senders::request(port.address(), destination, sync)
    {
        return KERN_SUCCESS;
    }
    announce_undelivered_notifications(id);
    KERN_SUCCESS
}

fn announce_undelivered_notifications(id: i32) {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "rosemary: mach_port_request_notification {id:#o} accepted but never delivered - \
             nothing here watches this kind of port event, so a caller waiting for it waits \
             forever rather than being told a live port died"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mistake this constant exists to stop: `MACH_PORT_RIGHT_SEND` is
    /// zero and `MACH_PORT_RIGHT_RECEIVE` is one, so a table indexed the
    /// intuitive way round hands out a send right to a caller that asked to
    /// receive.
    #[test]
    fn the_right_numbers_are_the_headers_and_not_the_intuitive_order() {
        assert_eq!(RIGHT_SEND, 0);
        assert_eq!(RIGHT_RECEIVE, 1);
        assert_eq!(type_of(RIGHT_SEND), 0x0001_0000);
        assert_eq!(type_of(RIGHT_RECEIVE), 0x0002_0000);
        assert_eq!(type_of(RIGHT_DEAD_NAME), 0x0010_0000);
    }

    #[test]
    fn allocating_a_receive_right_gives_a_name_that_reports_itself_as_one() {
        let mut name = MACH_PORT_NULL;
        // SAFETY: a live local for the out-parameter.
        assert_eq!(
            unsafe { mach_port_allocate(TASK_SELF, RIGHT_RECEIVE, &mut name) },
            KERN_SUCCESS
        );
        assert_ne!(name, MACH_PORT_NULL);

        let mut bits = 0;
        assert_eq!(
            unsafe { mach_port_type(TASK_SELF, name, &mut bits) },
            KERN_SUCCESS
        );
        assert_eq!(bits, type_of(RIGHT_RECEIVE));

        // A receive right cannot be deallocated, only destroyed - the check
        // that catches "deallocate the port you are about to listen on".
        assert_eq!(mach_port_deallocate(TASK_SELF, name), KERN_INVALID_RIGHT);
        assert_eq!(mach_port_destroy(TASK_SELF, name), KERN_SUCCESS);
        assert_eq!(mach_port_destroy(TASK_SELF, name), KERN_INVALID_NAME);
    }

    /// Crashpad's sequence: allocate a receive right, extract a send right to
    /// keep, hand the receive right away.
    #[test]
    fn a_send_right_extracted_from_a_receive_right_names_the_same_port() {
        let mut receive = MACH_PORT_NULL;
        // SAFETY: a live local for the out-parameter.
        unsafe { mach_port_allocate(TASK_SELF, RIGHT_RECEIVE, &mut receive) };

        let (mut send, mut kind) = (MACH_PORT_NULL, 0);
        // SAFETY: two live locals for the out-parameters.
        let rc = unsafe {
            mach_port_extract_right(
                TASK_SELF,
                receive,
                super::super::msg::MAKE_SEND,
                &mut send,
                &mut kind,
            )
        };
        assert_eq!(rc, KERN_SUCCESS);
        assert_eq!(kind, super::super::msg::MOVE_SEND);
        assert_ne!(send, receive);
        assert_eq!(address_of(send), address_of(receive));
        assert_eq!(destination(send).unwrap().1, false, "not send-once");

        mach_port_destroy(TASK_SELF, receive);
        mach_port_deallocate(TASK_SELF, send);
    }

    #[test]
    fn a_send_right_is_reference_counted_and_the_name_survives_until_the_last_one() {
        let mut receive = MACH_PORT_NULL;
        // SAFETY: a live local for the out-parameter.
        unsafe { mach_port_allocate(TASK_SELF, RIGHT_RECEIVE, &mut receive) };
        let address = address_of(receive).unwrap();

        let first = insert_send(address);
        let second = insert_send(address);
        assert_eq!(first, second, "the same port keeps the same name");

        assert_eq!(mach_port_deallocate(TASK_SELF, first), KERN_SUCCESS);
        assert!(address_of(first).is_some(), "one reference is still held");
        assert_eq!(mach_port_deallocate(TASK_SELF, first), KERN_SUCCESS);
        assert!(address_of(first).is_none());
        mach_port_destroy(TASK_SELF, receive);
    }

    #[test]
    fn a_port_set_holds_the_members_put_into_it() {
        let mut set = MACH_PORT_NULL;
        let mut member = MACH_PORT_NULL;
        // SAFETY: live locals for the out-parameters.
        unsafe {
            mach_port_allocate(TASK_SELF, RIGHT_PORT_SET, &mut set);
            mach_port_allocate(TASK_SELF, RIGHT_RECEIVE, &mut member);
        }
        assert_eq!(
            mach_port_insert_member(TASK_SELF, member, set),
            KERN_SUCCESS
        );
        assert_eq!(members(set).unwrap(), vec![member]);
        // A set is not a receive right, and inserting one into itself is not a
        // question with an answer.
        assert_eq!(
            mach_port_insert_member(TASK_SELF, set, set),
            KERN_INVALID_RIGHT
        );
        assert_eq!(
            mach_port_move_member(TASK_SELF, member, MACH_PORT_NULL),
            KERN_SUCCESS
        );
        assert!(members(set).unwrap().is_empty());

        mach_port_destroy(TASK_SELF, set);
        mach_port_destroy(TASK_SELF, member);
    }

    /// A receive right somebody else still holds cannot be moved out from under
    /// them - the check that stops this process reading a socket another
    /// process now owns.
    #[test]
    fn a_receive_right_in_use_is_not_moved() {
        let mut name = MACH_PORT_NULL;
        // SAFETY: a live local for the out-parameter.
        unsafe { mach_port_allocate(TASK_SELF, RIGHT_RECEIVE, &mut name) };
        let held = receiver(name).expect("a receive right");
        assert_eq!(move_receive(name).err(), Some(MACH_SEND_INVALID_RIGHT));
        assert!(receiver(name).is_some(), "the right was put back");
        drop(held);
        assert!(move_receive(name).is_ok());
    }

    /// Chromium makes a channel's port with its send right under one name,
    /// sends the receive right to the child and keeps sending on that name.
    #[test]
    fn a_constructed_send_right_outlives_its_receive_right_moving() {
        let options = PortOptions {
            flags: MPO_INSERT_SEND_RIGHT,
            queue_limit: 0,
            reserved: [0; 2],
        };
        let mut name = MACH_PORT_NULL;
        // SAFETY: live locals for the options and the out-parameter.
        let rc = unsafe { mach_port_construct(TASK_SELF, &options, 0, &mut name) };
        assert_eq!(rc, KERN_SUCCESS);
        let address = address_of(name).expect("an address");
        let moved = move_receive(name).expect("the receive right moves");
        assert_eq!(moved.address(), address);
        assert_eq!(address_of(name), Some(address), "the name still sends to it");
        assert_eq!(mach_port_deallocate(TASK_SELF, name), KERN_SUCCESS);
        assert_eq!(address_of(name), None, "and that was its last reference");
    }
}
