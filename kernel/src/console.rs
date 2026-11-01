//! The console server (#82, [ADR 0019](../../docs/adr/0019-first-user-task.md)
//! decision 8).
//!
//! The kernel never prints on a task's behalf. A task holding a capability to
//! the console endpoint `Call`s it with up to 32 bytes per message (label =
//! byte count, the message registers carry the bytes little-endian); this
//! kernel thread receives them, buffers a line per badge, and logs each line
//! with the badge that sent it. Any input is safe: a label above 32 is
//! clamped, bytes that are not UTF-8 become U+FFFD, and a line longer than the
//! buffer is flushed in pieces.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use ::ipc::{Message, Received};
use capability::{
    cnode_bytes, slot_align, CapId, Capability, MessageInfo, ObjectKind, ObjectRef, Rights,
    Untyped, MSG_REGISTERS,
};
use sched::ThreadId;

use crate::thread::CONTEXTS;
use crate::{ipc, SCHED};

/// Bytes one message can carry.
const MAX_BYTES: usize = 8 * MSG_REGISTERS;
/// Bytes buffered per badge before a line is flushed anyway.
const LINE_BYTES: usize = 128;
/// Badges with their own line buffer; others share the last one.
const LINES: usize = 4;
/// The server's endpoint slot.
const EP: CapId = CapId(1);
/// Stack for the server thread (it formats log lines).
const STACK_BYTES: usize = 16 * 1024;

/// The line the root task sends when every one of its checks held.
const ROOT_PASSED_LINE: &[u8] = b"root: confinement checks passed";
/// The root task's console badge.
pub const ROOT_BADGE: u64 = 1;

/// Set when the root task reports that all its checks passed.
pub static ROOT_PASSED: AtomicBool = AtomicBool::new(false);

/// Create the console endpoint and start the server as kernel thread `t`.
/// Returns the endpoint, for the capabilities handed to tasks.
pub fn start(t: ThreadId) -> ObjectRef {
    use alloc::boxed::Box;

    let hhdm = ipc::hhdm();
    let frame = hal::frame::alloc_frame().expect("console: no frame");
    let mut ut = Untyped::new(frame.addr(), hal::paging::PAGE);
    let (esz, eal) = ObjectKind::Endpoint.fixed_layout().unwrap();
    let slots = 4;
    let cnode = hal::object::retype(
        &mut ut,
        cnode_bytes(slots) as u64,
        slot_align() as u64,
        1,
        hhdm,
    )
    .expect("console: CNode retype");
    let ep = hal::object::retype(&mut ut, esz, eal, 1, hhdm).expect("console: endpoint retype");
    let ep = ObjectRef::new(ObjectKind::Endpoint, ep);

    // `cnode` was just retyped for `slots` slots and gets exactly one CapSpace.
    let mut space = hal::object::cnode_cap_space(cnode, slots, hhdm);
    space
        .insert_at(EP, Capability::new(ep, Rights::RECV))
        .expect("console: endpoint slot");
    ipc::set_cspace(t, ObjectRef::new(ObjectKind::CNode, cnode), space);

    let stack = Box::leak(alloc::vec![0u8; STACK_BYTES].into_boxed_slice());
    CONTEXTS[t.0].init(stack.as_ptr() as u64 + STACK_BYTES as u64, server);
    hal::cpu::without_interrupts(|| SCHED.lock().admit(t));
    ep
}

/// One badge's partial line.
struct Line {
    badge: u64,
    len: usize,
    buf: [u8; LINE_BYTES],
}

impl Line {
    const EMPTY: Self = Self {
        badge: 0,
        len: 0,
        buf: [0; LINE_BYTES],
    };

    /// Log the buffered bytes as one line and empty the buffer.
    fn flush(&mut self) {
        let text = &self.buf[..self.len];
        crate::info!("console[{}]: {}", self.badge, Lossy(text));
        if self.badge == ROOT_BADGE && text == ROOT_PASSED_LINE {
            ROOT_PASSED.store(true, Ordering::Release);
        }
        self.len = 0;
    }

    fn push(&mut self, b: u8) {
        if b == b'\n' {
            self.flush();
            return;
        }
        if self.len == LINE_BYTES {
            self.flush();
        }
        self.buf[self.len] = b;
        self.len += 1;
    }
}

/// Bytes shown as UTF-8, with U+FFFD for each invalid sequence.
struct Lossy<'a>(&'a [u8]);

impl fmt::Display for Lossy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

/// The bytes a console message carries: the first `min(label, 32)` bytes of
/// its message registers.
fn bytes(r: &Received) -> ([u8; MAX_BYTES], usize) {
    let mut out = [0u8; MAX_BYTES];
    for (i, word) in r.mr.iter().enumerate() {
        out[8 * i..8 * i + 8].copy_from_slice(&word.to_le_bytes());
    }
    let n = (r.info.label() as usize).min(MAX_BYTES);
    (out, n)
}

extern "C" fn server() -> ! {
    let mut lines = [Line::EMPTY; LINES];
    let mut next = ipc::recv(EP, None);
    loop {
        match next {
            Ok(r) => {
                let i = lines
                    .iter()
                    .position(|l| l.badge == r.badge || l.badge == 0)
                    .unwrap_or(LINES - 1);
                if lines[i].badge != r.badge {
                    if lines[i].len > 0 {
                        lines[i].flush();
                    }
                    lines[i].badge = r.badge;
                }
                let (buf, n) = bytes(&r);
                for &b in &buf[..n] {
                    lines[i].push(b);
                }
            }
            Err(e) => crate::warn!("console: receive failed: {:?}", e),
        }
        // Acknowledge the write (an empty reply) and wait for the next one.
        let ack = Message {
            info: MessageInfo::new(0, 0, 0),
            mr: [0; MSG_REGISTERS],
        };
        next = ipc::reply_recv(EP, ack, None, None);
    }
}
