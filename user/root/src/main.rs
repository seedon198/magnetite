#![no_std]
#![no_main]
#![forbid(unsafe_code)]

//! The root task (#82, [ADR 0019](../../../docs/adr/0019-first-user-task.md)
//! decision 9): the first user program. It holds four slots (0 null, 1 its
//! CNode, 2 Untyped, 3 the console) and proves it is confined to them,
//! reporting each step through the console server over IPC.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

use sys::{kind_code, label, MessageInfo, ObjectKind, SysError, CONSOLE, UNTYPED};

/// The slot root retypes a Notification into.
const NOTE: u64 = 4;
/// The slot root retypes an Endpoint into.
const ENDPOINT: u64 = 7;
/// The slots of a child Untyped root splits off, and a frame from it.
const CHILD: u64 = 8;
const CHILD_FRAME: u64 = 9;
/// The slot of the AddressSpace root retypes, and the address the kernel
/// finds the frame copy in slot 16 mapped at.
const SPACE: u64 = 11;
const MAPPED: u64 = 0x4000_1000;
const PAGE: u64 = 4096;
/// The slot of a CNode root retypes.
const KID: u64 = 10;
/// The slot root copies its console capability into, then revokes.
const COPY: u64 = 6;

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

/// An address space of its own making (ADR 0023): tables installed level by
/// level, frames mapped once per capability, W^X, and the copy and mint
/// rules. The kernel inspects the result after root exits: `MAPPED` maps the
/// copy in slot 16, and `MAPPED - PAGE` and `MAPPED + PAGE` are unmapped.
/// Slots for the thread checks: the Thread, a Notification the child
/// signals, one it waits on, its stack frame, and tables for that stack.
const RUN: u64 = 20;
const TOLD: u64 = 22;
const WAITS: u64 = 23;
const STACK: u64 = 24;
const TABLES: u64 = 25;
/// Where the child's stack page goes in root's own address space.
const CHILD_STACK: u64 = 0x5000_0000;

/// What the child does when it starts: 0 signal and exit, 1 wait for good,
/// 2 signal and delete its own Thread.
static MODE: AtomicU64 = AtomicU64::new(0);

fn signal(n: u64, bits: u64) -> Result<[u64; 4], SysError> {
    sys::invoke(n, label::NOTIFICATION_SIGNAL, &[bits])
}

/// A thread root starts in its own address space and capability space.
extern "C" fn child() -> ! {
    match MODE.load(Ordering::Acquire) {
        1 => {
            let _ = sys::wait(WAITS);
            let _ = signal(TOLD, 0x100);
        }
        2 => {
            let _ = signal(TOLD, 2);
            let _ = sys::invoke(sys::CNODE, label::CNODE_DELETE, &[RUN]);
            let _ = signal(TOLD, 0x200);
        }
        _ => {
            let _ = signal(TOLD, 1);
        }
    }
    sys::exit()
}

/// Give the scheduler to everyone else for a while.
fn settle() {
    for _ in 0..50 {
        sys::yield_now();
    }
}

/// Wait on `n` and check the bits that come back.
fn bits(what: &str, n: u64, want: u64) -> bool {
    let got = match sys::wait(n) {
        Ok(b) if b == want => Ok(()),
        Ok(other) => {
            say(format_args!("root: {what}: got {other:#x}"));
            Err(SysError::InvalidSyscall)
        }
        Err(e) => Err(e),
    };
    check(what, got, Ok(()))
}

/// Threads of root's own making (ADR 0023): configured into root's own
/// spaces with a stack page root maps, started, stopped while waiting,
/// destroyed while waiting, and destroying themselves.
fn thread_checks() -> bool {
    let mut ok = true;
    let retype = |k, at| sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[kind_code(k), 1, at]);
    let mut made = Ok(());
    for (k, at) in [
        (ObjectKind::Thread, RUN),
        (ObjectKind::Notification, TOLD),
        (ObjectKind::Notification, WAITS),
        (ObjectKind::Frame, STACK),
    ] {
        made = made.and(retype(k, at).map(|_| ()));
    }
    ok &= check(
        "retype a Thread, two Notifications and a stack frame",
        made,
        Ok(()),
    );
    // Tables for the stack page, one level at a time, until all are there.
    let mut at = TABLES;
    loop {
        let _ = retype(ObjectKind::PageTable, at);
        match sys::invoke(
            sys::SPACE,
            label::ADDRESS_SPACE_MAP_TABLE,
            &[at, CHILD_STACK],
        ) {
            Ok(_) => at += 1,
            Err(_) => {
                let _ = sys::invoke(sys::CNODE, label::CNODE_DELETE, &[at]);
                break;
            }
        }
    }
    let rw = u64::from((sys::Rights::READ | sys::Rights::WRITE).bits());
    let map = sys::invoke(
        sys::SPACE,
        label::ADDRESS_SPACE_MAP_FRAME,
        &[STACK, CHILD_STACK, rw],
    );
    ok &= check("map the stack into root's own space", map, Ok(()));
    let entry = child as *const () as u64;
    // As if called: the stack pointer is 8 below 16-byte alignment.
    let stack = CHILD_STACK + PAGE - 8;
    let thread = |l, args: &[u64]| sys::invoke(RUN, l, args);
    let resume = || thread(label::THREAD_RESUME, &[]);
    ok &= check(
        "resume an unconfigured Thread",
        resume(),
        Err(SysError::Unsupported),
    );
    let bad = thread(
        label::THREAD_CONFIGURE,
        &[sys::CNODE, sys::SPACE, 1 << 47, stack],
    );
    ok &= check(
        "configure a kernel-half entry",
        bad,
        Err(SysError::Unsupported),
    );
    let config = [sys::CNODE, sys::SPACE, entry, stack];
    ok &= check(
        "configure it in root's spaces",
        thread(label::THREAD_CONFIGURE, &config),
        Ok(()),
    );
    MODE.store(0, Ordering::Release);
    ok &= check("start it", resume(), Ok(()));
    ok &= bits("it signals 1", TOLD, 1);
    settle();
    // Stopped while it waits: a signal then finds no waiter.
    MODE.store(1, Ordering::Release);
    ok &= check("start it again, to wait", resume(), Ok(()));
    settle();
    ok &= check("suspend it", thread(label::THREAD_SUSPEND, &[]), Ok(()));
    ok &= check("signal what it waited on", signal(WAITS, 1), Ok(()));
    ok &= bits("the bits stay pending", WAITS, 1);
    // Destroyed while it waits.
    ok &= check("start it again, to wait", resume(), Ok(()));
    settle();
    let del = sys::invoke(sys::CNODE, label::CNODE_DELETE, &[RUN]);
    ok &= check("delete the Thread while it waits", del, Ok(()));
    ok &= check("signal what it waited on", signal(WAITS, 1), Ok(()));
    ok &= bits("the bits stay pending", WAITS, 1);
    // A thread that deletes its own Thread never comes back.
    ok &= check(
        "retype another Thread",
        retype(ObjectKind::Thread, RUN),
        Ok(()),
    );
    let again = thread(label::THREAD_CONFIGURE, &config);
    ok &= check("configure it", again, Ok(()));
    MODE.store(2, Ordering::Release);
    ok &= check("start it, to delete itself", resume(), Ok(()));
    ok &= bits("it signals 2", TOLD, 2);
    settle();
    ok &= check("its Thread is gone", resume(), Err(SysError::InvalidCap));
    ok &= check("signal it saw nothing more", signal(TOLD, 0x1000), Ok(()));
    ok &= bits("nothing came after the delete", TOLD, 0x1000);
    ok
}

/// The child task's program: signal slot 1 with 5, then wait on slot 2 for
/// good. Hand-assembled, so it needs no loader: it is all the child is.
#[rustfmt::skip]
const CHILD_CODE: &[u8] = &[
    0xb8, 3, 0, 0, 0,                             // mov eax, CALL
    0xbf, 1, 0, 0, 0,                             // mov edi, 1
    0x48, 0xbe, 7, 0, 0, 0, 1, 0, 0, 0,           // mov rsi, signal, 1 word
    0xba, 5, 0, 0, 0,                             // mov edx, 5
    0x49, 0xc7, 0xc4, 0xff, 0xff, 0xff, 0xff,     // mov r12, NO_SLOT
    0x0f, 0x05,                                   // syscall
    0xb8, 2, 0, 0, 0,                             // mov eax, RECV
    0xbf, 2, 0, 0, 0,                             // mov edi, 2
    0x31, 0xf6,                                   // xor esi, esi
    0x0f, 0x05,                                   // syscall
    0xb8, 3, 0, 0, 0,                             // mov eax, CALL: never
    0xbf, 1, 0, 0, 0,                             // mov edi, 1
    0xba, 0, 1, 0, 0,                             // mov edx, 0x100
    0x0f, 0x05,                                   // syscall
    0xb8, 7, 0, 0, 0,                             // mov eax, EXIT
    0x0f, 0x05,                                   // syscall
    0xeb, 0xfe,                                   // jmp $
];

/// Slots for the child task: the Untyped it is built from, its CNode,
/// address space, three tables, code frame and a copy, stack frame and
/// Thread, and root's Notifications it signals and waits on.
const BUDGET: u64 = 32;
const K_CNODE: u64 = 33;
const K_SPACE: u64 = 34;
const K_TABLES: u64 = 35;
const K_CODE: u64 = 38;
const K_CODE_RX: u64 = 39;
const K_STACK: u64 = 40;
const K_THREAD: u64 = 41;
const K_TOLD: u64 = 42;
const K_WAITS: u64 = 43;
/// Where the child's code and stack go in its space, and where root writes
/// the code in its own (next to the thread checks' stack page).
const K_CODE_AT: u64 = 0x40_0000;
const K_STACK_AT: u64 = 0x40_1000;
const WRITE_AT: u64 = CHILD_STACK + PAGE;

/// A task of root's own making (ADR 0023 step 5): built from one child
/// Untyped with its own CNode, address space and Thread, it runs code root
/// wrote; revoking the Untyped stops it and takes all of it back.
fn task_checks() -> bool {
    let mut ok = true;
    let split = sys::invoke(UNTYPED, label::UNTYPED_SPLIT, &[48 * 1024, BUDGET]);
    ok &= check("split 48 KiB for a child task", split, Ok(()));
    let from = |ut: u64, k, at| sys::invoke(ut, label::UNTYPED_RETYPE, &[kind_code(k), 1, at]);
    let cnode = sys::invoke(
        BUDGET,
        label::UNTYPED_RETYPE,
        &[kind_code(ObjectKind::CNode), 1, K_CNODE, 4],
    );
    let mut made = cnode.map(|_| ());
    for (k, at) in [
        (ObjectKind::AddressSpace, K_SPACE),
        (ObjectKind::PageTable, K_TABLES),
        (ObjectKind::PageTable, K_TABLES + 1),
        (ObjectKind::PageTable, K_TABLES + 2),
        (ObjectKind::Frame, K_CODE),
        (ObjectKind::Frame, K_STACK),
        (ObjectKind::Thread, K_THREAD),
    ] {
        made = made.and(from(BUDGET, k, at).map(|_| ()));
    }
    ok &= check(
        "retype its CNode, space, tables, frames and Thread",
        made,
        Ok(()),
    );
    let notes = from(UNTYPED, ObjectKind::Notification, K_TOLD).and(from(
        UNTYPED,
        ObjectKind::Notification,
        K_WAITS,
    ));
    ok &= check(
        "retype root's Notifications for it",
        notes.map(|_| ()),
        Ok(()),
    );
    // Root writes the code through a mapping of its own, then gives the
    // child a copy of the frame capability, mapped read and execute.
    let (rw, rx, rws) = (
        u64::from((sys::Rights::READ | sys::Rights::WRITE).bits()),
        u64::from((sys::Rights::READ | sys::Rights::EXECUTE).bits()),
        u64::from(sys::Rights::ALL.bits()),
    );
    let mine = sys::invoke(
        sys::SPACE,
        label::ADDRESS_SPACE_MAP_FRAME,
        &[K_CODE, WRITE_AT, rw],
    );
    ok &= check("map the code frame into root's space", mine, Ok(()));
    sys::debug::write_bytes(WRITE_AT, CHILD_CODE);
    let _ = sys::invoke(K_CODE, label::FRAME_UNMAP, &[]);
    let copy = sys::invoke(sys::CNODE, label::CNODE_COPY, &[K_CODE, K_CODE_RX, rws]);
    ok &= check("copy the code frame capability", copy, Ok(()));
    let mut built = Ok(());
    for t in 0..3 {
        let table = [K_TABLES + t, K_CODE_AT];
        built = built.and(sys::invoke(K_SPACE, label::ADDRESS_SPACE_MAP_TABLE, &table).map(|_| ()));
    }
    let map = |f, at, r| sys::invoke(K_SPACE, label::ADDRESS_SPACE_MAP_FRAME, &[f, at, r]);
    built = built
        .and(map(K_CODE_RX, K_CODE_AT, rx).map(|_| ()))
        .and(map(K_STACK, K_STACK_AT, rw).map(|_| ()));
    ok &= check("build its address space", built, Ok(()));
    // Its CNode: slot 1 signals root, slot 2 is what it waits on.
    let send = u64::from(sys::Rights::SEND.bits());
    let recv = u64::from(sys::Rights::RECV.bits());
    let caps = sys::invoke(K_CNODE, label::CNODE_COPY, &[K_TOLD, 1, send]).and(sys::invoke(
        K_CNODE,
        label::CNODE_COPY,
        &[K_WAITS, 2, recv],
    ));
    ok &= check(
        "give it two Notification capabilities",
        caps.map(|_| ()),
        Ok(()),
    );
    let config = [K_CNODE, K_SPACE, K_CODE_AT, K_STACK_AT + PAGE];
    let started = sys::invoke(K_THREAD, label::THREAD_CONFIGURE, &config).and(sys::invoke(
        K_THREAD,
        label::THREAD_RESUME,
        &[],
    ));
    ok &= check("configure and start it", started.map(|_| ()), Ok(()));
    ok &= bits("the child task signals 5", K_TOLD, 5);
    settle();
    // Revoking its Untyped stops it and takes every object back.
    ok &= check(
        "revoke its Untyped",
        sys::revoke(sys::CNODE, BUDGET),
        Ok(()),
    );
    let gone = sys::invoke(K_THREAD, label::THREAD_RESUME, &[]);
    ok &= check("its Thread is gone", gone, Err(SysError::InvalidCap));
    ok &= check("signal what it waited on", signal(K_WAITS, 1), Ok(()));
    ok &= bits("no one took it", K_WAITS, 1);
    ok &= check("signal root", signal(K_TOLD, 0x1000), Ok(()));
    ok &= bits("the child never woke", K_TOLD, 0x1000);
    let again = from(BUDGET, ObjectKind::Thread, K_THREAD);
    ok &= check("retype a Thread from its Untyped again", again, Ok(()));
    ok
}

fn address_space_checks() -> bool {
    let mut ok = true;
    let kind = |k| kind_code(k);
    let retype = |k, at| sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[kind(k), 1, at]);
    ok &= check(
        "retype an AddressSpace into slot 11",
        retype(ObjectKind::AddressSpace, SPACE),
        Ok(()),
    );
    let mut made = Ok(());
    for at in [12, 13, 14, 17] {
        made = made.and(retype(ObjectKind::PageTable, at).map(|_| ()));
    }
    ok &= check("retype four PageTables", made, Ok(()));
    ok &= check(
        "retype a Frame into slot 15",
        retype(ObjectKind::Frame, 15),
        Ok(()),
    );
    ok &= check(
        "retype a Frame into slot 18",
        retype(ObjectKind::Frame, 18),
        Ok(()),
    );
    let (r, rw, rx) = (
        u64::from(sys::Rights::READ.bits()),
        u64::from((sys::Rights::READ | sys::Rights::WRITE).bits()),
        u64::from((sys::Rights::READ | sys::Rights::EXECUTE).bits()),
    );
    let base = MAPPED - PAGE;
    let map = |f: u64, v: u64, rights: u64| {
        sys::invoke(SPACE, label::ADDRESS_SPACE_MAP_FRAME, &[f, v, rights])
    };
    let table = |t: u64| sys::invoke(SPACE, label::ADDRESS_SPACE_MAP_TABLE, &[t, base]);
    ok &= check(
        "map a frame with no tables",
        map(15, base, rw),
        Err(SysError::MissingTable),
    );
    let mut installed = Ok(());
    for t in [12, 13, 14] {
        installed = installed.and(table(t).map(|_| ()));
    }
    ok &= check(
        "install a PDPT, a page directory and a page table",
        installed,
        Ok(()),
    );
    ok &= check(
        "install a fourth table there",
        table(17),
        Err(SysError::AlreadyMapped),
    );
    ok &= check("map the frame in slot 15", map(15, base, rw), Ok(()));
    ok &= check(
        "map that capability again",
        map(15, MAPPED, rw),
        Err(SysError::AlreadyMapped),
    );
    let copy = |src: u64, dst: u64| {
        sys::invoke(
            sys::CNODE,
            label::CNODE_COPY,
            &[src, dst, u64::from(sys::Rights::ALL.bits())],
        )
    };
    ok &= check(
        "copy the frame capability into slot 16",
        copy(15, 16),
        Ok(()),
    );
    ok &= check(
        "map the copy at the same address",
        map(16, base, r),
        Err(SysError::AlreadyMapped),
    );
    ok &= check(
        "map the copy, read and execute",
        map(16, MAPPED, rx),
        Ok(()),
    );
    let wx = u64::from((sys::Rights::READ | sys::Rights::WRITE | sys::Rights::EXECUTE).bits());
    ok &= check(
        "map a frame writable and executable",
        map(18, MAPPED + PAGE, wx),
        Err(SysError::CapNotPermitted),
    );
    ok &= check(
        "map a frame unreadable",
        map(18, MAPPED + PAGE, u64::from(sys::Rights::WRITE.bits())),
        Err(SysError::CapNotPermitted),
    );
    ok &= check(
        "map the frame in slot 18",
        map(18, MAPPED + PAGE, r),
        Ok(()),
    );
    ok &= check("unmap it", sys::invoke(18, label::FRAME_UNMAP, &[]), Ok(()));
    ok &= check(
        "unmap it again",
        sys::invoke(18, label::FRAME_UNMAP, &[]),
        Ok(()),
    );
    ok &= check(
        "copy a PageTable capability",
        copy(12, 19),
        Err(SysError::CapNotPermitted),
    );
    let mint = sys::invoke(sys::CNODE, label::CNODE_MINT, &[15, 19, r, 7]);
    ok &= check(
        "mint a frame capability",
        mint,
        Err(SysError::CapNotPermitted),
    );
    let del = sys::invoke(sys::CNODE, label::CNODE_DELETE, &[15]);
    ok &= check("delete the frame capability in slot 15", del, Ok(()));
    ok
}

fn main() -> ! {
    say(format_args!(
        "root: hello from ring 3; slots 0 null, 1 CNode, 2 Untyped 256 KiB, 3 console, 5 its address space"
    ));
    let mut ok = true;

    // A refused retype has no effect: sixteen 4 KiB frames into its occupied
    // CNode slot would use up all 64 KiB if a refusal still took the memory.
    let frame = kind_code(ObjectKind::Frame);
    let mut refused = true;
    for _ in 0..16 {
        let r = sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[frame, 1, sys::CNODE]);
        refused &= r == Err(SysError::CapOccupied);
    }
    let r: Result<(), SysError> = if refused {
        Err(SysError::CapOccupied)
    } else {
        Ok(())
    };
    ok &= check(
        "retype 16 frames into occupied slot 1",
        r,
        Err(SysError::CapOccupied),
    );

    // A kernel object it may create and use: a Notification from its Untyped.
    let note = kind_code(ObjectKind::Notification);
    let retype = sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[note, 1, NOTE]);
    ok &= check("retype a Notification into slot 4", retype, Ok(()));
    // Waiting with bits pending returns them at once, and takes them all.
    let signal = sys::invoke(NOTE, label::NOTIFICATION_SIGNAL, &[0b11]);
    ok &= check("signal it", signal, Ok(()));
    let bits = sys::wait(NOTE);
    let got = match bits {
        Ok(0b11) => Ok(()),
        Ok(other) => {
            say(format_args!("root: wait gave {other:#b}"));
            Err(SysError::InvalidSyscall)
        }
        Err(e) => Err(e),
    };
    ok &= check("wait on it: bits 0b11", got, Ok(()));
    // Left pending for the kernel to find after root exits.
    let signal = sys::invoke(NOTE, label::NOTIFICATION_SIGNAL, &[0b101]);
    ok &= check("signal it again", signal, Ok(()));
    // An Endpoint too; the kernel grants it to a server once root exits,
    // and reaping root must take it back (#105).
    let endpoint = kind_code(ObjectKind::Endpoint);
    let retype = sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[endpoint, 1, ENDPOINT]);
    ok &= check("retype an Endpoint into slot 7", retype, Ok(()));
    // A child Untyped is a bounded budget (#162): 8 KiB split off holds one
    // page-aligned frame after its control block, and no more. Revoking the
    // child takes the frame back and frees its whole region again.
    let split = sys::invoke(UNTYPED, label::UNTYPED_SPLIT, &[0x2000, CHILD]);
    ok &= check("split an 8 KiB Untyped into slot 8", split, Ok(()));
    let first = sys::invoke(CHILD, label::UNTYPED_RETYPE, &[frame, 1, CHILD_FRAME]);
    ok &= check("retype a frame from it into slot 9", first, Ok(()));
    let more = sys::invoke(CHILD, label::UNTYPED_RETYPE, &[frame, 1, CHILD_FRAME + 1]);
    ok &= check(
        "retype a second frame from it",
        more,
        Err(SysError::OutOfMemory),
    );
    ok &= check("revoke slot 8", sys::revoke(sys::CNODE, CHILD), Ok(()));
    let again = sys::invoke(CHILD, label::UNTYPED_RETYPE, &[frame, 1, CHILD_FRAME]);
    ok &= check("retype a frame into slot 9 again", again, Ok(()));
    ok &= address_space_checks();
    ok &= thread_checks();
    ok &= task_checks();

    // More arguments than the ABI carries are refused, not truncated (#135).
    let five = sys::invoke(NOTE, label::NOTIFICATION_SIGNAL, &[0b10, 0, 0, 0, 0]);
    ok &= check(
        "invoke with five arguments",
        five,
        Err(SysError::InvalidSyscall),
    );

    // Slots it does not hold: the null slot, an empty one, one past its CNode.
    for (slot, what) in [
        (0, "invoke slot 0"),
        (30, "invoke empty slot 30"),
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

    // Revoke takes back what was derived (ADR 0020): a copy of the console
    // capability goes, and the capability it was copied from stays.
    let send = u64::from(sys::Rights::SEND.bits());
    let copy = sys::invoke(sys::CNODE, label::CNODE_COPY, &[CONSOLE, COPY, send]);
    ok &= check("copy the console capability into slot 6", copy, Ok(()));
    ok &= check("revoke slot 3", sys::revoke(sys::CNODE, CONSOLE), Ok(()));
    let plain = MessageInfo::new(0, 0, 0);
    let gone = sys::send(COPY, plain, [0; 4], sys::NULL);
    ok &= check(
        "send through the revoked copy",
        gone,
        Err(SysError::InvalidCap),
    );
    let empty = sys::revoke(sys::CNODE, COPY);
    ok &= check("revoke empty slot 6", empty, Err(SysError::CapEmpty));

    // A CNode of its own making (ADR 0023): copies into it land there, not
    // in root's space, and destroying it with a capability inside is fine.
    let cnode = kind_code(ObjectKind::CNode);
    let odd = sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[cnode, 1, KID, 12]);
    ok &= check("retype a 12-slot CNode", odd, Err(SysError::Unsupported));
    let kid = sys::invoke(UNTYPED, label::UNTYPED_RETYPE, &[cnode, 1, KID, 16]);
    ok &= check("retype a 16-slot CNode into slot 10", kid, Ok(()));
    let into = [CONSOLE, 6, send];
    ok &= check(
        "copy the console capability into its slot 6",
        sys::invoke(KID, label::CNODE_COPY, &into),
        Ok(()),
    );
    let mine = sys::invoke(6, label::NOTIFICATION_SIGNAL, &[1]);
    ok &= check(
        "root's own slot 6 stays empty",
        mine,
        Err(SysError::InvalidCap),
    );
    let twice = sys::invoke(KID, label::CNODE_COPY, &into);
    ok &= check(
        "copy into its slot 6 again",
        twice,
        Err(SysError::CapOccupied),
    );
    let del = sys::invoke(KID, label::CNODE_DELETE, &[6]);
    ok &= check("delete its slot 6", del, Ok(()));
    let del = sys::invoke(KID, label::CNODE_DELETE, &[6]);
    ok &= check("delete its slot 6 again", del, Err(SysError::CapEmpty));
    let refill = sys::invoke(KID, label::CNODE_COPY, &into);
    ok &= check("copy into its slot 6 once more", refill, Ok(()));
    let gone = sys::invoke(sys::CNODE, label::CNODE_DELETE, &[KID]);
    ok &= check("delete the CNode, a capability still inside", gone, Ok(()));

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
