//! Kernel side of synchronous IPC (#81,
//! [ADR 0018](../../docs/adr/0018-synchronous-ipc.md)).
//!
//! The pure `ipc` crate decides who meets whom and what moves; this module
//! loads and stores endpoint queues through the HHDM, keeps each thread's
//! capability space, applies the engine's verdict to the scheduler (block the
//! caller, make woken threads ready) and switches threads.
//!
//! Every operation runs with interrupts off. Locks are taken in the fixed order
//! `IPC`, `CSPACES`, `SCHED`, and all are dropped before a context switch (the
//! deadlock rules in `docs/architecture/interrupts.md`). The boot thread never
//! blocks on IPC, so the ready queue never empties.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ::ipc::{
    CapAccess, Effect, EndpointQueue, Ipc, IpcError, Message, NotificationQueue, Outcome, Received,
};
use boot_info::BootInfo;
use capability::{
    cnode_bytes, mdb, slot_align, CapError, CapId, Capability, MessageInfo, ObjectKind, ObjectRef,
    Rights,
};
use hal::sync::SpinLock;
use sched::ThreadId;

use crate::cspace::{self, Registry, CSPACES};
use crate::thread::{self, CONTEXTS};
use crate::SCHED;

/// Every thread's IPC state.
static IPC: SpinLock<Ipc> = SpinLock::new(Ipc::new());

/// The HHDM offset, for reaching endpoint state.
static HHDM: AtomicU64 = AtomicU64::new(0);

/// Record the HHDM offset. Call once during init, before any IPC.
pub fn init(hhdm: u64) {
    HHDM.store(hhdm, Ordering::Relaxed);
}

/// The HHDM offset recorded by [`init`].
pub fn hhdm() -> u64 {
    HHDM.load(Ordering::Relaxed)
}

/// Send `msg` through the endpoint capability in slot `ep`, with the
/// capability in `cap` if `msg.info.caps()` is 1. Blocks until received.
pub fn send(ep: CapId, msg: Message, cap: Option<CapId>) -> Result<(), IpcError> {
    invoke(Some(ep), |ipc, caps, q, me, rights, badge| {
        ipc.send(caps, q, me, rights, badge, msg, cap)
    })
    .map(|_| ())
}

/// Send like [`send`], then block for the reply, which is returned. A
/// capability in the reply lands in `reply_slot`; naming none refuses one.
pub fn call(
    ep: CapId,
    msg: Message,
    cap: Option<CapId>,
    reply_slot: Option<CapId>,
) -> Result<Received, IpcError> {
    invoke(Some(ep), |ipc, caps, q, me, rights, badge| {
        ipc.call(caps, q, me, rights, badge, msg, cap, reply_slot)
    })
    .map(|r| r.expect("ipc: a completed call carries the reply"))
}

/// Block until a message arrives through the endpoint capability in slot `ep`.
/// An incoming capability lands in `recv_slot`, which must be empty.
pub fn recv(ep: CapId, recv_slot: Option<CapId>) -> Result<Received, IpcError> {
    invoke(Some(ep), |ipc, caps, q, me, rights, _| {
        ipc.recv(caps, q, me, rights, recv_slot)
    })
    .map(|r| r.expect("ipc: a completed receive carries the message"))
}

/// Answer the caller in this thread's reply slot (if it holds one), then block
/// for the next message through `ep`, as one operation: a server's loop.
pub fn reply_recv(
    ep: CapId,
    msg: Message,
    cap: Option<CapId>,
    recv_slot: Option<CapId>,
) -> Result<Received, IpcError> {
    invoke(Some(ep), |ipc, caps, q, me, rights, _| {
        ipc.reply_recv(caps, q, me, rights, msg, cap, recv_slot)
    })
    .map(|r| r.expect("ipc: a completed receive carries the message"))
}

/// Answer the caller in this thread's reply slot.
pub fn reply(msg: Message, cap: Option<CapId>) -> Result<(), IpcError> {
    invoke(None, |ipc, caps, _, me, _, _| ipc.reply(caps, me, msg, cap)).map(|_| ())
}

/// Clear exiting thread `t`'s IPC state; returns the threads to make ready (a
/// caller that was waiting on `t` for a reply, now failed with `PeerExited`).
/// Call with interrupts off.
pub fn exit_thread(t: ThreadId) -> Outcome {
    IPC.lock().exit(t)
}

/// Run `f` on the IPC state and the CNode registry, with interrupts off and
/// `IPC` then `CSPACES` held: the locks for any capability operation that
/// may destroy an object and so wake threads blocked on it (ADR 0020).
pub fn with_caps<R>(f: impl FnOnce(&mut Ipc, &mut Registry) -> R) -> R {
    hal::cpu::without_interrupts(|| {
        let mut ipc = IPC.lock();
        let mut reg = CSPACES.lock();
        f(&mut ipc, &mut reg)
    })
}

/// A capability was deleted: if it was the last to its object, destroy the
/// object (ADR 0020). An Endpoint or a Notification wakes every thread queued
/// on it with `Revoked`. Nothing else holds kernel state to tear down yet:
/// CNode, Untyped, Frame and Thread memory is reclaimed by the task reap.
///
/// Call with `IPC` and `CSPACES` held (see [`with_caps`]); takes `SCHED`.
pub fn destroy(ipc: &mut Ipc, deleted: &mdb::Deleted<ObjectRef>) {
    let obj = deleted.cap.object;
    if !deleted.last {
        return;
    }
    let outcome = match obj.kind {
        ObjectKind::Endpoint => on_endpoint(obj, |q| ipc.destroy_endpoint(q)),
        ObjectKind::Notification => on_notification(obj, |n| ipc.destroy_notification(n)),
        _ => return,
    };
    wake(&outcome);
}

/// Signal Notification `n` with `bits`, waking its longest waiter if any bit
/// is then pending (ADR 0022). Call with `IPC` held; takes `SCHED`.
pub fn signal(ipc: &mut Ipc, n: ObjectRef, bits: u64) {
    let outcome = on_notification(n, |q| ipc.signal(q, bits));
    wake(&outcome);
}

/// Run `f` on endpoint `ep`'s queue, loaded and stored back through the HHDM.
fn on_endpoint(ep: ObjectRef, f: impl FnOnce(&mut EndpointQueue) -> Outcome) -> Outcome {
    let hhdm = hhdm();
    let mut state = hal::object::load_endpoint(ep, hhdm);
    let mut queue = EndpointQueue::load(&state);
    let outcome = f(&mut queue);
    queue.store(&mut state);
    hal::object::store_endpoint(ep, hhdm, state);
    outcome
}

/// Run `f` on Notification `n`'s state, loaded and stored back through the
/// HHDM.
fn on_notification(n: ObjectRef, f: impl FnOnce(&mut NotificationQueue) -> Outcome) -> Outcome {
    let hhdm = hhdm();
    let mut state = hal::object::load_notification(n, hhdm);
    let mut queue = NotificationQueue::load(&state);
    let outcome = f(&mut queue);
    queue.store(&mut state);
    hal::object::store_notification(n, hhdm, state);
    outcome
}

/// Make the threads an operation woke ready. Takes `SCHED`.
fn wake(outcome: &Outcome) {
    let mut sched = SCHED.lock();
    for t in outcome.woken() {
        assert!(sched.admit(t), "ipc: a woken thread already runnable");
    }
}

/// The thread at the head of the queue of `obj`, an Endpoint or a
/// Notification, if any.
pub fn waiting_on(obj: ObjectRef) -> Option<ThreadId> {
    with_caps(|_, _| match obj.kind {
        ObjectKind::Notification => {
            NotificationQueue::load(&hal::object::load_notification(obj, hhdm()))
                .waiters
                .head
        }
        _ => EndpointQueue::load(&hal::object::load_endpoint(obj, hhdm())).head,
    })
}

/// Wait on the Notification capability in slot `n`: take its pending bits,
/// blocking until a signal if there are none (ADR 0022).
pub fn wait(n: CapId) -> Result<u64, IpcError> {
    hal::cpu::without_interrupts(|| {
        let me = SCHED.lock().current().expect("ipc: a running thread");
        let outcome = with_caps(|ipc, r| {
            let cap = r.lookup(me, n).map_err(|_| IpcError::InvalidCap)?;
            if cap.object.kind != ObjectKind::Notification {
                return Err(IpcError::InvalidCap);
            }
            Ok(on_notification(cap.object, |q| ipc.wait(q, me, cap.rights)))
        })?;
        finish(me, &outcome)
    })
    .map(|r| r.expect("ipc: a completed wait carries the bits").badge)
}

/// The capability spaces as the engine sees them.
struct Spaces<'a>(&'a mut Registry);

impl CapAccess for Spaces<'_> {
    type Obj = ObjectRef;

    fn get(&mut self, t: ThreadId, slot: CapId) -> Result<Capability<ObjectRef>, CapError> {
        self.0.lookup(t, slot)
    }

    fn is_empty(&mut self, t: ThreadId, slot: CapId) -> bool {
        self.get(t, slot) == Err(CapError::Empty)
    }

    fn transfer(
        &mut self,
        s: ThreadId,
        src: CapId,
        r: ThreadId,
        dst: CapId,
    ) -> Result<(), CapError> {
        let from = self.0.slot(s, src)?;
        let to = self.0.slot(r, dst)?;
        let rights = self.0.cap(from)?.rights;
        cspace::copy(self.0, from, to, rights)
    }
}

/// Run one IPC operation for the current thread. `ep` names the endpoint
/// capability (`None` for `Reply`); `op` runs the engine with the endpoint's
/// queue and the capability's rights and badge. Applies the outcome, blocking
/// if told to, and returns this thread's result once it has one.
fn invoke(
    ep: Option<CapId>,
    op: impl FnOnce(&mut Ipc, &mut Spaces<'_>, &mut EndpointQueue, ThreadId, Rights, u64) -> Outcome,
) -> Result<Option<Received>, IpcError> {
    hal::cpu::without_interrupts(|| {
        let me = SCHED.lock().current().expect("ipc: a running thread");
        let hhdm = HHDM.load(Ordering::Relaxed);
        let outcome = {
            let mut ipc = IPC.lock();
            let mut spaces = CSPACES.lock();
            let mut caps = Spaces(&mut spaces);
            let (obj, rights, badge) = match ep {
                Some(slot) => {
                    let cap = caps.get(me, slot).map_err(|_| IpcError::InvalidCap)?;
                    if cap.object.kind != ObjectKind::Endpoint {
                        return Err(IpcError::InvalidCap);
                    }
                    (Some(cap.object), cap.rights, cap.badge)
                }
                None => (None, Rights::NONE, 0),
            };
            let mut state = obj.map(|o| hal::object::load_endpoint(o, hhdm));
            let mut queue = state
                .as_ref()
                .map_or(EndpointQueue::new(), EndpointQueue::load);
            let outcome = op(&mut ipc, &mut caps, &mut queue, me, rights, badge);
            if let (Some(o), Some(s)) = (obj, state.as_mut()) {
                queue.store(s);
                hal::object::store_endpoint(o, hhdm, *s);
            }
            outcome
        };
        finish(me, &outcome)
    })
}

/// Apply an operation's `outcome` for the current thread `me`: make the
/// woken threads ready, block and switch away if told to, and return `me`'s
/// result once it has one. Call with interrupts off and no lock held.
fn finish(me: ThreadId, outcome: &Outcome) -> Result<Option<Received>, IpcError> {
    let next = {
        let mut sched = SCHED.lock();
        for t in outcome.woken() {
            assert!(sched.admit(t), "ipc: woken thread already runnable");
        }
        if outcome.caller == Effect::Block {
            // The boot thread is the idle thread: it never blocks, so the
            // run queue is never empty (#107, ADR 0018).
            assert_ne!(
                me,
                ThreadId(0),
                "ipc: the boot (idle) thread must never block"
            );
            sched.remove(me);
            let next = sched.current();
            Some(next.expect("ipc: every thread blocked (the boot thread never blocks on IPC)"))
        } else {
            None
        }
    };
    if let Some(next) = next {
        thread::switch_to(me, next);
    }
    IPC.lock()
        .take_result(me)
        .expect("ipc: a running thread has its result")
}

// ---------------------------------------------------------------- demo (#81)

/// The demo's server and client threads.
const SERVER: ThreadId = ThreadId(5);
const CLIENT: ThreadId = ThreadId(6);
/// Demo threads that have finished.
static IPC_DONE: AtomicU64 = AtomicU64::new(0);
/// The server slot holding the demo endpoint's original capability.
const ORIGIN: CapId = CapId(15);
/// The server slot holding its Notification.
const NOTE: CapId = CapId(7);
/// Stack size for each demo thread (they format log lines).
const STACK_BYTES: usize = 16 * 1024;

/// Prove IPC between two kernel threads with their own capability spaces: a
/// call carrying a capability and answered by a reply, the badge seen by the
/// server, a send without `GRANT` refused, and a plain send through that same
/// capability delivered.
pub fn demo(boot_info: &BootInfo) {
    use alloc::boxed::Box;
    use capability::Untyped;

    const SLOTS: usize = 16;
    let hhdm = boot_info.hhdm_offset;

    let (Some(objects), Some(page)) = (hal::frame::alloc_frame(), hal::frame::alloc_frame()) else {
        panic!("ipc: no frames for the demo");
    };
    let mut ut = Untyped::new(objects.addr(), hal::paging::PAGE);
    let mut frames = Untyped::new(page.addr(), hal::paging::PAGE);
    let (esz, eal) = ObjectKind::Endpoint.fixed_layout().unwrap();
    let (fsz, fal) = ObjectKind::Frame.fixed_layout().unwrap();
    let cnode = (cnode_bytes(SLOTS) as u64, slot_align() as u64);
    let (nsz, nal) = ObjectKind::Notification.fixed_layout().unwrap();
    let (Some(srv), Some(cli), Some(ep), Some(note), Some(frame)) = (
        hal::object::retype(&mut ut, cnode.0, cnode.1, 1, hhdm),
        hal::object::retype(&mut ut, cnode.0, cnode.1, 1, hhdm),
        hal::object::retype(&mut ut, esz, eal, 1, hhdm),
        hal::object::retype(&mut ut, nsz, nal, 1, hhdm),
        hal::object::retype(&mut frames, fsz, fal, 1, hhdm),
    ) else {
        panic!("ipc: demo retype failed");
    };

    // Each CNode was just retyped for SLOTS slots; its space is kept for the
    // life of the system.
    let (srv, cli) = (
        ObjectRef::new(ObjectKind::CNode, srv),
        ObjectRef::new(ObjectKind::CNode, cli),
    );
    let server = hal::object::cnode_cap_space(srv, SLOTS, hhdm);
    let client = hal::object::cnode_cap_space(cli, SLOTS, hhdm);
    cspace::install(SERVER, srv, server);
    cspace::install(CLIENT, cli, client);
    // Every capability to the endpoint derives from one original, kept in the
    // server's last slot (ADR 0020): the last-capability check relies on it.
    let ep = ObjectRef::new(ObjectKind::Endpoint, ep);
    let note = ObjectRef::new(ObjectKind::Notification, note);
    let note_at = cspace::insert_root(SERVER, NOTE, Capability::new(note, Rights::ALL))
        .expect("ipc: the server's Notification");
    let origin = cspace::insert_root(SERVER, ORIGIN, Capability::new(ep, Rights::ALL))
        .expect("ipc: the endpoint's original");
    let frame = ObjectRef::new(ObjectKind::Frame, frame);
    let frame_rights = Rights::READ | Rights::WRITE | Rights::MAP;
    let client_frame = cspace::insert_root(CLIENT, CapId(2), Capability::new(frame, frame_rights))
        .expect("ipc: the client's frame");
    cspace::with(|r| {
        let at = |t, id| r.slot(t, id).expect("ipc: a demo slot");
        let (srv0, cli0, cli1) = (
            at(SERVER, CapId(0)),
            at(CLIENT, CapId(0)),
            at(CLIENT, CapId(1)),
        );
        mdb::copy(r, origin, srv0, Rights::RECV)?;
        mdb::mint(r, origin, cli0, Rights::SEND | Rights::GRANT, 7)?;
        mdb::mint(r, origin, cli1, Rights::SEND, 9)
    })
    .expect("ipc: demo capabilities");

    let threads: [(ThreadId, extern "C" fn() -> !); 2] =
        [(SERVER, ipc_server), (CLIENT, ipc_client)];
    for (t, entry) in threads {
        let stack = Box::leak(alloc::vec![0u8; STACK_BYTES].into_boxed_slice());
        CONTEXTS[t.0].init(stack.as_ptr() as u64 + STACK_BYTES as u64, entry);
    }
    hal::cpu::without_interrupts(|| {
        let mut s = SCHED.lock();
        s.admit(SERVER);
        s.admit(CLIENT);
    });

    while IPC_DONE.load(Ordering::Acquire) < 2 {
        crate::yield_now();
    }

    // The refused send moved nothing: the server's second receive slot is
    // still empty, while the frame from the call sits in the first, derived
    // from the client's capability that sent it.
    let (first, second) = cspace::with(|r| {
        let at = r.slot(SERVER, CapId(5)).expect("ipc: a demo slot");
        let parent = r.entry(at).map(|e| (e.link.prev, e.link.depth));
        (
            r.cap(at).map(|c| (c.object, parent)),
            r.lookup(SERVER, CapId(6)),
        )
    });
    assert_eq!(
        first,
        Ok((frame, Some((Some(client_frame), 1)))),
        "ipc: frame not received as a child of the sender's capability"
    );
    assert_eq!(
        second,
        Err(CapError::Empty),
        "ipc: a refused transfer moved a capability"
    );
    crate::info!("ipc: call/reply, badge, capability transfer and GRANT enforcement verified");

    // The server now waits on the endpoint again. Revoking the original takes
    // back every capability derived from it, the server's included; deleting
    // the original then destroys the endpoint, which wakes the server with
    // `Revoked` (ADR 0020).
    while waiting_on(ep) != Some(SERVER) {
        crate::yield_now();
    }
    let derived = [(SERVER, 0), (CLIENT, 0), (CLIENT, 1)];
    with_caps(|ipc, r| {
        while !cspace::revoke(ipc, r, origin).expect("ipc: revoke the original") {}
        for (t, id) in derived {
            assert_eq!(
                r.lookup(t, CapId(id)),
                Err(CapError::Empty),
                "ipc: revoke left a derived endpoint capability"
            );
        }
        assert_eq!(
            IPC_DONE.load(Ordering::Acquire),
            2,
            "ipc: woken before the last capability went"
        );
        cspace::delete(ipc, r, origin).expect("ipc: delete the original");
    });
    while IPC_DONE.load(Ordering::Acquire) < 3 {
        crate::yield_now();
    }
    crate::info!("ipc: revoke took back derived capabilities; a waiter on a destroyed endpoint woke with Revoked");

    // The server now waits on its Notification (ADR 0022): a signal wakes it
    // with the bits; then deleting the only capability wakes it with
    // `Revoked`.
    while waiting_on(note) != Some(SERVER) {
        crate::yield_now();
    }
    with_caps(|ipc, _| signal(ipc, note, 0b110));
    while waiting_on(note) != Some(SERVER) {
        crate::yield_now();
    }
    with_caps(|ipc, r| cspace::delete(ipc, r, note_at)).expect("ipc: delete the Notification");
    while IPC_DONE.load(Ordering::Acquire) < 4 {
        crate::yield_now();
    }
    crate::info!("ipc: a Notification waiter woke with the signalled bits, then with Revoked");
}

extern "C" fn ipc_server() -> ! {
    let r = recv(CapId(0), Some(CapId(5))).expect("ipc: server receive");
    let cap =
        cspace::with(|r| r.lookup(SERVER, CapId(5))).expect("ipc: server received a capability");
    crate::info!(
        "ipc: server got label={} mr={:?} badge={} cap={:?} rights={}",
        r.info.label(),
        r.mr,
        r.badge,
        cap.object.kind,
        RightsList(cap.rights)
    );
    let answer = Message {
        info: MessageInfo::new(1, 1, 0),
        mr: [10, 0, 0, 0],
    };
    // Answer, and wait for the next message, in one step.
    let r = reply_recv(CapId(0), answer, None, Some(CapId(6))).expect("ipc: server reply_recv");
    crate::info!(
        "ipc: server got label={} badge={} caps={}",
        r.info.label(),
        r.badge,
        r.info.caps()
    );
    IPC_DONE.fetch_add(1, Ordering::Release);
    // Wait once more: the demo destroys the endpoint under us.
    let revoked = recv(CapId(0), None);
    assert_eq!(
        revoked,
        Err(IpcError::Revoked),
        "ipc: a destroyed endpoint's waiter"
    );
    IPC_DONE.fetch_add(1, Ordering::Release);
    assert_eq!(wait(NOTE), Ok(0b110), "ipc: the signalled bits");
    assert_eq!(
        wait(NOTE),
        Err(IpcError::Revoked),
        "ipc: a destroyed Notification's waiter"
    );
    IPC_DONE.fetch_add(1, Ordering::Release);
    crate::exit_current()
}

extern "C" fn ipc_client() -> ! {
    let request = Message {
        info: MessageInfo::new(42, 4, 1),
        mr: [1, 2, 3, 4],
    };
    let r = call(CapId(0), request, Some(CapId(2)), None).expect("ipc: client call");
    crate::info!(
        "ipc: client got reply label={} mr0={}",
        r.info.label(),
        r.mr[0]
    );

    let with_cap = Message {
        info: MessageInfo::new(3, 0, 1),
        mr: [0; 4],
    };
    let refused = send(CapId(1), with_cap, Some(CapId(2)));
    assert_eq!(refused, Err(IpcError::NoGrant), "ipc: GRANT not enforced");
    crate::info!("ipc: send through a cap without GRANT failed: NoGrant");

    let plain = Message {
        info: MessageInfo::new(3, 0, 0),
        mr: [0; 4],
    };
    let empty = send(CapId(3), plain, None);
    assert_eq!(
        empty,
        Err(IpcError::InvalidCap),
        "ipc: an empty slot was not InvalidCap"
    );
    crate::info!("ipc: send through an empty slot failed: InvalidCap");
    send(CapId(1), plain, None).expect("ipc: client plain send");
    IPC_DONE.fetch_add(1, Ordering::Release);
    crate::exit_current()
}

/// Rights as `READ|WRITE|MAP`, for the log.
struct RightsList(Rights);

impl fmt::Display for RightsList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [(Rights, &str); 8] = [
            (Rights::READ, "READ"),
            (Rights::WRITE, "WRITE"),
            (Rights::EXECUTE, "EXECUTE"),
            (Rights::MAP, "MAP"),
            (Rights::SEND, "SEND"),
            (Rights::RECV, "RECV"),
            (Rights::GRANT, "GRANT"),
            (Rights::RETYPE, "RETYPE"),
        ];
        let mut first = true;
        for (r, name) in NAMES {
            if self.0.contains(r) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}
