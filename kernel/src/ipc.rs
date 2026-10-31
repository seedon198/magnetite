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

use ::ipc::{CapAccess, Effect, EndpointQueue, Ipc, IpcError, Message, Outcome, Received};
use boot_info::BootInfo;
use capability::{
    cnode_bytes, slot_align, CapError, CapId, CapSpace, Capability, MessageInfo, ObjectKind,
    ObjectRef, Rights,
};
use hal::sync::SpinLock;
use sched::ThreadId;

use crate::thread::{self, CONTEXTS};
use crate::SCHED;

/// Every thread's IPC state.
static IPC: SpinLock<Ipc> = SpinLock::new(Ipc::new());

/// Each thread's capability space, formed once over its CNode. Never re-form a
/// live one: `hal::object::cnode_cap_space` clears the CNode it wraps.
static CSPACES: SpinLock<[Option<CapSpace<'static, ObjectRef>>; ::ipc::MAX_THREADS]> =
    SpinLock::new([const { None }; ::ipc::MAX_THREADS]);

/// The HHDM offset, for reaching endpoint state.
static HHDM: AtomicU64 = AtomicU64::new(0);

/// Give thread `t` its capability space.
pub fn set_cspace(t: ThreadId, space: CapSpace<'static, ObjectRef>) {
    hal::cpu::without_interrupts(|| CSPACES.lock()[t.0] = Some(space));
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

/// The capability spaces as the engine sees them.
struct Spaces<'a>(&'a mut [Option<CapSpace<'static, ObjectRef>>; ::ipc::MAX_THREADS]);

impl CapAccess for Spaces<'_> {
    type Obj = ObjectRef;

    fn get(&mut self, t: ThreadId, slot: CapId) -> Result<Capability<ObjectRef>, CapError> {
        self.0[t.0].as_ref().ok_or(CapError::OutOfRange)?.get(slot)
    }

    fn is_empty(&mut self, t: ThreadId, slot: CapId) -> bool {
        self.get(t, slot) == Err(CapError::Empty)
    }

    fn insert_at(
        &mut self,
        t: ThreadId,
        slot: CapId,
        cap: Capability<ObjectRef>,
    ) -> Result<(), CapError> {
        self.0[t.0]
            .as_mut()
            .ok_or(CapError::OutOfRange)?
            .insert_at(slot, cap)
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
            let (phys, rights, badge) = match ep {
                Some(slot) => {
                    let cap = caps.get(me, slot).map_err(|_| IpcError::MissingRight)?;
                    if cap.object.kind != ObjectKind::Endpoint {
                        return Err(IpcError::MissingRight);
                    }
                    (Some(cap.object.phys), cap.rights, cap.badge)
                }
                None => (None, Rights::NONE, 0),
            };
            let mut state = phys.map(|p| hal::object::load_endpoint(p, hhdm));
            let mut queue = state
                .as_ref()
                .map_or(EndpointQueue::new(), EndpointQueue::load);
            let outcome = op(&mut ipc, &mut caps, &mut queue, me, rights, badge);
            if let (Some(p), Some(s)) = (phys, state.as_mut()) {
                queue.store(s);
                hal::object::store_endpoint(p, hhdm, *s);
            }
            outcome
        };

        let next = {
            let mut sched = SCHED.lock();
            for t in outcome.woken() {
                assert!(sched.admit(t), "ipc: woken thread already runnable");
            }
            if outcome.caller == Effect::Block {
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
    })
}

// ---------------------------------------------------------------- demo (#81)

/// The demo's server and client threads.
const SERVER: ThreadId = ThreadId(5);
const CLIENT: ThreadId = ThreadId(6);
/// Demo threads that have finished.
static IPC_DONE: AtomicU64 = AtomicU64::new(0);
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
    HHDM.store(hhdm, Ordering::Relaxed);

    let (Some(objects), Some(page)) = (hal::frame::alloc_frame(), hal::frame::alloc_frame()) else {
        crate::warn!("ipc: no frames for the demo");
        return;
    };
    let mut ut = Untyped::new(objects.addr(), hal::paging::PAGE);
    let mut frames = Untyped::new(page.addr(), hal::paging::PAGE);
    let (esz, eal) = ObjectKind::Endpoint.fixed_layout().unwrap();
    let (fsz, fal) = ObjectKind::Frame.fixed_layout().unwrap();
    let cnode = (cnode_bytes(SLOTS) as u64, slot_align() as u64);
    let (Some(srv), Some(cli), Some(ep), Some(frame)) = (
        hal::object::retype(&mut ut, cnode.0, cnode.1, 1, hhdm),
        hal::object::retype(&mut ut, cnode.0, cnode.1, 1, hhdm),
        hal::object::retype(&mut ut, esz, eal, 1, hhdm),
        hal::object::retype(&mut frames, fsz, fal, 1, hhdm),
    ) else {
        crate::warn!("ipc: demo retype failed");
        return;
    };

    // Each CNode was just retyped for SLOTS slots, and each gets exactly one
    // CapSpace, kept for the life of the system.
    let mut server = hal::object::cnode_cap_space(srv, SLOTS, hhdm);
    let mut client = hal::object::cnode_cap_space(cli, SLOTS, hhdm);
    let ep = ObjectRef::new(ObjectKind::Endpoint, ep);
    let badged = |rights, badge| Capability {
        object: ep,
        rights,
        badge,
    };
    server.insert_at(CapId(0), badged(Rights::RECV, 0)).unwrap();
    client
        .insert_at(CapId(0), badged(Rights::SEND | Rights::GRANT, 7))
        .unwrap();
    client.insert_at(CapId(1), badged(Rights::SEND, 9)).unwrap();
    let frame = ObjectRef::new(ObjectKind::Frame, frame);
    let frame_rights = Rights::READ | Rights::WRITE | Rights::MAP;
    client
        .insert_at(CapId(2), Capability::new(frame, frame_rights))
        .unwrap();
    set_cspace(SERVER, server);
    set_cspace(CLIENT, client);

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
    // still empty, while the frame from the call sits in the first.
    let (first, second) = hal::cpu::without_interrupts(|| {
        let spaces = CSPACES.lock();
        let s = spaces[SERVER.0].as_ref().unwrap();
        (s.get(CapId(5)), s.get(CapId(6)))
    });
    assert!(
        first.is_ok_and(|c| c.object == frame),
        "ipc: frame not received"
    );
    assert_eq!(
        second,
        Err(CapError::Empty),
        "ipc: a refused transfer moved a capability"
    );
    crate::info!("ipc: call/reply, badge, capability transfer and GRANT enforcement verified");
}

extern "C" fn ipc_server() -> ! {
    let r = recv(CapId(0), Some(CapId(5))).expect("ipc: server receive");
    let cap =
        hal::cpu::without_interrupts(|| CSPACES.lock()[SERVER.0].as_ref().unwrap().get(CapId(5)))
            .expect("ipc: server received a capability");
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
    reply(answer, None).expect("ipc: server reply");

    let r = recv(CapId(0), Some(CapId(6))).expect("ipc: server second receive");
    crate::info!(
        "ipc: server got label={} badge={} caps={}",
        r.info.label(),
        r.badge,
        r.info.caps()
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
