#![no_std]
#![no_main]
#![forbid(unsafe_code)]

//! The root task (#82, [ADR 0019](../../../docs/adr/0019-first-user-task.md)
//! decision 9): the first user program. It holds four slots (0 null, 1 its
//! CNode, 2 Untyped, 3 the console) and proves it is confined to them,
//! reporting each step through the console server over IPC.

use core::fmt::{self, Write};

use sys::{kind_code, label, MessageInfo, ObjectKind, SysError, CONSOLE, UNTYPED};

/// The slot root retypes a Notification into.
const NOTE: u64 = 4;

/// A line built on the stack, then written to the console.
struct Line {
    buf: [u8; 128],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

fn say(args: fmt::Arguments) {
    let mut line = Line {
        buf: [0; 128],
        len: 0,
    };
    let _ = line.write_fmt(args);
    let _ = line.write_str("\n");
    let _ = sys::write(CONSOLE, &line.buf[..line.len]);
}

/// Log `what` with its outcome, and whether it matched `want`.
fn check<T: fmt::Debug>(what: &str, got: Result<T, SysError>, want: Result<(), SysError>) -> bool {
    let ok = got.as_ref().map(|_| ()).map_err(|e| *e) == want;
    match &got {
        Ok(_) => say(format_args!("root: {what}: ok")),
        Err(e) => say(format_args!("root: {what}: {e:?}")),
    }
    if !ok {
        say(format_args!("root: {what}: expected {want:?}"));
    }
    ok
}

fn main() -> ! {
    say(format_args!(
        "root: hello from ring 3; slots 0 null, 1 CNode, 2 Untyped 64 KiB, 3 console"
    ));
    let mut ok = true;

    // A kernel object it may create and use: a Notification from its Untyped.
    let note = kind_code(ObjectKind::Notification);
    let retype = sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[note, 1, NOTE]);
    ok &= check("retype a Notification into slot 4", retype, Ok(()));
    let signal = sys::invoke(NOTE, label::NOTIFICATION_SIGNAL, &[0b101]);
    ok &= check("signal it", signal, Ok(()));

    // Slots it does not hold: the null slot, an empty one, one past its CNode.
    for (slot, what) in [
        (0, "invoke slot 0"),
        (5, "invoke empty slot 5"),
        (64, "invoke slot 64"),
    ] {
        let r = sys::invoke(slot, label::NOTIFICATION_SIGNAL, &[1]);
        ok &= check(what, r, Err(SysError::InvalidCap));
    }

    // Its console capability has no GRANT: it cannot pass capabilities on.
    let with_cap = MessageInfo::new(0, 0, 1);
    let grant = sys::send(CONSOLE, with_cap, [0; 4], NOTE);
    ok &= check(
        "send a capability to the console",
        grant,
        Err(SysError::NoGrant),
    );

    // Malformed console messages are clamped and repaired, never fatal: a
    // label of 200 (32 bytes are taken) and a byte that is not UTF-8.
    let mut words = [0u64; 4];
    for (i, b) in b"root: label 200 clamped to 32 b\n".iter().enumerate() {
        words[i / 8] |= u64::from(*b) << (8 * (i % 8));
    }
    let long = sys::call(CONSOLE, MessageInfo::new(200, 4, 0), words, sys::NO_SLOT);
    ok &= check("console message with label 200", long, Ok(()));
    let bad = sys::write(CONSOLE, b"root: invalid byte \xff replaced\n");
    ok &= check("console message with an invalid byte", bad, Ok(()));

    if ok {
        say(format_args!("root: confinement checks passed"));
    }
    sys::exit()
}

sys::entry!(main);
