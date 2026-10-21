//! What one task asks the server to have done to another: the work the server
//! queues for a guest and the guest answers about itself.
//!
//! A guest cannot reach into another's memory (a host that guards processes
//! from each other, Yama's, refuses even the same user), so the target does it:
//! the server queues [`Work`] for it, pokes it with a private signal, and the
//! target's handler fetches the queue and answers each item with a
//! [`Completion`].
//!
//! The handler may not allocate, so beside the [`Wire`] encodings used by the
//! server and by requesters, this module decodes work from a borrowed buffer
//! ([`WorkView`]) and encodes completions into one ([`Sink`]), byte for byte
//! the same as their `Wire` forms.

use crate::codec::{Decoder, Encoder, Malformed, Wire};

/// The most data one piece of work carries either way. A larger read or write
/// is split by the server, so the handler's buffers have a fixed size.
pub const CHUNK: usize = 64 << 10;

/// How many bytes of work one fetch hands over at most, beyond the first
/// piece, which is always handed over.
pub const BATCH: usize = 192 << 10;

/// The private signal index ([`starnix::signal::private_signal`]) that tells a
/// guest the server has work for it. 0 and 1 are `thread_suspend`'s.
pub const SIGNAL_INDEX: u8 = 2;

/// The signal that tells a guest the server has work for it, or `None` on a
/// host with none to spare.
pub fn signal() -> Option<i32> {
    starnix::signal::private_signal(SIGNAL_INDEX)
}

/// One operation on a task, done by the task itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskOp {
    Read {
        address: u64,
        size: u64,
    },
    Write {
        address: u64,
        data: Vec<u8>,
    },
    Protect {
        address: u64,
        size: u64,
        set_maximum: bool,
        protection: i32,
    },
    Allocate {
        address: u64,
        size: u64,
        flags: i32,
    },
    Deallocate {
        address: u64,
        size: u64,
    },
    Copy {
        source: u64,
        size: u64,
        dest: u64,
    },
    Suspend,
    Resume,
}

const READ: u8 = 1;
const WRITE: u8 = 2;
const PROTECT: u8 = 3;
const ALLOCATE: u8 = 4;
const DEALLOCATE: u8 = 5;
const COPY: u8 = 6;
const SUSPEND: u8 = 7;
const RESUME: u8 = 8;

impl Wire for TaskOp {
    fn put(&self, out: &mut Encoder) {
        match self {
            TaskOp::Read { address, size } => out.put(&READ).put(address).put(size),
            TaskOp::Write { address, data } => out.put(&WRITE).put(address).put(data),
            TaskOp::Protect {
                address,
                size,
                set_maximum,
                protection,
            } => out
                .put(&PROTECT)
                .put(address)
                .put(size)
                .put(set_maximum)
                .put(protection),
            TaskOp::Allocate {
                address,
                size,
                flags,
            } => out.put(&ALLOCATE).put(address).put(size).put(flags),
            TaskOp::Deallocate { address, size } => out.put(&DEALLOCATE).put(address).put(size),
            TaskOp::Copy { source, size, dest } => out.put(&COPY).put(source).put(size).put(dest),
            TaskOp::Suspend => out.put(&SUSPEND),
            TaskOp::Resume => out.put(&RESUME),
        };
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match Op::take(input)? {
            Op::Read { address, size } => TaskOp::Read { address, size },
            Op::Write { address, data } => TaskOp::Write {
                address,
                data: data.to_vec(),
            },
            Op::Protect {
                address,
                size,
                set_maximum,
                protection,
            } => TaskOp::Protect {
                address,
                size,
                set_maximum,
                protection,
            },
            Op::Allocate {
                address,
                size,
                flags,
            } => TaskOp::Allocate {
                address,
                size,
                flags,
            },
            Op::Deallocate { address, size } => TaskOp::Deallocate { address, size },
            Op::Copy { source, size, dest } => TaskOp::Copy { source, size, dest },
            Op::Suspend => TaskOp::Suspend,
            Op::Resume => TaskOp::Resume,
        })
    }
}

/// [`TaskOp`] borrowing its data from the buffer it was decoded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op<'a> {
    Read {
        address: u64,
        size: u64,
    },
    Write {
        address: u64,
        data: &'a [u8],
    },
    Protect {
        address: u64,
        size: u64,
        set_maximum: bool,
        protection: i32,
    },
    Allocate {
        address: u64,
        size: u64,
        flags: i32,
    },
    Deallocate {
        address: u64,
        size: u64,
    },
    Copy {
        source: u64,
        size: u64,
        dest: u64,
    },
    Suspend,
    Resume,
}

impl<'a> Op<'a> {
    fn take(input: &mut Decoder<'a>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            READ => Op::Read {
                address: input.take()?,
                size: input.take()?,
            },
            WRITE => Op::Write {
                address: input.take()?,
                data: input.bytes()?,
            },
            PROTECT => Op::Protect {
                address: input.take()?,
                size: input.take()?,
                set_maximum: input.take()?,
                protection: input.take()?,
            },
            ALLOCATE => Op::Allocate {
                address: input.take()?,
                size: input.take()?,
                flags: input.take()?,
            },
            DEALLOCATE => Op::Deallocate {
                address: input.take()?,
                size: input.take()?,
            },
            COPY => Op::Copy {
                source: input.take()?,
                size: input.take()?,
                dest: input.take()?,
            },
            SUSPEND => Op::Suspend,
            RESUME => Op::Resume,
            _ => return Err(Malformed),
        })
    }
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskOutcome {
    Done,
    /// What a read read.
    Data(Vec<u8>),
    /// Where an allocation landed.
    Address(u64),
    /// The task refused, with its `kern_return_t`.
    Failed(i32),
    /// The task is gone: it exited, or `exec` replaced the image the request
    /// named. Darwin's port for it is dead.
    Gone,
    /// The task cannot be asked: its host has no private signal to reach it.
    Unreachable,
}

const DONE: u8 = 0;
const DATA: u8 = 1;
const ADDRESS: u8 = 2;
const FAILED: u8 = 3;
const GONE: u8 = 4;
const UNREACHABLE: u8 = 5;

impl Wire for TaskOutcome {
    fn put(&self, out: &mut Encoder) {
        match self {
            TaskOutcome::Done => out.put(&DONE),
            TaskOutcome::Data(data) => out.put(&DATA).put(data),
            TaskOutcome::Address(address) => out.put(&ADDRESS).put(address),
            TaskOutcome::Failed(code) => out.put(&FAILED).put(code),
            TaskOutcome::Gone => out.put(&GONE),
            TaskOutcome::Unreachable => out.put(&UNREACHABLE),
        };
    }
    fn take(input: &mut Decoder<'_>) -> Result<Self, Malformed> {
        Ok(match u8::take(input)? {
            DONE => TaskOutcome::Done,
            DATA => TaskOutcome::Data(input.take()?),
            ADDRESS => TaskOutcome::Address(input.take()?),
            FAILED => TaskOutcome::Failed(input.take()?),
            GONE => TaskOutcome::Gone,
            UNREACHABLE => TaskOutcome::Unreachable,
            _ => return Err(Malformed),
        })
    }
}

/// A piece of work queued for a guest, as a fetch hands it over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub id: u64,
    pub op: TaskOp,
}

crate::wire_struct!(Work { id, op });

/// A guest's answer to a [`Work`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub id: u64,
    pub outcome: TaskOutcome,
}

crate::wire_struct!(Completion { id, outcome });

/// One piece of a fetched batch, borrowed from the buffer it arrived in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkView<'a> {
    pub id: u64,
    pub op: Op<'a>,
}

/// The work in a `Reply::Work` payload, without copying it out.
pub struct Batch<'a> {
    input: Decoder<'a>,
    left: u32,
}

impl<'a> Batch<'a> {
    /// `payload` is a whole reply frame; anything but a `Reply::Work` is
    /// malformed here.
    pub fn parse(payload: &'a [u8]) -> Result<Self, Malformed> {
        let mut input = Decoder::new(payload);
        if u8::take(&mut input)? != crate::protocol::REPLY_WORK {
            return Err(Malformed);
        }
        let left = u32::take(&mut input)?;
        Ok(Batch { input, left })
    }

    pub fn is_empty(&self) -> bool {
        self.left == 0
    }
}

impl<'a> Iterator for Batch<'a> {
    type Item = Result<WorkView<'a>, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let mut take = || -> Result<WorkView<'a>, Malformed> {
            Ok(WorkView {
                id: self.input.take()?,
                op: Op::take(&mut self.input)?,
            })
        };
        let item = take();
        if item.is_err() {
            self.left = 0;
        }
        Some(item)
    }
}

/// A frame built in a fixed buffer: the length prefix, then the payload.
///
/// Every `put_*` fails rather than grow, and nothing here allocates.
pub struct Sink<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

/// A frame did not fit its buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

impl<'a> Sink<'a> {
    /// Start a frame in `buffer`, after a space for its length.
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Sink { buffer, len: 4 }
    }

    fn raw(&mut self, bytes: &[u8]) -> Result<(), Full> {
        let end = self.len.checked_add(bytes.len()).ok_or(Full)?;
        self.buffer
            .get_mut(self.len..end)
            .ok_or(Full)?
            .copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    /// Room left, in bytes.
    pub fn room(&self) -> usize {
        self.buffer.len().saturating_sub(self.len)
    }

    /// A `Request::Fetch`.
    pub fn fetch(&mut self) -> Result<(), Full> {
        self.raw(&[crate::protocol::REQUEST_FETCH])
    }

    /// The head of a `Request::Complete` of `count` completions; the
    /// completions follow with [`Sink::completion`].
    pub fn complete(&mut self, count: u32) -> Result<usize, Full> {
        self.raw(&[crate::protocol::REQUEST_COMPLETE])?;
        let at = self.len;
        self.raw(&count.to_le_bytes())?;
        Ok(at)
    }

    /// Rewrite a count written by [`Sink::complete`] at `at`.
    pub fn recount(&mut self, at: usize, count: u32) {
        self.buffer[at..at + 4].copy_from_slice(&count.to_le_bytes());
    }

    /// The head of a completion that returns data, with room for `len` bytes
    /// after it, which the caller fills through [`Sink::reserve`].
    pub fn data(&mut self, id: u64, len: usize) -> Result<&mut [u8], Full> {
        self.raw(&id.to_le_bytes())?;
        self.raw(&[DATA])?;
        self.raw(&(len as u32).to_le_bytes())?;
        self.reserve(len)
    }

    /// `len` bytes of payload for the caller to fill in place.
    pub fn reserve(&mut self, len: usize) -> Result<&mut [u8], Full> {
        let end = self.len.checked_add(len).ok_or(Full)?;
        if end > self.buffer.len() {
            return Err(Full);
        }
        let start = self.len;
        self.len = end;
        Ok(&mut self.buffer[start..end])
    }

    /// A completion without data.
    pub fn completion(&mut self, id: u64, outcome: Outcome) -> Result<(), Full> {
        self.raw(&id.to_le_bytes())?;
        match outcome {
            Outcome::Done => self.raw(&[DONE]),
            Outcome::Address(address) => {
                self.raw(&[ADDRESS])?;
                self.raw(&address.to_le_bytes())
            }
            Outcome::Failed(code) => {
                self.raw(&[FAILED])?;
                self.raw(&code.to_le_bytes())
            }
        }
    }

    /// Where the frame ends, which is the position a caller rewinds to with
    /// [`Sink::truncate`] to take back a completion it could not finish.
    pub fn mark(&self) -> usize {
        self.len
    }

    pub fn truncate(&mut self, mark: usize) {
        self.len = mark.max(4);
    }

    /// The whole frame, its length filled in.
    pub fn finish(&mut self) -> &[u8] {
        let payload = (self.len - 4) as u32;
        self.buffer[..4].copy_from_slice(&payload.to_le_bytes());
        &self.buffer[..self.len]
    }
}

/// A completion's outcome, as the handler produces it: data is written in
/// place through [`Sink::data`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Address(u64),
    Failed(i32),
}

/// How a requester splits `op` so each piece fits [`CHUNK`].
pub fn split(op: TaskOp) -> Vec<TaskOp> {
    match op {
        TaskOp::Read { address, size } if size > CHUNK as u64 => {
            let mut pieces = Vec::new();
            let mut done = 0;
            while done < size {
                let n = (size - done).min(CHUNK as u64);
                pieces.push(TaskOp::Read {
                    address: address.wrapping_add(done),
                    size: n,
                });
                done += n;
            }
            pieces
        }
        TaskOp::Write { address, data } if data.len() > CHUNK => data
            .chunks(CHUNK)
            .enumerate()
            .map(|(i, piece)| TaskOp::Write {
                address: address.wrapping_add((i * CHUNK) as u64),
                data: piece.to_vec(),
            })
            .collect(),
        op => vec![op],
    }
}

/// The encoded size of `work`, for keeping a fetch under [`BATCH`].
pub fn encoded_len(work: &Work) -> usize {
    let mut out = Encoder::new();
    work.put(&mut out);
    out.bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};
    use crate::protocol::{Reply, Request};

    fn every_op() -> Vec<TaskOp> {
        vec![
            TaskOp::Read {
                address: 0x1000,
                size: 16,
            },
            TaskOp::Write {
                address: 0x2000,
                data: b"rosemary".to_vec(),
            },
            TaskOp::Protect {
                address: 0x3000,
                size: 0x1000,
                set_maximum: true,
                protection: 3,
            },
            TaskOp::Allocate {
                address: 0,
                size: 0x4000,
                flags: 1,
            },
            TaskOp::Deallocate {
                address: 0x5000,
                size: 0x1000,
            },
            TaskOp::Copy {
                source: 1,
                size: 2,
                dest: 3,
            },
            TaskOp::Suspend,
            TaskOp::Resume,
        ]
    }

    #[test]
    fn a_fetched_batch_reads_back_without_copying() {
        let works: Vec<Work> = every_op()
            .into_iter()
            .enumerate()
            .map(|(i, op)| Work { id: i as u64, op })
            .collect();
        let payload = encode(&Reply::Work(works.clone()));
        let batch = Batch::parse(&payload).unwrap();
        let views: Vec<WorkView<'_>> = batch.map(Result::unwrap).collect();
        assert_eq!(views.len(), works.len());
        assert_eq!(
            views[1].op,
            Op::Write {
                address: 0x2000,
                data: b"rosemary"
            }
        );
        assert_eq!(views[7].id, 7);
        assert_eq!(views[7].op, Op::Resume);
    }

    #[test]
    fn completions_built_in_place_decode_as_a_request() {
        let mut buffer = [0u8; 256];
        let mut sink = Sink::new(&mut buffer);
        let count = sink.complete(0).unwrap();
        sink.completion(1, Outcome::Done).unwrap();
        sink.data(2, 3).unwrap().copy_from_slice(b"abc");
        sink.completion(3, Outcome::Address(0x7000)).unwrap();
        sink.completion(4, Outcome::Failed(2)).unwrap();
        sink.recount(count, 4);
        let frame = sink.finish().to_vec();
        let payload = crate::codec::read_frame(&mut frame.as_slice())
            .unwrap()
            .unwrap();
        match decode::<Request>(&payload).unwrap() {
            Request::Complete(list) => assert_eq!(
                list,
                vec![
                    Completion {
                        id: 1,
                        outcome: TaskOutcome::Done
                    },
                    Completion {
                        id: 2,
                        outcome: TaskOutcome::Data(b"abc".to_vec())
                    },
                    Completion {
                        id: 3,
                        outcome: TaskOutcome::Address(0x7000)
                    },
                    Completion {
                        id: 4,
                        outcome: TaskOutcome::Failed(2)
                    },
                ]
            ),
            other => panic!("decoded as {other:?}"),
        }
    }

    #[test]
    fn a_fetch_built_in_place_decodes_as_a_request() {
        let mut buffer = [0u8; 8];
        let mut sink = Sink::new(&mut buffer);
        sink.fetch().unwrap();
        let frame = sink.finish().to_vec();
        let payload = crate::codec::read_frame(&mut frame.as_slice())
            .unwrap()
            .unwrap();
        assert!(matches!(
            decode::<Request>(&payload).unwrap(),
            Request::Fetch
        ));
    }

    #[test]
    fn a_full_sink_refuses_rather_than_grows() {
        let mut buffer = [0u8; 16];
        let mut sink = Sink::new(&mut buffer);
        sink.complete(1).unwrap();
        assert_eq!(sink.data(1, 64).err(), Some(Full));
    }

    #[test]
    fn every_op_round_trips() {
        for op in every_op() {
            assert_eq!(decode::<TaskOp>(&encode(&op)).unwrap(), op);
        }
        let outcomes = vec![
            TaskOutcome::Done,
            TaskOutcome::Data(vec![1, 2]),
            TaskOutcome::Address(9),
            TaskOutcome::Failed(1),
            TaskOutcome::Gone,
            TaskOutcome::Unreachable,
        ];
        for outcome in outcomes {
            assert_eq!(decode::<TaskOutcome>(&encode(&outcome)).unwrap(), outcome);
        }
    }

    #[test]
    fn large_reads_and_writes_split_into_chunks_in_order() {
        let pieces = split(TaskOp::Read {
            address: 0x10_0000,
            size: CHUNK as u64 * 2 + 1,
        });
        assert_eq!(pieces.len(), 3);
        assert_eq!(
            pieces[2],
            TaskOp::Read {
                address: 0x10_0000 + 2 * CHUNK as u64,
                size: 1
            }
        );
        let pieces = split(TaskOp::Write {
            address: 0,
            data: vec![7; CHUNK + 5],
        });
        assert_eq!(pieces.len(), 2);
        assert!(matches!(&pieces[1], TaskOp::Write { address, data }
            if *address == CHUNK as u64 && data.len() == 5));
    }
}
