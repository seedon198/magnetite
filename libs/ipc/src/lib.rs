#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Synchronous IPC over Endpoints, as pure logic
//! ([ADR 0018](../../docs/adr/0018-synchronous-ipc.md)).

use capability::object::EndpointState;
use capability::{MessageInfo, MSG_REGISTERS};
use sched::ThreadId;

/// A message: the packed [`MessageInfo`] word plus the message registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    /// Label, length (words of `mr` in use) and capability count.
    pub info: MessageInfo,
    /// The message registers.
    pub mr: [u64; MSG_REGISTERS],
}

/// What a receiver gets: the message as delivered, and the badge of the
/// capability the sender used. `info.caps()` is 1 exactly when a capability
/// arrived in the receive slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// Label, length and the count of capabilities that arrived.
    pub info: MessageInfo,
    /// The first `info.length()` registers as sent; the rest are zero.
    pub mr: [u64; MSG_REGISTERS],
    /// The sender's endpoint capability badge (0 if unbadged).
    pub badge: u64,
}

/// Why an IPC operation failed. The error always goes to the thread at fault
/// (ADR 0018).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// The endpoint capability lacks `SEND` (to send) or `RECV` (to receive).
    MissingRight,
    /// A capability was sent through an endpoint capability without `GRANT`.
    NoGrant,
    /// The message claims more than one capability.
    TooManyCaps,
    /// The slot naming the capability to send is empty.
    SourceEmpty,
    /// A capability was sent but the receiver named no receive slot.
    NoReceiveSlot,
    /// The receive slot is already occupied.
    ReceiveSlotFull,
    /// `Reply` with no caller waiting in the reply slot.
    NoReplySlot,
    /// `Recv` while still holding a caller in the reply slot.
    ReplySlotFull,
}

/// Which kind of thread an endpoint's queue holds; never both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    /// No thread is blocked on the endpoint.
    Empty,
    /// Blocked senders, waiting for a receiver.
    Senders,
    /// Blocked receivers, waiting for a sender.
    Receivers,
}

/// An endpoint's FIFO of blocked threads, decoded from its [`EndpointState`].
/// The queue links live in the per-thread IPC state, so the endpoint stores
/// only its two ends and the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointQueue {
    /// The longest-waiting thread.
    pub head: Option<ThreadId>,
    /// The most recently queued thread.
    pub tail: Option<ThreadId>,
    /// What the queued threads are waiting to do.
    pub kind: QueueKind,
}

impl EndpointQueue {
    /// An empty queue (what a zeroed `EndpointState` decodes to).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            kind: QueueKind::Empty,
        }
    }

    /// Decode from an endpoint's stored state.
    #[must_use]
    pub fn load(s: &EndpointState) -> Self {
        Self {
            head: decode(s.queue_head),
            tail: decode(s.queue_tail),
            kind: match s.queue_kind {
                1 => QueueKind::Senders,
                2 => QueueKind::Receivers,
                _ => QueueKind::Empty,
            },
        }
    }

    /// Encode into an endpoint's stored state.
    pub fn store(&self, s: &mut EndpointState) {
        s.queue_head = encode(self.head);
        s.queue_tail = encode(self.tail);
        s.queue_kind = match self.kind {
            QueueKind::Empty => 0,
            QueueKind::Senders => 1,
            QueueKind::Receivers => 2,
        };
    }
}

impl Default for EndpointQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A stored thread id: `id + 1`, so that zeroed memory means "none".
fn encode(t: Option<ThreadId>) -> u64 {
    t.map_or(0, |t| t.0 as u64 + 1)
}

fn decode(w: u64) -> Option<ThreadId> {
    (w != 0).then(|| ThreadId((w - 1) as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_queue_roundtrips_through_state() {
        let q = EndpointQueue {
            head: Some(ThreadId(0)),
            tail: Some(ThreadId(5)),
            kind: QueueKind::Receivers,
        };
        let mut s = EndpointState::default();
        q.store(&mut s);
        assert_eq!((s.queue_head, s.queue_tail, s.queue_kind), (1, 6, 2));
        assert_eq!(EndpointQueue::load(&s), q);
    }

    #[test]
    fn zeroed_state_is_an_empty_queue() {
        let q = EndpointQueue::load(&EndpointState::default());
        assert_eq!(q, EndpointQueue::new());
        assert_eq!(q.kind, QueueKind::Empty);
        assert_eq!((q.head, q.tail), (None, None));
    }
}
