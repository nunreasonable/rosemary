//! The process server, Rosemary's counterpart to `wineserver` and
//! `darlingserver`.
//!
//! Every guest is a host process of its own, and a process cannot see past
//! itself: `top`, `ps` and anything else that enumerates tasks need somebody
//! who knows every guest at once. That is this server - one per prefix,
//! started on demand by the first guest and gone a few seconds after the last.
//!
//! It also boots and shuts down the prefix, as `darlingserver` does: the
//! prefix's launchd runs first, as pid 1, and the guest that asked runs once
//! it is up. On the way down the server ends every guest outside launchd's
//! tree, asks launchd to stop its services, and leaves once nothing is left.
//!
//! * [`client`] is a guest's side: check in once, then ask.
//! * [`credentials`] are a guest's user and group ids, which the server
//!   holds for every guest and which change by XNU's rules.
//! * [`task`] is what one task has the server ask of another, and
//!   [`mailbox`] the queues that hold it until the target answers.
//! * [`daemon`] is the server: a process table ([`registry`]) kept honest by
//!   the host kernel (through `starnix`), answered over a private Unix socket
//!   ([`location`], [`protocol`], [`codec`]).
//!
//! See `docs/server.md` for the design and what is still out of scope.

pub mod client;
pub mod codec;
pub mod credentials;
pub mod daemon;
pub mod location;
pub mod mailbox;
pub mod protocol;
pub mod registry;
pub mod task;
