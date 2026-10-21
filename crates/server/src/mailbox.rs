//! Work the server holds for each guest until the guest answers it.
//!
//! One mailbox per pid, for one image of it. Work waits in `queued` until the
//! guest's handler fetches it, then in `in_flight` until it is completed. A
//! requester waits on its own [`Pending`], never on a lock, so a guest that
//! does not answer holds up nobody but the one asking it.
//!
//! Nothing is dropped while the guest lives. Its work fails with
//! [`TaskOutcome::Gone`] when it exits, or when a fresh check-in replaces the
//! image the work was for.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::task::{self, Completion, TaskOp, TaskOutcome, Work};

/// One piece of work and the slot its answer lands in.
pub struct Pending {
    work: Work,
    outcome: Mutex<Option<TaskOutcome>>,
    ready: Condvar,
}

impl Pending {
    fn finish(&self, outcome: TaskOutcome) {
        let mut slot = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(outcome);
            self.ready.notify_all();
        }
    }

    /// Block until the work is answered, or its task is gone.
    pub fn wait(&self) -> TaskOutcome {
        let mut slot = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(outcome) = slot.take() {
                return outcome;
            }
            slot = self.ready.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct Mailbox {
    image: u64,
    serviceable: bool,
    /// The connection the guest fetches on, while one is open.
    service: Option<u64>,
    queued: VecDeque<Arc<Pending>>,
    in_flight: HashMap<u64, Arc<Pending>>,
}

impl Mailbox {
    fn fail(&mut self) {
        for pending in self
            .queued
            .drain(..)
            .chain(self.in_flight.drain().map(|(_, p)| p))
        {
            pending.finish(TaskOutcome::Gone);
        }
    }
}

/// What [`Mailboxes::submit`] needs to happen next.
pub enum Submitted {
    /// Queued: wait on these, after signalling the guest if `signal`.
    Queued {
        pending: Vec<Arc<Pending>>,
        signal: bool,
    },
    /// Not queued, and this is the answer.
    Refused(TaskOutcome),
}

#[derive(Default)]
pub struct Mailboxes {
    boxes: Mutex<HashMap<i32, Mailbox>>,
    next_id: AtomicU64,
}

impl Mailboxes {
    pub fn new() -> Self {
        Self::default()
    }

    fn boxes(&self) -> MutexGuard<'_, HashMap<i32, Mailbox>> {
        self.boxes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `pid` checked in as `image`. A different image than its mailbox's is an
    /// `exec`, or a new process under a reused pid, and the old one's work is
    /// for a task that no longer exists.
    pub fn open(&self, pid: i32, image: u64, serviceable: bool) {
        let mut boxes = self.boxes();
        match boxes.get_mut(&pid) {
            Some(mailbox) if mailbox.image == image => mailbox.serviceable = serviceable,
            _ => {
                if let Some(mut old) = boxes.remove(&pid) {
                    old.fail();
                }
                boxes.insert(
                    pid,
                    Mailbox {
                        image,
                        serviceable,
                        service: None,
                        queued: VecDeque::new(),
                        in_flight: HashMap::new(),
                    },
                );
            }
        }
    }

    /// `connection` is now how `pid`, as `image`, fetches its work. True when
    /// work is already waiting, and the guest should be signalled for it.
    pub fn serve(&self, pid: i32, image: u64, connection: u64) -> bool {
        let mut boxes = self.boxes();
        let Some(mailbox) = boxes.get_mut(&pid) else {
            return false;
        };
        if mailbox.image != image {
            return false;
        }
        mailbox.service = Some(connection);
        !mailbox.queued.is_empty()
    }

    /// Queue `ops` for `pid` as of `image`, in order.
    pub fn submit(&self, pid: i32, image: u64, ops: Vec<TaskOp>) -> Submitted {
        let mut boxes = self.boxes();
        let Some(mailbox) = boxes.get_mut(&pid) else {
            return Submitted::Refused(TaskOutcome::Gone);
        };
        if mailbox.image != image {
            return Submitted::Refused(TaskOutcome::Gone);
        }
        if !mailbox.serviceable {
            return Submitted::Refused(TaskOutcome::Unreachable);
        }
        let pending: Vec<Arc<Pending>> = ops
            .into_iter()
            .map(|op| {
                Arc::new(Pending {
                    work: Work {
                        id: self.next_id.fetch_add(1, Ordering::Relaxed),
                        op,
                    },
                    outcome: Mutex::new(None),
                    ready: Condvar::new(),
                })
            })
            .collect();
        mailbox.queued.extend(pending.iter().cloned());
        Submitted::Queued {
            pending,
            signal: mailbox.service.is_some(),
        }
    }

    /// Hand over the work queued for the guest fetching on `connection`: at
    /// least one piece if any is queued, and no more than [`task::BATCH`]
    /// bytes beyond it.
    pub fn fetch(&self, pid: i32, connection: u64) -> Vec<Work> {
        let mut boxes = self.boxes();
        let Some(mailbox) = boxes.get_mut(&pid) else {
            return Vec::new();
        };
        if mailbox.service != Some(connection) {
            return Vec::new();
        }
        let mut works = Vec::new();
        let mut bytes = 0;
        while let Some(next) = mailbox.queued.front() {
            let size = task::encoded_len(&next.work);
            if !works.is_empty() && bytes + size > task::BATCH {
                break;
            }
            bytes += size;
            let next = mailbox.queued.pop_front().unwrap();
            works.push(next.work.clone());
            mailbox.in_flight.insert(next.work.id, next);
        }
        works
    }

    /// Answers from the guest fetching on `connection`. One that answers work
    /// it was not handed - another guest's, or none at all - changes nothing.
    pub fn complete(&self, pid: i32, connection: u64, completions: Vec<Completion>) {
        let mut boxes = self.boxes();
        let Some(mailbox) = boxes.get_mut(&pid) else {
            return;
        };
        if mailbox.service != Some(connection) {
            return;
        }
        for completion in completions {
            if let Some(pending) = mailbox.in_flight.remove(&completion.id) {
                pending.finish(completion.outcome);
            }
        }
    }

    /// The service connection `connection` closed. If its guest is still
    /// `alive` - the connection broke, and the guest will open another - the
    /// work it had fetched goes back in the queue; otherwise everything fails.
    pub fn disconnected(&self, connection: u64, alive: impl Fn(i32, u64) -> bool) {
        let mut boxes = self.boxes();
        let mut dead = Vec::new();
        for (pid, mailbox) in boxes.iter_mut() {
            if mailbox.service != Some(connection) {
                continue;
            }
            mailbox.service = None;
            if alive(*pid, mailbox.image) {
                let mut returned: Vec<Arc<Pending>> =
                    mailbox.in_flight.drain().map(|(_, p)| p).collect();
                returned.sort_by_key(|pending| pending.work.id);
                for pending in returned.into_iter().rev() {
                    mailbox.queued.push_front(pending);
                }
            } else {
                dead.push(*pid);
            }
        }
        for pid in dead {
            if let Some(mut mailbox) = boxes.remove(&pid) {
                mailbox.fail();
            }
        }
    }

    /// Fail and drop every mailbox whose guest is no longer `alive` at its
    /// image: one that exited, or became a host program, without the server
    /// hearing of it on a connection.
    pub fn sweep(&self, alive: impl Fn(i32, u64) -> bool) {
        let mut boxes = self.boxes();
        let dead: Vec<i32> = boxes
            .iter()
            .filter(|(pid, mailbox)| !alive(**pid, mailbox.image))
            .map(|(pid, _)| *pid)
            .collect();
        for pid in dead {
            if let Some(mut mailbox) = boxes.remove(&pid) {
                mailbox.fail();
            }
        }
    }

    /// Whether anything waits on a mailbox, for a sweep to bother.
    pub fn is_empty(&self) -> bool {
        self.boxes()
            .values()
            .all(|mailbox| mailbox.queued.is_empty() && mailbox.in_flight.is_empty())
    }
}

/// Gather the answers to the pieces of one split operation, in order: the
/// pieces of a read concatenated, and the first piece that did not succeed
/// otherwise.
pub fn gather(pending: &[Arc<Pending>]) -> TaskOutcome {
    let mut data: Option<Vec<u8>> = None;
    let mut last = TaskOutcome::Done;
    for piece in pending {
        match piece.wait() {
            TaskOutcome::Data(bytes) => data.get_or_insert_with(Vec::new).extend(bytes),
            outcome @ (TaskOutcome::Done | TaskOutcome::Address(_)) => last = outcome,
            failed => return failed,
        }
    }
    match data {
        Some(bytes) => TaskOutcome::Data(bytes),
        None => last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read() -> TaskOp {
        TaskOp::Read {
            address: 0x1000,
            size: 4,
        }
    }

    fn queued(submitted: Submitted) -> Vec<Arc<Pending>> {
        match submitted {
            Submitted::Queued { pending, .. } => pending,
            Submitted::Refused(outcome) => panic!("refused with {outcome:?}"),
        }
    }

    #[test]
    fn work_is_fetched_once_and_answered_by_its_own_guest_only() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, true);
        boxes.open(20, 1, true);
        boxes.serve(10, 1, 100);
        boxes.serve(20, 1, 200);
        let pending = queued(boxes.submit(10, 1, vec![read()]));
        let works = boxes.fetch(10, 100);
        assert_eq!(works.len(), 1);
        assert!(boxes.fetch(10, 100).is_empty());

        // Another guest answering it is ignored.
        let forged = Completion {
            id: works[0].id,
            outcome: TaskOutcome::Data(b"evil".to_vec()),
        };
        boxes.complete(20, 200, vec![forged]);
        boxes.complete(
            10,
            100,
            vec![Completion {
                id: works[0].id,
                outcome: TaskOutcome::Data(b"good".to_vec()),
            }],
        );
        assert_eq!(gather(&pending), TaskOutcome::Data(b"good".to_vec()));
    }

    #[test]
    fn work_waits_for_a_service_connection_that_is_not_open_yet() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, true);
        match boxes.submit(10, 1, vec![read()]) {
            Submitted::Queued { signal, .. } => assert!(!signal),
            Submitted::Refused(outcome) => panic!("{outcome:?}"),
        }
        assert!(boxes.serve(10, 1, 100), "serving says there is work");
        assert_eq!(boxes.fetch(10, 100).len(), 1);
    }

    #[test]
    fn an_exec_fails_the_old_images_work_and_refuses_more() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, true);
        let pending = queued(boxes.submit(10, 1, vec![read()]));
        boxes.open(10, 2, true);
        assert_eq!(gather(&pending), TaskOutcome::Gone);
        assert!(matches!(
            boxes.submit(10, 1, vec![read()]),
            Submitted::Refused(TaskOutcome::Gone)
        ));
    }

    #[test]
    fn a_broken_connection_requeues_while_the_guest_lives_and_fails_once_it_dies() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, true);
        boxes.serve(10, 1, 100);
        let pending = queued(boxes.submit(10, 1, vec![read(), read()]));
        assert_eq!(boxes.fetch(10, 100).len(), 2);
        boxes.disconnected(100, |_, _| true);
        boxes.serve(10, 1, 101);
        let again = boxes.fetch(10, 101);
        assert_eq!(again.len(), 2);
        assert!(again[0].id < again[1].id, "order is kept");
        boxes.disconnected(101, |_, _| false);
        assert_eq!(gather(&pending), TaskOutcome::Gone);
    }

    #[test]
    fn a_sweep_fails_work_for_a_guest_that_died_quietly() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, true);
        let pending = queued(boxes.submit(10, 1, vec![read()]));
        boxes.sweep(|_, _| false);
        assert_eq!(gather(&pending), TaskOutcome::Gone);
    }

    #[test]
    fn an_unreachable_guest_is_refused_at_once() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, false);
        assert!(matches!(
            boxes.submit(10, 1, vec![read()]),
            Submitted::Refused(TaskOutcome::Unreachable)
        ));
    }

    #[test]
    fn the_first_failing_piece_is_the_answer() {
        let boxes = Mailboxes::new();
        boxes.open(10, 1, true);
        boxes.serve(10, 1, 100);
        let pending = queued(boxes.submit(10, 1, vec![read(), read()]));
        let works = boxes.fetch(10, 100);
        boxes.complete(
            10,
            100,
            vec![
                Completion {
                    id: works[0].id,
                    outcome: TaskOutcome::Data(vec![1]),
                },
                Completion {
                    id: works[1].id,
                    outcome: TaskOutcome::Failed(1),
                },
            ],
        );
        assert_eq!(gather(&pending), TaskOutcome::Failed(1));
    }
}
