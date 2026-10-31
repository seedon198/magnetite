#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Synchronous IPC over Endpoints, as pure logic
//! ([ADR 0018](../../docs/adr/0018-synchronous-ipc.md)).

mod engine;

pub use engine::{CapAccess, Effect, Ipc, Outcome, MAX_THREADS};

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

    use capability::{CapError, CapId, CapSpace, Capability, Rights, Slot};

    /// For tests that move no capabilities: any capability access is a bug.
    struct NoCaps;
    impl CapAccess for NoCaps {
        type Obj = u32;
        fn get(&mut self, _: ThreadId, _: CapId) -> Result<Capability<u32>, CapError> {
            unreachable!("no capability access expected")
        }
        fn is_empty(&mut self, _: ThreadId, _: CapId) -> bool {
            unreachable!("no capability access expected")
        }
        fn insert_at(&mut self, _: ThreadId, _: CapId, _: Capability<u32>) -> Result<(), CapError> {
            unreachable!("no capability access expected")
        }
    }

    const T1: ThreadId = ThreadId(1);
    const T2: ThreadId = ThreadId(2);
    const T3: ThreadId = ThreadId(3);
    const T4: ThreadId = ThreadId(4);

    fn msg(label: u32, len: u8, mr: [u64; MSG_REGISTERS]) -> Message {
        Message {
            info: MessageInfo::new(label, len, 0),
            mr,
        }
    }

    fn woken(o: &Outcome) -> [Option<ThreadId>; 4] {
        let mut w = [None; 4];
        for (i, t) in o.woken().enumerate() {
            w[i] = Some(t);
        }
        w
    }

    #[test]
    fn recv_then_send_delivers_message_and_badge() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let o = ipc.recv(&mut NoCaps, &mut ep, T1, Rights::RECV, None);
        assert_eq!(o.caller, Effect::Block);
        let o = ipc.send(
            &mut NoCaps,
            &mut ep,
            T2,
            Rights::SEND,
            7,
            msg(42, 2, [1, 2, 99, 99]),
            None,
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(T1), None, None, None]);
        let r = ipc.take_result(T1).unwrap().unwrap().unwrap();
        assert_eq!((r.info.label(), r.info.length(), r.info.caps()), (42, 2, 0));
        assert_eq!(r.mr, [1, 2, 0, 0], "registers past length must not leak");
        assert_eq!(r.badge, 7);
        assert_eq!(ipc.take_result(T2), Some(Ok(None)));
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn send_then_recv_delivers() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let o = ipc.send(
            &mut NoCaps,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            msg(5, 1, [9, 0, 0, 0]),
            None,
        );
        assert_eq!(o.caller, Effect::Block);
        assert_eq!(ep.kind, QueueKind::Senders);
        let o = ipc.recv(&mut NoCaps, &mut ep, T1, Rights::RECV, None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(T2), None, None, None]);
        let r = ipc.take_result(T1).unwrap().unwrap().unwrap();
        assert_eq!((r.info.label(), r.mr[0], r.badge), (5, 9, 0));
        assert_eq!(ipc.take_result(T2), Some(Ok(None)));
        assert_eq!(ep.kind, QueueKind::Empty);
    }

    #[test]
    fn queued_senders_are_served_fifo() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        for (t, label) in [(T1, 1), (T2, 2), (T3, 3)] {
            let o = ipc.send(
                &mut NoCaps,
                &mut ep,
                t,
                Rights::SEND,
                0,
                msg(label, 0, [0; 4]),
                None,
            );
            assert_eq!(o.caller, Effect::Block);
        }
        for want in 1..=3 {
            let o = ipc.recv(&mut NoCaps, &mut ep, T4, Rights::RECV, None);
            assert_eq!(o.caller, Effect::Done);
            let r = ipc.take_result(T4).unwrap().unwrap().unwrap();
            assert_eq!(r.info.label(), want);
        }
        assert_eq!(ep.kind, QueueKind::Empty);
    }

    #[test]
    fn send_without_send_right_fails() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let o = ipc.send(
            &mut NoCaps,
            &mut ep,
            T1,
            Rights::RECV,
            0,
            msg(1, 0, [0; 4]),
            None,
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T1), Some(Err(IpcError::MissingRight)));
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn recv_without_recv_right_fails() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let o = ipc.recv(&mut NoCaps, &mut ep, T1, Rights::SEND, None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T1), Some(Err(IpcError::MissingRight)));
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn queue_never_mixes_kinds() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        ipc.recv(&mut NoCaps, &mut ep, T1, Rights::RECV, None);
        ipc.recv(&mut NoCaps, &mut ep, T2, Rights::RECV, None);
        assert_eq!(ep.kind, QueueKind::Receivers);
        ipc.send(
            &mut NoCaps,
            &mut ep,
            T3,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
        );
        assert_eq!(
            (ep.kind, ep.head, ep.tail),
            (QueueKind::Receivers, Some(T2), Some(T2))
        );
        ipc.send(
            &mut NoCaps,
            &mut ep,
            T4,
            Rights::SEND,
            0,
            msg(2, 0, [0; 4]),
            None,
        );
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn call_reply_roundtrip() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let (server, client) = (T1, T2);
        ipc.recv(&mut NoCaps, &mut ep, server, Rights::RECV, None);
        let o = ipc.call(
            &mut NoCaps,
            &mut ep,
            client,
            Rights::SEND,
            0,
            msg(42, 0, [0; 4]),
            None,
            None,
        );
        assert_eq!(o.caller, Effect::Block);
        assert_eq!(woken(&o), [Some(server), None, None, None]);
        let r = ipc.take_result(server).unwrap().unwrap().unwrap();
        assert_eq!(r.info.label(), 42);
        assert_eq!(
            ipc.take_result(client),
            None,
            "the caller waits for the reply"
        );

        let o = ipc.reply(&mut NoCaps, server, msg(1, 1, [10, 0, 0, 0]), None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(client), None, None, None]);
        let r = ipc.take_result(client).unwrap().unwrap().unwrap();
        assert_eq!((r.info.label(), r.mr[0], r.badge), (1, 10, 0));
        assert_eq!(ipc.take_result(server), Some(Ok(None)));

        let o = ipc.reply(&mut NoCaps, server, msg(1, 0, [0; 4]), None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(
            ipc.take_result(server),
            Some(Err(IpcError::NoReplySlot)),
            "the reply slot is one-shot"
        );
    }

    #[test]
    fn reply_with_empty_slot_fails() {
        let mut ipc = Ipc::new();
        let o = ipc.reply(&mut NoCaps, T1, msg(1, 0, [0; 4]), None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [None; 4]);
        assert_eq!(ipc.take_result(T1), Some(Err(IpcError::NoReplySlot)));
    }

    #[test]
    fn recv_with_full_reply_slot_fails() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        ipc.call(
            &mut NoCaps,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            None,
        );
        ipc.recv(&mut NoCaps, &mut ep, T1, Rights::RECV, None);
        ipc.take_result(T1);
        let o = ipc.recv(&mut NoCaps, &mut ep, T1, Rights::RECV, None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T1), Some(Err(IpcError::ReplySlotFull)));
        assert_eq!(ipc.take_result(T2), None, "the caller is still waiting");
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn reply_recv_with_empty_slot_just_receives() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let o = ipc.reply_recv(
            &mut NoCaps,
            &mut ep,
            T1,
            Rights::RECV,
            msg(0, 0, [0; 4]),
            None,
            None,
        );
        assert_eq!(o.caller, Effect::Block);
        assert_eq!(ipc.take_result(T1), None);
        assert_eq!((ep.kind, ep.head), (QueueKind::Receivers, Some(T1)));
    }

    #[test]
    fn reply_recv_replies_then_receives() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let (server, a, b) = (T1, T2, T3);
        ipc.call(
            &mut NoCaps,
            &mut ep,
            a,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            None,
        );
        ipc.recv(&mut NoCaps, &mut ep, server, Rights::RECV, None);
        ipc.take_result(server);
        ipc.send(
            &mut NoCaps,
            &mut ep,
            b,
            Rights::SEND,
            3,
            msg(2, 0, [0; 4]),
            None,
        );

        let o = ipc.reply_recv(
            &mut NoCaps,
            &mut ep,
            server,
            Rights::RECV,
            msg(9, 0, [0; 4]),
            None,
            None,
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(a), Some(b), None, None]);
        assert_eq!(
            ipc.take_result(a).unwrap().unwrap().unwrap().info.label(),
            9
        );
        let r = ipc.take_result(server).unwrap().unwrap().unwrap();
        assert_eq!((r.info.label(), r.badge), (2, 3));
        assert_eq!(ipc.take_result(b), Some(Ok(None)));
    }

    /// Eight threads with eight capability slots each, on real `CapSpace`s.
    struct TestCaps {
        s: [[Slot<u32>; 8]; 8],
    }

    impl TestCaps {
        fn new() -> Self {
            Self {
                s: [[Slot::empty(); 8]; 8],
            }
        }
        fn put(&mut self, t: ThreadId, slot: u32, cap: Capability<u32>) {
            CapSpace::new(&mut self.s[t.0])
                .insert_at(CapId(slot), cap)
                .unwrap();
        }
        fn at(&mut self, t: ThreadId, slot: u32) -> Option<Capability<u32>> {
            CapSpace::new(&mut self.s[t.0]).get(CapId(slot)).ok()
        }
        fn del(&mut self, t: ThreadId, slot: u32) {
            CapSpace::new(&mut self.s[t.0]).delete(CapId(slot)).unwrap();
        }
    }

    impl CapAccess for TestCaps {
        type Obj = u32;
        fn get(&mut self, t: ThreadId, slot: CapId) -> Result<Capability<u32>, CapError> {
            CapSpace::new(&mut self.s[t.0]).get(slot)
        }
        fn is_empty(&mut self, t: ThreadId, slot: CapId) -> bool {
            CapSpace::new(&mut self.s[t.0]).get(slot) == Err(CapError::Empty)
        }
        fn insert_at(
            &mut self,
            t: ThreadId,
            slot: CapId,
            cap: Capability<u32>,
        ) -> Result<(), CapError> {
            CapSpace::new(&mut self.s[t.0]).insert_at(slot, cap)
        }
    }

    const SG: Rights = Rights::SEND.union(Rights::GRANT);

    fn frame() -> Capability<u32> {
        Capability {
            object: 77,
            rights: Rights::READ | Rights::WRITE,
            badge: 5,
        }
    }

    fn with_cap(label: u32) -> Message {
        Message {
            info: MessageInfo::new(label, 0, 1),
            mr: [0; 4],
        }
    }

    #[test]
    fn cap_transfer_copies_rights_and_badge() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        let o = ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(
            ipc.take_result(T1).unwrap().unwrap().unwrap().info.caps(),
            1
        );
        assert_eq!(c.at(T1, 6), Some(frame()), "same rights and badge");
        assert_eq!(c.at(T2, 2), Some(frame()), "copied: the sender keeps it");
    }

    #[test]
    fn cap_transfer_to_a_receiver_arriving_later() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        let o = ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)));
        assert_eq!(o.caller, Effect::Block);
        let o = ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(c.at(T1, 6), Some(frame()));
        assert_eq!(ipc.take_result(T2), Some(Ok(None)));
    }

    #[test]
    fn cap_without_grant_fails_nothing_delivered() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        let o = ipc.send(
            &mut c,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            with_cap(1),
            Some(CapId(2)),
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [None; 4]);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::NoGrant)));
        assert_eq!(
            (ep.kind, ep.head),
            (QueueKind::Receivers, Some(T1)),
            "the receiver still waits"
        );
        assert_eq!(ipc.take_result(T1), None);
        assert_eq!(c.at(T1, 6), None);
    }

    #[test]
    fn plain_send_without_grant_succeeds() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        let o = ipc.send(
            &mut c,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            msg(3, 0, [0; 4]),
            None,
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(
            ipc.take_result(T1).unwrap().unwrap().unwrap().info.caps(),
            0
        );
        assert_eq!(c.at(T1, 6), None);
    }

    #[test]
    fn too_many_caps_fails() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        let two = Message {
            info: MessageInfo::new(1, 0, 2),
            mr: [0; 4],
        };
        let o = ipc.send(&mut c, &mut ep, T2, SG, 0, two, Some(CapId(2)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::TooManyCaps)));
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn empty_source_fails_immediately() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        let o = ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::SourceEmpty)));
        assert_eq!(ep, EndpointQueue::new(), "the sender is not queued");
    }

    #[test]
    fn recv_naming_full_slot_fails() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T1, 6, frame());
        let o = ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T1), Some(Err(IpcError::ReceiveSlotFull)));
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn arriving_sender_fails_when_receiver_named_no_slot() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, None);
        let o = ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::NoReceiveSlot)));
        assert_eq!((ep.kind, ep.head), (QueueKind::Receivers, Some(T1)));
    }

    #[test]
    fn queued_sender_fails_and_receiver_moves_on() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)));
        ipc.send(
            &mut c,
            &mut ep,
            T3,
            Rights::SEND,
            0,
            msg(2, 0, [0; 4]),
            None,
        );
        let o = ipc.recv(&mut c, &mut ep, T1, Rights::RECV, None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(T2), Some(T3), None, None]);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::NoReceiveSlot)));
        assert_eq!(
            ipc.take_result(T1).unwrap().unwrap().unwrap().info.label(),
            2
        );
        assert_eq!(ipc.take_result(T3), Some(Ok(None)));
        assert_eq!(ep, EndpointQueue::new());
    }

    #[test]
    fn receiver_skips_many_failing_senders() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        for t in [T2, T3, T4] {
            c.put(t, 2, frame());
            ipc.send(&mut c, &mut ep, t, SG, 0, with_cap(1), Some(CapId(2)));
        }
        let t5 = ThreadId(5);
        ipc.send(
            &mut c,
            &mut ep,
            t5,
            Rights::SEND,
            0,
            msg(5, 0, [0; 4]),
            None,
        );
        let o = ipc.recv(&mut c, &mut ep, T1, Rights::RECV, None);
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(T2), Some(T3), Some(T4), Some(t5)]);
        assert_eq!(
            ipc.take_result(T1).unwrap().unwrap().unwrap().info.label(),
            5
        );
        assert_eq!(
            ep,
            EndpointQueue::new(),
            "no sender is left queued behind a waiting receiver"
        );
    }

    #[test]
    fn receive_slot_filled_meanwhile_fails_sender() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        c.put(T1, 6, frame());
        let o = ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::ReceiveSlotFull)));
        assert_eq!((ep.kind, ep.head), (QueueKind::Receivers, Some(T1)));
        assert_eq!(ipc.take_result(T1), None);
    }

    #[test]
    fn source_emptied_while_queued_fails_sender() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 2, frame());
        assert_eq!(
            ipc.send(&mut c, &mut ep, T2, SG, 0, with_cap(1), Some(CapId(2)))
                .caller,
            Effect::Block
        );
        c.del(T2, 2);
        let o = ipc.recv(&mut c, &mut ep, T1, Rights::RECV, Some(CapId(6)));
        assert_eq!(o.caller, Effect::Block, "nothing left to receive");
        assert_eq!(woken(&o), [Some(T2), None, None, None]);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::SourceEmpty)));
        assert_eq!(c.at(T1, 6), None);
    }

    #[test]
    fn reply_with_cap_into_callers_slot() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, None);
        ipc.call(
            &mut c,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            Some(CapId(4)),
        );
        ipc.take_result(T1);
        c.put(T1, 3, frame());
        let o = ipc.reply(&mut c, T1, with_cap(9), Some(CapId(3)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [Some(T2), None, None, None]);
        assert_eq!(
            ipc.take_result(T2).unwrap().unwrap().unwrap().info.caps(),
            1
        );
        assert_eq!(c.at(T2, 4), Some(frame()));
    }

    #[test]
    fn reply_with_cap_without_consent_fails_and_keeps_slot() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, None);
        ipc.call(
            &mut c,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            None,
        );
        ipc.take_result(T1);
        c.put(T1, 3, frame());
        let o = ipc.reply(&mut c, T1, with_cap(9), Some(CapId(3)));
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [None; 4]);
        assert_eq!(ipc.take_result(T1), Some(Err(IpcError::NoReceiveSlot)));
        assert_eq!(ipc.take_result(T2), None, "the caller still waits");
        let o = ipc.reply(&mut c, T1, msg(9, 0, [0; 4]), None);
        assert_eq!(
            woken(&o),
            [Some(T2), None, None, None],
            "a plain retry succeeds"
        );
    }

    #[test]
    fn call_naming_full_reply_slot_fails() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        c.put(T2, 4, frame());
        ipc.recv(&mut c, &mut ep, T1, Rights::RECV, None);
        let o = ipc.call(
            &mut c,
            &mut ep,
            T2,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            Some(CapId(4)),
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [None; 4]);
        assert_eq!(ipc.take_result(T2), Some(Err(IpcError::ReceiveSlotFull)));
        assert_eq!(
            (ep.kind, ep.head),
            (QueueKind::Receivers, Some(T1)),
            "the server keeps waiting"
        );
        assert_eq!(ipc.take_result(T1), None);
    }

    #[test]
    fn reply_recv_failed_reply_skips_receive() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        let (server, client, other) = (T1, T2, T3);
        ipc.recv(&mut c, &mut ep, server, Rights::RECV, None);
        ipc.call(
            &mut c,
            &mut ep,
            client,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            None,
        );
        ipc.take_result(server);
        ipc.send(
            &mut c,
            &mut ep,
            other,
            Rights::SEND,
            0,
            msg(2, 0, [0; 4]),
            None,
        );
        c.put(server, 3, frame());
        let o = ipc.reply_recv(
            &mut c,
            &mut ep,
            server,
            Rights::RECV,
            with_cap(9),
            Some(CapId(3)),
            None,
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [None; 4]);
        assert_eq!(ipc.take_result(server), Some(Err(IpcError::NoReceiveSlot)));
        assert_eq!(ipc.take_result(client), None, "the caller still waits");
        assert_eq!(
            (ep.kind, ep.head),
            (QueueKind::Senders, Some(other)),
            "the receive half did not run"
        );
        let o = ipc.reply(&mut c, server, msg(9, 0, [0; 4]), None);
        assert_eq!(
            woken(&o),
            [Some(client), None, None, None],
            "the slot kept the caller"
        );
    }

    #[test]
    fn reply_recv_replies_then_blocks() {
        let (mut ipc, mut ep) = (Ipc::new(), EndpointQueue::new());
        let (server, client) = (T1, T2);
        ipc.recv(&mut NoCaps, &mut ep, server, Rights::RECV, None);
        ipc.call(
            &mut NoCaps,
            &mut ep,
            client,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            None,
        );
        ipc.take_result(server);
        let o = ipc.reply_recv(
            &mut NoCaps,
            &mut ep,
            server,
            Rights::RECV,
            msg(9, 0, [0; 4]),
            None,
            None,
        );
        assert_eq!(o.caller, Effect::Block);
        assert_eq!(woken(&o), [Some(client), None, None, None]);
        assert_eq!(
            ipc.take_result(client)
                .unwrap()
                .unwrap()
                .unwrap()
                .info
                .label(),
            9
        );
        assert_eq!(ipc.take_result(server), None);
        assert_eq!((ep.kind, ep.head), (QueueKind::Receivers, Some(server)));
    }

    #[test]
    fn reply_recv_full_recv_slot_does_not_reply() {
        let (mut ipc, mut ep, mut c) = (Ipc::new(), EndpointQueue::new(), TestCaps::new());
        let (server, client) = (T1, T2);
        ipc.recv(&mut c, &mut ep, server, Rights::RECV, None);
        ipc.call(
            &mut c,
            &mut ep,
            client,
            Rights::SEND,
            0,
            msg(1, 0, [0; 4]),
            None,
            None,
        );
        ipc.take_result(server);
        c.put(server, 6, frame());
        let o = ipc.reply_recv(
            &mut c,
            &mut ep,
            server,
            Rights::RECV,
            msg(9, 0, [0; 4]),
            None,
            Some(CapId(6)),
        );
        assert_eq!(o.caller, Effect::Done);
        assert_eq!(woken(&o), [None; 4], "no reply was sent");
        assert_eq!(
            ipc.take_result(server),
            Some(Err(IpcError::ReceiveSlotFull))
        );
        assert_eq!(ipc.take_result(client), None);
    }

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
