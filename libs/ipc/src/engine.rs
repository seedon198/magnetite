//! The IPC engine: per-thread IPC state, endpoint queues and the rendezvous.
//!
//! Every operation is a pure state transition. It returns an [`Outcome`]
//! saying whether the calling thread keeps running or blocks, and which
//! threads become ready again. The kernel applies that to the scheduler. A
//! thread reads the result of its operation with [`Ipc::take_result`]: at once
//! if it kept running, or when it resumes after being woken.
//!
//! Errors always land on the thread at fault (ADR 0018). A capability transfer
//! that cannot complete fails the **sender**, whichever side arrived first,
//! and moves nothing; the receiver is never penalized.

use capability::{CapError, CapId, Capability, MessageInfo, Rights, MSG_REGISTERS};
use sched::ThreadId;

use crate::{EndpointQueue, IpcError, Message, QueueKind, Received};

/// How many threads the engine tracks: one slot per schedulable thread.
pub const MAX_THREADS: usize = sched::MAX_THREADS;

// Woken thread ids are stored in a byte each.
const _: () = assert!(MAX_THREADS <= u8::MAX as usize);

/// The kernel's access to the threads' capability spaces, which the engine
/// needs only to move a capability with a message.
pub trait CapAccess {
    /// The kernel's object handle, carried opaquely.
    type Obj: Copy;
    /// The capability in thread `t`'s `slot`.
    fn get(&mut self, t: ThreadId, slot: CapId) -> Result<Capability<Self::Obj>, CapError>;
    /// Whether thread `t`'s `slot` exists and is empty (an out-of-range slot
    /// is not empty).
    fn is_empty(&mut self, t: ThreadId, slot: CapId) -> bool;
    /// Store `cap` in thread `t`'s empty `slot`.
    fn insert_at(
        &mut self,
        t: ThreadId,
        slot: CapId,
        cap: Capability<Self::Obj>,
    ) -> Result<(), CapError>;
}

/// What happens to the thread that invoked an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// It keeps running; its result is ready in [`Ipc::take_result`].
    Done,
    /// It must block until a later operation wakes it.
    Block,
}

/// The result of one operation: the caller's fate and the threads to make
/// ready again, in order. One operation can wake many threads (a receiver
/// fails every queued sender it cannot take on its way to a match), so the
/// list holds every thread, a byte each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the caller keeps running or blocks.
    pub caller: Effect,
    wake: [u8; MAX_THREADS],
    len: u8,
}

impl Outcome {
    fn new(caller: Effect) -> Self {
        Self {
            caller,
            wake: [0; MAX_THREADS],
            len: 0,
        }
    }

    fn push(&mut self, t: ThreadId) {
        // Each thread is queued at most once, so it is woken at most once.
        self.wake[usize::from(self.len)] = t.0 as u8;
        self.len += 1;
    }

    /// The threads to make ready again, in order.
    pub fn woken(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.wake[..usize::from(self.len)]
            .iter()
            .map(|&t| ThreadId(usize::from(t)))
    }
}

/// What a thread is doing, from IPC's point of view.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Not blocked in IPC.
    Idle,
    /// Queued on an endpoint, waiting for a receiver.
    Sending(Pending),
    /// Queued on an endpoint, waiting for a sender. `slot` is where an
    /// incoming capability may land.
    Receiving { slot: Option<CapId> },
    /// Made a `Call` that a receiver took; waiting for its reply. `slot` is
    /// where a capability carried by the reply may land: naming one is the
    /// caller's consent to receive a capability.
    AwaitingReply { slot: Option<CapId> },
}

/// A message on its way: everything a queued sender needs to complete later.
#[derive(Debug, Clone, Copy)]
struct Pending {
    msg: Message,
    badge: u64,
    /// The sender's slot holding the capability to transfer, if any.
    cap: Option<CapId>,
    /// `Some(reply_slot)` for a `Call`: the sender then awaits a reply.
    call: Option<Option<CapId>>,
}

/// One thread's IPC state.
#[derive(Debug, Clone, Copy)]
struct Tcb {
    state: State,
    /// The next thread in the endpoint queue this thread is on.
    next: Option<ThreadId>,
    /// The one-shot reply slot: the caller whose `Call` this thread took.
    reply_to: Option<ThreadId>,
    /// The result of this thread's last operation, until it is taken.
    result: Option<Result<Option<Received>, IpcError>>,
}

impl Tcb {
    const IDLE: Self = Self {
        state: State::Idle,
        next: None,
        reply_to: None,
        result: None,
    };
}

/// The IPC engine: the IPC state of every thread.
pub struct Ipc {
    t: [Tcb; MAX_THREADS],
}

impl Default for Ipc {
    fn default() -> Self {
        Self::new()
    }
}

// The operations take the invocation's parts as plain arguments, matching the
// register ABI one to one; bundling them would only rename them.
#[allow(clippy::too_many_arguments)]
impl Ipc {
    /// No thread is blocked and no result is pending.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            t: [Tcb::IDLE; MAX_THREADS],
        }
    }

    /// Take the result of `me`'s last operation: `Ok(None)` for a completed
    /// send or reply, `Ok(Some(_))` for a receive. `None` while still blocked.
    pub fn take_result(&mut self, me: ThreadId) -> Option<Result<Option<Received>, IpcError>> {
        self.t[me.0].result.take()
    }

    /// Send `msg` through an endpoint capability with `ep_rights` and
    /// `ep_badge`. Blocks until a receiver takes it. With `msg.info.caps()`
    /// of 1, the capability in `cap` is copied to the receiver; that needs
    /// `GRANT` on the endpoint capability.
    pub fn send<C: CapAccess>(
        &mut self,
        caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        ep_badge: u64,
        msg: Message,
        cap: Option<CapId>,
    ) -> Outcome {
        self.send_inner(caps, ep, me, ep_rights, ep_badge, msg, cap, None)
    }

    /// Send `msg` like [`send`](Self::send), then wait for the reply as one
    /// step. The receiver that takes it holds this thread in its reply slot.
    /// `reply_recv_slot` is where a capability carried by the reply may land;
    /// naming none refuses capabilities in the reply.
    pub fn call<C: CapAccess>(
        &mut self,
        caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        ep_badge: u64,
        msg: Message,
        cap: Option<CapId>,
        reply_recv_slot: Option<CapId>,
    ) -> Outcome {
        let call = Some(reply_recv_slot);
        self.send_inner(caps, ep, me, ep_rights, ep_badge, msg, cap, call)
    }

    /// Receive through an endpoint capability with `ep_rights`, blocking
    /// until a sender arrives. `recv_slot` is where an incoming capability may
    /// land; it must be empty. Fails with `ReplySlotFull` while a caller
    /// awaits this thread's reply.
    pub fn recv<C: CapAccess>(
        &mut self,
        caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        recv_slot: Option<CapId>,
    ) -> Outcome {
        if let Err(e) = self.recv_checks(caps, me, ep_rights, recv_slot) {
            return self.fail(me, e);
        }
        if self.t[me.0].reply_to.is_some() {
            return self.fail(me, IpcError::ReplySlotFull);
        }
        self.recv_unchecked(caps, ep, me, recv_slot, Outcome::new(Effect::Done))
    }

    /// Answer the thread in `me`'s reply slot with `msg`, waking it and
    /// emptying the slot. A capability in the reply lands in the slot the
    /// caller named in its `Call`; if it named none, the reply fails with
    /// `NoReceiveSlot`. On any failure the caller stays waiting in the slot.
    pub fn reply<C: CapAccess>(
        &mut self,
        caps: &mut C,
        me: ThreadId,
        msg: Message,
        cap: Option<CapId>,
    ) -> Outcome {
        let Some(caller) = self.t[me.0].reply_to else {
            return self.fail(me, IpcError::NoReplySlot);
        };
        let State::AwaitingReply { slot } = self.t[caller.0].state else {
            unreachable!("ipc: the thread in a reply slot awaits a reply");
        };
        let reply = Pending {
            msg,
            badge: 0,
            cap: if msg.info.caps() == 1 { cap } else { None },
            call: None,
        };
        let checked = Self::sender_checks(caps, me, &msg, cap)
            .and_then(|()| Self::transfer_checks(caps, &reply, me, caller, slot));
        if let Err(e) = checked {
            return self.fail(me, e);
        }
        self.t[me.0].reply_to = None;
        self.deliver(caps, reply, me, caller, slot);
        self.t[me.0].result = Some(Ok(None));
        let mut out = Outcome::new(Effect::Done);
        out.push(caller);
        out
    }

    /// `Reply` (if the reply slot holds a caller), then `Recv`, as one
    /// operation: a server's loop. With an empty reply slot it just receives.
    /// If the reply fails, the receive does not run.
    pub fn reply_recv<C: CapAccess>(
        &mut self,
        caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        msg: Message,
        cap: Option<CapId>,
        recv_slot: Option<CapId>,
    ) -> Outcome {
        if let Err(e) = self.recv_checks(caps, me, ep_rights, recv_slot) {
            return self.fail(me, e);
        }
        let mut out = Outcome::new(Effect::Done);
        if self.t[me.0].reply_to.is_some() {
            out = self.reply(caps, me, msg, cap);
            if let Some(Err(_)) = self.t[me.0].result {
                return out;
            }
            self.t[me.0].result = None;
        }
        self.recv_unchecked(caps, ep, me, recv_slot, out)
    }

    fn send_inner<C: CapAccess>(
        &mut self,
        caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        ep_badge: u64,
        msg: Message,
        cap: Option<CapId>,
        call: Option<Option<CapId>>,
    ) -> Outcome {
        if !ep_rights.contains(Rights::SEND) {
            return self.fail(me, IpcError::MissingRight);
        }
        if msg.info.caps() == 1 && !ep_rights.contains(Rights::GRANT) {
            return self.fail(me, IpcError::NoGrant);
        }
        if let Err(e) = Self::sender_checks(caps, me, &msg, cap) {
            return self.fail(me, e);
        }
        // A caller consents to a capability in the reply by naming an empty
        // slot; a full one is its own mistake, refused now rather than failing
        // the server's reply later.
        if let Some(Some(slot)) = call {
            if !caps.is_empty(me, slot) {
                return self.fail(me, IpcError::ReceiveSlotFull);
            }
        }
        let pending = Pending {
            msg,
            badge: ep_badge,
            cap: if msg.info.caps() == 1 { cap } else { None },
            call,
        };
        if ep.kind == QueueKind::Receivers {
            let r = ep.head.expect("ipc: a receivers queue has a head");
            let State::Receiving { slot } = self.t[r.0].state else {
                unreachable!("ipc: a queued receiver is in the Receiving state");
            };
            // A transfer the waiting receiver cannot take fails this sender;
            // the receiver keeps waiting.
            if let Err(e) = Self::transfer_checks(caps, &pending, me, r, slot) {
                return self.fail(me, e);
            }
            self.dequeue(ep);
            let mut out = Outcome::new(Effect::Done);
            out.push(r);
            if !self.complete(caps, pending, me, r, slot) {
                out.caller = Effect::Block;
            }
            out
        } else {
            self.enqueue(ep, me, QueueKind::Senders, State::Sending(pending));
            Outcome::new(Effect::Block)
        }
    }

    /// The receive half, after its checks: take the first queued sender whose
    /// message can be delivered, failing (and waking) every sender ahead of it
    /// whose capability transfer cannot complete; block if none is left.
    /// `out` carries wakes from an earlier half of the operation.
    fn recv_unchecked<C: CapAccess>(
        &mut self,
        caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        recv_slot: Option<CapId>,
        mut out: Outcome,
    ) -> Outcome {
        while ep.kind == QueueKind::Senders {
            let s = self.dequeue(ep).expect("ipc: a senders queue has a head");
            let State::Sending(pending) = self.t[s.0].state else {
                unreachable!("ipc: a queued sender is in the Sending state");
            };
            if let Err(e) = Self::transfer_checks(caps, &pending, s, me, recv_slot) {
                self.t[s.0].state = State::Idle;
                self.t[s.0].result = Some(Err(e));
                out.push(s);
                continue;
            }
            if self.complete(caps, pending, s, me, recv_slot) {
                out.push(s);
            }
            out.caller = Effect::Done;
            return out;
        }
        let state = State::Receiving { slot: recv_slot };
        self.enqueue(ep, me, QueueKind::Receivers, state);
        out.caller = Effect::Block;
        out
    }

    /// Checks on the receiving side at invocation: the `RECV` right, and a
    /// named receive slot that exists and is empty.
    fn recv_checks<C: CapAccess>(
        &self,
        caps: &mut C,
        me: ThreadId,
        ep_rights: Rights,
        recv_slot: Option<CapId>,
    ) -> Result<(), IpcError> {
        if !ep_rights.contains(Rights::RECV) {
            return Err(IpcError::MissingRight);
        }
        match recv_slot {
            Some(slot) if !caps.is_empty(me, slot) => Err(IpcError::ReceiveSlotFull),
            _ => Ok(()),
        }
    }

    /// The sender's own checks at invocation: at most one capability, and a
    /// capability actually in the named slot.
    fn sender_checks<C: CapAccess>(
        caps: &mut C,
        me: ThreadId,
        msg: &Message,
        cap: Option<CapId>,
    ) -> Result<(), IpcError> {
        match msg.info.caps() {
            0 => Ok(()),
            1 => match cap {
                Some(src) if caps.get(me, src).is_ok() => Ok(()),
                _ => Err(IpcError::SourceEmpty),
            },
            _ => Err(IpcError::TooManyCaps),
        }
    }

    /// Whether `p`'s capability (if any) can move from `s` to `r`'s `slot`
    /// right now. Checked again at every rendezvous: the source may have been
    /// emptied, or the receive slot filled, since the threads queued.
    fn transfer_checks<C: CapAccess>(
        caps: &mut C,
        p: &Pending,
        s: ThreadId,
        r: ThreadId,
        slot: Option<CapId>,
    ) -> Result<(), IpcError> {
        let Some(src) = p.cap else {
            return Ok(());
        };
        if caps.get(s, src).is_err() {
            return Err(IpcError::SourceEmpty);
        }
        let Some(slot) = slot else {
            return Err(IpcError::NoReceiveSlot);
        };
        if !caps.is_empty(r, slot) {
            return Err(IpcError::ReceiveSlotFull);
        }
        Ok(())
    }

    /// Complete a rendezvous of sender `s` and receiver `r` (transfer already
    /// checked): deliver the message and capability, then settle the sender.
    /// A `Call` sender moves to awaiting its reply and fills `r`'s reply slot;
    /// any other sender is done. Returns whether the sender may run (it then
    /// has its result).
    fn complete<C: CapAccess>(
        &mut self,
        caps: &mut C,
        p: Pending,
        s: ThreadId,
        r: ThreadId,
        slot: Option<CapId>,
    ) -> bool {
        self.deliver(caps, p, s, r, slot);
        match p.call {
            Some(reply_slot) => {
                self.t[s.0].state = State::AwaitingReply { slot: reply_slot };
                self.t[r.0].reply_to = Some(s);
                false
            }
            None => {
                self.t[s.0].state = State::Idle;
                self.t[s.0].result = Some(Ok(None));
                true
            }
        }
    }

    /// Deliver `p` from `s` to `r`: only `length` registers, the rest zero,
    /// and the capability (already checked) copied into `r`'s `slot`.
    fn deliver<C: CapAccess>(
        &mut self,
        caps: &mut C,
        p: Pending,
        s: ThreadId,
        r: ThreadId,
        slot: Option<CapId>,
    ) {
        let mut arrived = 0;
        if let (Some(src), Some(dst)) = (p.cap, slot) {
            let cap = caps.get(s, src).expect("ipc: transfer source checked");
            caps.insert_at(r, dst, cap)
                .expect("ipc: transfer destination checked");
            arrived = 1;
        }
        let len = usize::from(p.msg.info.length());
        let mut mr = [0; MSG_REGISTERS];
        mr[..len].copy_from_slice(&p.msg.mr[..len]);
        let info = MessageInfo::new(p.msg.info.label(), p.msg.info.length(), arrived);
        self.t[r.0].state = State::Idle;
        self.t[r.0].result = Some(Ok(Some(Received {
            info,
            mr,
            badge: p.badge,
        })));
    }

    /// End `me`'s operation with `e`: it keeps running and nothing changes.
    fn fail(&mut self, me: ThreadId, e: IpcError) -> Outcome {
        self.t[me.0].result = Some(Err(e));
        Outcome::new(Effect::Done)
    }

    /// Append `t` to `ep`'s queue, in `state`.
    fn enqueue(&mut self, ep: &mut EndpointQueue, t: ThreadId, kind: QueueKind, state: State) {
        debug_assert!(ep.kind == QueueKind::Empty || ep.kind == kind);
        self.t[t.0].state = state;
        self.t[t.0].next = None;
        match ep.tail {
            Some(tail) => self.t[tail.0].next = Some(t),
            None => ep.head = Some(t),
        }
        ep.tail = Some(t);
        ep.kind = kind;
    }

    /// Remove and return the head of `ep`'s queue.
    fn dequeue(&mut self, ep: &mut EndpointQueue) -> Option<ThreadId> {
        let head = ep.head?;
        ep.head = self.t[head.0].next.take();
        if ep.head.is_none() {
            ep.tail = None;
            ep.kind = QueueKind::Empty;
        }
        Some(head)
    }
}
