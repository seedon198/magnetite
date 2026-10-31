//! The IPC engine: per-thread IPC state, endpoint queues and the rendezvous.
//!
//! Every operation is a pure state transition. It returns an [`Outcome`]
//! saying whether the calling thread keeps running or blocks, and which
//! threads become ready again. The kernel applies that to the scheduler. A
//! thread reads the result of its operation with [`Ipc::take_result`]: at once
//! if it kept running, or when it resumes after being woken.

use capability::{CapError, CapId, Capability, MessageInfo, Rights, MSG_REGISTERS};
use sched::ThreadId;

use crate::{EndpointQueue, IpcError, Message, QueueKind, Received};

/// How many threads the engine tracks: one slot per schedulable thread.
pub const MAX_THREADS: usize = sched::MAX_THREADS;

/// The kernel's access to the threads' capability spaces, which the engine
/// needs only to move a capability with a message.
pub trait CapAccess {
    /// The kernel's object handle, carried opaquely.
    type Obj: Copy;
    /// The capability in thread `t`'s `slot`.
    fn get(&mut self, t: ThreadId, slot: CapId) -> Result<Capability<Self::Obj>, CapError>;
    /// Whether thread `t`'s `slot` is empty.
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
/// ready again, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the caller keeps running or blocks.
    pub caller: Effect,
    /// Threads woken by this operation (the first `None` ends the list).
    pub wake: [Option<ThreadId>; 4],
}

impl Outcome {
    fn new(caller: Effect) -> Self {
        Self {
            caller,
            wake: [None; 4],
        }
    }

    fn push(&mut self, t: ThreadId) {
        let slot = self.wake.iter_mut().find(|w| w.is_none());
        *slot.expect("ipc: more wakes than one operation can produce") = Some(t);
    }

    /// The threads to make ready again, in order.
    pub fn woken(&self) -> impl Iterator<Item = ThreadId> + '_ {
        self.wake.iter().map_while(|w| *w)
    }
}

/// What a thread is doing, from IPC's point of view.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Not blocked in IPC.
    Idle,
    /// Queued on an endpoint, waiting for a receiver.
    Sending(Pending),
    /// Queued on an endpoint, waiting for a sender.
    // Read once capability transfer lands (Task 4 of the #81 plan).
    #[allow(dead_code)]
    Receiving { slot: Option<CapId> },
}

/// A message on its way: everything a queued sender needs to complete later.
#[derive(Debug, Clone, Copy)]
struct Pending {
    msg: Message,
    badge: u64,
}

/// One thread's IPC state.
#[derive(Debug, Clone, Copy)]
struct Tcb {
    state: State,
    /// The next thread in the endpoint queue this thread is on.
    next: Option<ThreadId>,
    /// The result of this thread's last operation, until it is taken.
    result: Option<Result<Option<Received>, IpcError>>,
}

impl Tcb {
    const IDLE: Self = Self {
        state: State::Idle,
        next: None,
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
    /// `ep_badge`. Blocks until a receiver takes it.
    // The operations take the invocation's parts as plain arguments, matching
    // the register ABI one to one; bundling them would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub fn send<C: CapAccess>(
        &mut self,
        _caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        ep_badge: u64,
        msg: Message,
        _cap: Option<CapId>,
    ) -> Outcome {
        if !ep_rights.contains(Rights::SEND) {
            return self.fail(me, IpcError::MissingRight);
        }
        let pending = Pending {
            msg,
            badge: ep_badge,
        };
        if ep.kind == QueueKind::Receivers {
            let r = self.dequeue(ep).expect("ipc: receivers queue has a head");
            let mut out = Outcome::new(Effect::Done);
            self.complete(pending, r);
            self.t[me.0].result = Some(Ok(None));
            out.push(r);
            out
        } else {
            self.enqueue(ep, me, QueueKind::Senders, State::Sending(pending));
            Outcome::new(Effect::Block)
        }
    }

    /// Receive through an endpoint capability with `ep_rights`, blocking
    /// until a sender arrives. `recv_slot` is where an incoming capability may
    /// land.
    pub fn recv<C: CapAccess>(
        &mut self,
        _caps: &mut C,
        ep: &mut EndpointQueue,
        me: ThreadId,
        ep_rights: Rights,
        recv_slot: Option<CapId>,
    ) -> Outcome {
        if !ep_rights.contains(Rights::RECV) {
            return self.fail(me, IpcError::MissingRight);
        }
        if ep.kind == QueueKind::Senders {
            let s = self.dequeue(ep).expect("ipc: senders queue has a head");
            let State::Sending(pending) = self.t[s.0].state else {
                unreachable!("ipc: a queued sender is in the Sending state");
            };
            self.t[s.0].state = State::Idle;
            self.t[s.0].result = Some(Ok(None));
            self.complete(pending, me);
            let mut out = Outcome::new(Effect::Done);
            out.push(s);
            out
        } else {
            self.enqueue(
                ep,
                me,
                QueueKind::Receivers,
                State::Receiving { slot: recv_slot },
            );
            Outcome::new(Effect::Block)
        }
    }

    /// Deliver `p` to receiver `r`: only `length` registers, the rest zero.
    fn complete(&mut self, p: Pending, r: ThreadId) {
        let len = usize::from(p.msg.info.length());
        let mut mr = [0; MSG_REGISTERS];
        mr[..len].copy_from_slice(&p.msg.mr[..len]);
        let info = MessageInfo::new(p.msg.info.label(), p.msg.info.length(), 0);
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
