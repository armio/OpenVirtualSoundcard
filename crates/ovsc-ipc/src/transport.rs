//! The transport interface between the driver (client) and the daemon
//! (server), with two implementations: XPC on macOS (`crate::xpc`) and an
//! in-process one for tests on every OS ([`crate::mem`]).
//!
//! The semantics follow XPC, and both implementations keep them:
//!
//! * Each side runs every callback (handler events, replies, `after` and
//!   `run` work) on one serial queue, one at a time.
//! * A client connection is made lazily: `connect` installs the handler and
//!   the first `send` or `request` reaches the server, which then sees a new
//!   peer before that peer's first message.
//! * `on_interrupted` means the daemon went away. The connection stays
//!   usable: the next message goes to the next daemon instance.
//! * `on_invalid` means the connection can never work (no such service, or
//!   the listener is gone); `connect` must be called again to retry.
//! * After `cancel`, or a new `connect`, nothing from the old connection
//!   reaches the handler, and the transport drops its handler reference.
//! * Every `request` gets exactly one reply: the daemon's message, or
//!   [`TransportError::Timeout`] after the timeout,
//!   [`TransportError::Interrupted`] if the daemon went away first,
//!   [`TransportError::Invalid`] if the connection is (or becomes) invalid or
//!   cancelled, or [`TransportError::Decode`] if the reply is not a message.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::protocol::{ProtoError, ToDaemon, ToPlugin};

/// The callback that receives a request's outcome.
pub type ReplyFn = Box<dyn FnOnce(Result<ToPlugin, TransportError>) + Send>;

/// A client connection as the server sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    /// Unique per server, never reused.
    pub id: u64,
    /// The peer's effective user ID.
    pub euid: u32,
    pub pid: i32,
}

/// Why a request got no message back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The daemon went away (it exited, or refused the peer).
    Interrupted,
    /// The connection is unusable: no such service, or it was cancelled.
    Invalid,
    /// No reply within the timeout.
    Timeout,
    /// The reply is not a message this version understands.
    Decode(ProtoError),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Interrupted => write!(f, "connection interrupted"),
            TransportError::Invalid => write!(f, "connection invalid"),
            TransportError::Timeout => write!(f, "no reply in time"),
            TransportError::Decode(e) => write!(f, "undecodable reply: {e}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// The driver's side of a connection's events. Called on the transport's
/// queue.
pub trait ClientHandler: Send + Sync {
    /// An unsolicited message from the daemon.
    fn on_message(&self, m: ToPlugin);
    /// The daemon went away; the connection will reach the next instance.
    fn on_interrupted(&self);
    /// The connection can never work; `connect` again to retry.
    fn on_invalid(&self);
}

/// The driver's connection to the daemon.
pub trait ClientTransport: Send + Sync {
    /// (Re)creates the connection with `h` as its handler, replacing any
    /// previous one.
    fn connect(&self, h: Arc<dyn ClientHandler>);
    /// Ends the connection; pending requests fail with `Invalid`.
    fn cancel(&self);
    /// Sends a message that expects no reply. Lost if there is no
    /// connection or no daemon.
    fn send(&self, m: ToDaemon);
    /// Sends a message and calls `reply` exactly once, on the queue.
    fn request(
        &self,
        m: ToDaemon,
        timeout: Duration,
        reply: Box<dyn FnOnce(Result<ToPlugin, TransportError>) + Send>,
    );
    /// Runs `f` on the queue after `delay`.
    fn after(&self, delay: Duration, f: Box<dyn FnOnce() + Send>);
    /// Runs `f` on the queue.
    fn run(&self, f: Box<dyn FnOnce() + Send>);
}

/// The daemon's side of its peers' events. Called on the transport's queue.
pub trait ServerHandler: Send + Sync {
    /// A new peer, before any of its messages. Returning false cancels the
    /// connection without a reply; the peer sees an interruption.
    fn on_peer(&self, p: PeerInfo) -> bool;
    /// A message that expects a reply. `None` sends none: the peer's request
    /// times out.
    fn on_request(&self, peer: u64, m: ToDaemon) -> Option<ToPlugin>;
    /// A message that expects no reply.
    fn on_message(&self, peer: u64, m: ToDaemon);
    /// An accepted peer disconnected.
    fn on_peer_gone(&self, peer: u64);
}

/// The daemon's listener.
pub trait ServerTransport: Send + Sync {
    /// Starts accepting peers. Fails if already started or if the listener
    /// cannot be created.
    fn start(&self, h: Arc<dyn ServerHandler>) -> io::Result<()>;
    /// Sends an unsolicited message to an accepted peer; ignored for any
    /// other peer.
    fn send(&self, peer: u64, m: ToPlugin);
}
