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

    selftest();
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

/// One badge's partial line. `in_use` marks the buffer taken: any badge,
/// including 0, may own one (#129).
struct Line {
    in_use: bool,
    badge: u64,
    len: usize,
    buf: [u8; LINE_BYTES],
}

impl Line {
    const EMPTY: Self = Self {
        in_use: false,
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
            self.in_use = false; // a finished line frees its buffer
            return;
        }
        if self.len == LINE_BYTES {
            self.flush();
        }
        self.buf[self.len] = b;
        self.len += 1;
    }
}

/// Bytes shown as UTF-8, with U+FFFD for each invalid sequence, and every
/// control character escaped: C0 and DEL as `\xNN`, C1 as `\u{NN}`. A `\r` or
/// an escape sequence would otherwise let a task overwrite the `console[N]:`
/// prefix on a terminal and pass its text off as a kernel line (#128).
struct Lossy<'a>(&'a [u8]);

impl fmt::Display for Lossy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            for c in chunk.valid().chars() {
                match c {
                    '\0'..='\x1f' | '\x7f' => write!(f, "\\x{:02x}", c as u32)?,
                    '\u{80}'..='\u{9f}' => write!(f, "\\u{{{:x}}}", c as u32)?,
                    _ => fmt::Write::write_char(f, c)?,
                }
            }
            if !chunk.invalid().is_empty() {
                f.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

/// The line buffer for `badge`: its own, else a free one, else the last,
/// flushed and taken over. Marks it in use by `badge`.
fn claim(lines: &mut [Line; LINES], badge: u64) -> usize {
    let i = lines
        .iter()
        .position(|l| l.in_use && l.badge == badge)
        .or_else(|| lines.iter().position(|l| !l.in_use))
        .unwrap_or(LINES - 1);
    let line = &mut lines[i];
    if !(line.in_use && line.badge == badge) {
        if line.len > 0 {
            line.flush();
        }
        line.badge = badge;
        line.in_use = true;
    }
    i
}

/// A fixed buffer to format into, for the self-test.
struct Fixed {
    buf: [u8; 128],
    len: usize,
}

impl fmt::Write for Fixed {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.buf
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Check, at boot, that a task cannot forge log lines (#128) and that a
/// badge-0 sender's line is its own (#129).
fn selftest() {
    use fmt::Write as _;

    let mut out = Fixed {
        buf: [0; 128],
        len: 0,
    };
    let forged = b"x\r[INFO] \x1b[31mred\x7f \xc2\x9b\xff";
    write!(out, "{}", Lossy(forged)).expect("console: self-test buffer");
    assert_eq!(
        &out.buf[..out.len],
        "x\\x0d[INFO] \\x1b[31mred\\x7f \\u{9b}\u{fffd}".as_bytes(),
        "console: control characters reach the log"
    );

    let mut lines = [Line::EMPTY; LINES];
    let zero = claim(&mut lines, 0);
    lines[zero].push(b'a'); // a partial line from badge 0
    assert_ne!(
        claim(&mut lines, 5),
        zero,
        "console: badge 5 took badge 0's line"
    );
    assert_eq!(claim(&mut lines, 0), zero, "console: badge 0 lost its line");
    assert_eq!(
        lines[zero].len, 1,
        "console: badge 0's partial line was lost"
    );
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
                let i = claim(&mut lines, r.badge);
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
