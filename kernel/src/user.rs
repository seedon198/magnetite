//! User threads (#82, [ADR 0019](../../docs/adr/0019-first-user-task.md),
//! PR 1): each in its own address space with its own kernel stack, entering
//! ring 3 once and returning to the kernel only through syscalls and
//! interrupts. A fault in ring 3 ends the thread, never the kernel.
//!
//! This module proves the mechanism with hand-assembled stubs and two debug
//! syscalls of their own (`hello`, `report`), and keeps its regression checks
//! running. Real programs are loaded by `task`; the ABI is `syscall`.

use core::sync::atomic::{AtomicU64, Ordering};

use boot_info::BootInfo;
use hal::paging::{AddressSpace, Flags, PAGE};
use hal::syscall::{SyscallArgs, UserEntry};
use sched::ThreadId;

use crate::thread::{self, CONTEXTS};
use crate::{KERNEL_SPACE, SCHED};

/// Where a user thread's code and stack live in its own address space. The
/// page below the stack stays unmapped as a guard.
const USER_CODE_VA: u64 = 0x40_0000;
const USER_STACK_VA: u64 = 0x7fff_f000;

/// Debug syscalls for the mechanism checks below, outside the ABI's numbers
/// and honoured only for the threads this demo spawns.
const SYS_HELLO: u64 = 0x100;
const SYS_REPORT: u64 = 0x101;
/// Returns where the kernel's frame chain ends inside this syscall (#111).
const SYS_CHAIN: u64 = 0x102;

/// Each user thread's first message register, handed to it in `rdx`.
static MR0: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];
/// How far below the top of its stack page each user thread starts.
static RSP_OFFSET: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];
/// Values user threads reported with `report`, OR-ed together per thread.
static REPORT: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];
/// The exception vector that killed each user thread, plus one (0 = none),
/// and the user `rip` it was raised at.
static KILLED_BY: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];
static KILLED_AT: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];

/// The demo's threads, one bit each: only they may use the debug syscalls.
static DEBUG_THREADS: AtomicU64 = AtomicU64::new(0);

/// Create user thread `t`: a fresh address space holding `code` (read and
/// execute) and a one-page stack (read and write, never execute), its own
/// kernel stack from window slot `stack_slot`, `mr0` for its `rdx`, and its
/// stack pointer `rsp_offset` bytes below the top of its stack page. Admits it
/// to the run queue. Returns `false` if memory ran out.
pub fn spawn_user(t: ThreadId, stack_slot: usize, code: &[u8], mr0: u64, rsp_offset: u64) -> bool {
    assert!(code.len() <= PAGE as usize, "user: stub larger than a page");
    let Some(mut space) = hal::cpu::without_interrupts(|| {
        KERNEL_SPACE
            .lock()
            .as_ref()
            .and_then(AddressSpace::new_user)
    }) else {
        return false;
    };
    let (Some(code_frame), Some(stack_frame)) =
        (hal::frame::alloc_frame(), hal::frame::alloc_frame())
    else {
        return false;
    };
    let hhdm = crate::ipc::hhdm();
    hal::object::write_phys(code_frame.addr(), hhdm, code);
    let mapped = space
        .map(USER_CODE_VA, code_frame.addr(), Flags::USER)
        .is_ok()
        && space
            .map(
                USER_STACK_VA,
                stack_frame.addr(),
                Flags::USER | Flags::WRITABLE | Flags::NO_EXECUTE,
            )
            .is_ok();
    let Some(kstack) = thread::alloc_kernel_stack(stack_slot) else {
        return false;
    };
    if !mapped {
        return false;
    }
    DEBUG_THREADS.fetch_or(1 << t.0, Ordering::Relaxed);
    MR0[t.0].store(mr0, Ordering::Relaxed);
    RSP_OFFSET[t.0].store(rsp_offset, Ordering::Relaxed);
    thread::set_kernel_stack(t, stack_slot, kstack);
    thread::set_address_space(t, space);
    CONTEXTS[t.0].init(kstack, user_entry);
    hal::cpu::without_interrupts(|| SCHED.lock().admit(t))
}

/// A user thread's first code, on its own kernel stack and in its own address
/// space (`switch_to` installed both): drop to ring 3 for good.
extern "C" fn user_entry() -> ! {
    let t =
        hal::cpu::without_interrupts(|| SCHED.lock().current()).expect("user: a running thread");
    let entry = UserEntry {
        rip: USER_CODE_VA,
        rsp: USER_STACK_VA + PAGE - RSP_OFFSET[t.0].load(Ordering::Relaxed),
        cap: 0,
        msg_info: 0,
        mr: [MR0[t.0].load(Ordering::Relaxed), 0, 0, 0],
    };
    hal::syscall::enter_user_once(&entry)
}

/// The demo's debug syscalls, for its own threads only: `hello` logs and
/// `report` records a value. `None` for any other thread or number, which the
/// dispatcher then treats as an ordinary syscall.
pub fn debug_syscall(t: ThreadId, args: &SyscallArgs) -> Option<u64> {
    if DEBUG_THREADS.load(Ordering::Relaxed) & (1 << t.0) == 0 {
        return None;
    }
    match args.number {
        SYS_HELLO => {
            crate::info!(
                "user: thread {} in ring 3, own address space (mr0={:#x})",
                t.0,
                args.mr[0]
            );
            Some(0)
        }
        SYS_REPORT => {
            REPORT[t.0].fetch_or(args.mr[0], Ordering::Relaxed);
            Some(0)
        }
        SYS_CHAIN => Some(hal::backtrace::chain_end()),
        _ => None,
    }
}

/// A CPU exception raised in ring 3: log it and end the thread. The kernel
/// carries on. Registered once in `kmain`.
pub fn user_fault(frame: &mut hal::idt::TrapFrame) -> ! {
    let t = SCHED.lock().current().expect("user: a running thread");
    KILLED_BY[t.0].store(frame.vector + 1, Ordering::Relaxed);
    KILLED_AT[t.0].store(frame.rip, Ordering::Relaxed);
    let name = hal::idt::exception_name(frame.vector);
    // CR2 is meaningful only for #PF; for anything else it still holds the
    // last page fault taken anywhere, possibly another context's (#104).
    if frame.vector == 14 {
        crate::info!(
            "user: thread {} killed: {name} at {:#x}, error {:#x}, rip={:#x}",
            t.0,
            hal::idt::fault_address(),
            frame.error_code,
            frame.rip
        );
    } else {
        crate::info!(
            "user: thread {} killed: {name}, error {:#x}, rip={:#x}",
            t.0,
            frame.error_code,
            frame.rip
        );
    }
    crate::exit_current()
}

/// The syscall boundary will not return to the current thread: end it. Both
/// causes are backstops that ring 3 cannot reach today: the GDT gives it no
/// way into compatibility mode, and `USER_TOP` keeps code out of the last
/// user page. Registered once in `kmain`.
pub fn syscall_killed(reason: hal::syscall::Kill) -> ! {
    let t = SCHED.lock().current().expect("user: a running thread");
    let why = match reason {
        hal::syscall::Kill::CompatSyscall => "syscall from compatibility mode",
        hal::syscall::Kill::NonCanonicalReturn => "non-canonical syscall return address",
    };
    crate::info!("user: thread {} killed: {why}", t.0);
    crate::exit_current()
}

/// Forget thread `t`'s debug-syscall permission, so a later task given the
/// same id cannot use them (#131). Called when `t` is reaped.
pub fn forget(t: ThreadId) {
    DEBUG_THREADS.fetch_and(!(1 << t.0), Ordering::Relaxed);
}

/// Frames currently free.
fn free_frames() -> usize {
    hal::frame::stats().map_or(0, |s| s.free)
}

/// Prove the mechanism, and pin the failure modes a review found:
///
/// - threads 7 and 8 say hello, block inside the kernel (`yield`) across each
///   other's syscalls, report the user stack pointer they came back with, spin
///   long enough to be preempted in ring 3, and exit;
/// - thread 9 reads kernel memory and is killed;
/// - thread 10 reports every register at ring-3 entry and after a syscall: no
///   kernel value may reach ring 3, so both reports are zero;
/// - thread 11 sets `RFLAGS.NT`, issues an unknown syscall (it must return
///   `InvalidSyscall`) and exits: the flag must not leak into the kernel,
///   where a later `iretq` would fault in ring 0;
/// - thread 15 far-returns to selector `0x1b` to reach compatibility mode and
///   `syscall` from there, which would enter ring 0 at `IA32_CSTAR` on its own
///   stack (#102): it must be killed by a ring-3 fault at the far return
///   (`#GP` on hardware; QEMU 8 raises `#PF` reading the user stack first);
/// - thread 16 executes `sysenter`, which must fault in ring 3 too;
/// - thread 17 sets `rbp` to a marker and asks where the kernel's frame chain
///   ends inside its syscall: at 0, never at the user's `rbp` (#111), which
///   must come back unchanged.
pub fn demo(boot_info: &BootInfo, preemptive: bool) {
    // Assembled with `llvm-mc -triple=x86_64 -filetype=obj` and
    // `llvm-objcopy -O binary --only-section=.text`, from:
    //   mov eax, 0x100; syscall            (hello)
    //   mov eax, 6; syscall                (yield)
    //   mov rdx, rsp; mov eax, 0x101; syscall   (report rsp)
    //   mov r12d, 0x2000000; 1: dec r12d; jnz 1b
    //   mov eax, 7; syscall; 2: jmp 2b     (exit)
    const YIELD_REPORT: [u8; 44] = [
        0xb8, 0x00, 0x01, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48,
        0x89, 0xe2, 0xb8, 0x01, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x41, 0xbc, 0x00, 0x00, 0x00, 0x02,
        0x41, 0xff, 0xcc, 0x75, 0xfb, 0xb8, 0x07, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xfe,
    ];
    // rax = OR of all registers at entry; mov rdx, rax; set r12 = 0x12 and
    // r13 = 0x13; report (0x101); xor them back; rax = OR of rdi, rsi, r8 to
    // r10, rbx, rbp, r12 to r15 after the syscall; report; exit. Both reports
    // are zero exactly when no kernel value reached ring 3 and r12/r13 came
    // back unchanged. Assembled as YIELD_REPORT.
    const LEAK: [u8; 124] = [
        0x48, 0x89, 0xe8, 0x48, 0x09, 0xd8, 0x48, 0x09, 0xc8, 0x48, 0x09, 0xd0, 0x48, 0x09, 0xf0,
        0x48, 0x09, 0xf8, 0x4c, 0x09, 0xc0, 0x4c, 0x09, 0xc8, 0x4c, 0x09, 0xd0, 0x4c, 0x09, 0xd8,
        0x4c, 0x09, 0xe0, 0x4c, 0x09, 0xe8, 0x4c, 0x09, 0xf0, 0x4c, 0x09, 0xf8, 0x48, 0x89, 0xc2,
        0x41, 0xbc, 0x12, 0x00, 0x00, 0x00, 0x41, 0xbd, 0x13, 0x00, 0x00, 0x00, 0xb8, 0x01, 0x01,
        0x00, 0x00, 0x0f, 0x05, 0x49, 0x83, 0xf4, 0x12, 0x49, 0x83, 0xf5, 0x13, 0x48, 0x89, 0xf8,
        0x48, 0x09, 0xf0, 0x4c, 0x09, 0xc0, 0x4c, 0x09, 0xc8, 0x4c, 0x09, 0xd0, 0x48, 0x09, 0xd8,
        0x48, 0x09, 0xe8, 0x4c, 0x09, 0xe0, 0x4c, 0x09, 0xe8, 0x4c, 0x09, 0xf0, 0x4c, 0x09, 0xf8,
        0x48, 0x89, 0xc2, 0xb8, 0x01, 0x01, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x07, 0x00, 0x00, 0x00,
        0x0f, 0x05, 0xeb, 0xfe,
    ];
    // pushfq; or qword [rsp], 0x4000 (NT); popfq; then syscall 99, which no
    // ABI defines; report its rax; exit. Assembled as YIELD_REPORT.
    const SET_NT_EXIT: [u8; 36] = [
        0x9c, 0x48, 0x81, 0x0c, 0x24, 0x00, 0x40, 0x00, 0x00, 0x9d, 0xb8, 0x63, 0x00, 0x00, 0x00,
        0x0f, 0x05, 0x48, 0x89, 0xc2, 0xb8, 0x01, 0x01, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x07, 0x00,
        0x00, 0x00, 0x0f, 0x05, 0xeb, 0xfe,
    ];
    // push 0x1b; lea rax, [rip + 1f]; push rax; retfq; 1: syscall; 2: jmp 2b.
    // The retfq is at offset 10. Assembled as YIELD_REPORT.
    const COMPAT_SYSCALL: [u8; 16] = [
        0x6a, 0x1b, 0x48, 0x8d, 0x05, 0x03, 0x00, 0x00, 0x00, 0x50, 0x48, 0xcb, 0x0f, 0x05, 0xeb,
        0xfe,
    ];
    // sysenter; 2: jmp 2b. Assembled as YIELD_REPORT.
    const SYSENTER: [u8; 4] = [0x0f, 0x34, 0xeb, 0xfe];
    // mov ebp, 0x12345678; chain (0x102); mov rdx, rbp; xor rdx, 0x12345678;
    // or rdx, rax; report (0x101); exit. Zero exactly when the chain ended at
    // 0 and rbp came back. Assembled as YIELD_REPORT.
    const RBP_CHAIN: [u8; 41] = [
        0xbd, 0x78, 0x56, 0x34, 0x12, 0xb8, 0x02, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x89, 0xea,
        0x48, 0x81, 0xf2, 0x78, 0x56, 0x34, 0x12, 0x48, 0x09, 0xc2, 0xb8, 0x01, 0x01, 0x00, 0x00,
        0x0f, 0x05, 0xb8, 0x07, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xfe,
    ];
    // movabs rax, [hhdm]; jmp $
    let mut fault = [0u8; 12];
    fault[..2].copy_from_slice(&[0x48, 0xA1]);
    fault[2..10].copy_from_slice(&boot_info.hhdm_offset.to_le_bytes());
    fault[10..].copy_from_slice(&[0xEB, 0xFE]);

    let free_before = free_frames();

    let spawned = spawn_user(ThreadId(7), 1, &YIELD_REPORT, 0xA1, 0)
        && spawn_user(ThreadId(8), 2, &YIELD_REPORT, 0xA2, 64)
        && spawn_user(ThreadId(9), 3, &fault, 0, 0)
        && spawn_user(ThreadId(10), 4, &LEAK, 0, 0)
        && spawn_user(ThreadId(11), 5, &SET_NT_EXIT, 0, 0)
        && spawn_user(ThreadId(15), 8, &COMPAT_SYSCALL, 0, 0)
        && spawn_user(ThreadId(16), 9, &SYSENTER, 0, 0)
        && spawn_user(ThreadId(17), 10, &RBP_CHAIN, 0, 0);
    assert!(spawned, "user: could not create the demo threads");

    // A kernel-stack slot in use is refused, with nothing allocated and the
    // owner's stack untouched (#130).
    let before_refusal = free_frames();
    assert!(
        thread::alloc_kernel_stack(1).is_none(),
        "user: allocated a kernel stack over thread 7's"
    );
    assert_eq!(
        free_frames(),
        before_refusal,
        "user: a refused kernel stack leaked frames"
    );

    let demo_threads = || [7, 8, 9, 10, 11, 15, 16, 17].into_iter().map(ThreadId);
    for t in demo_threads() {
        thread::wait_exit(t, "a user demo task");
    }

    let top = USER_STACK_VA + PAGE;
    let (rsp7, rsp8) = (
        REPORT[7].load(Ordering::Relaxed),
        REPORT[8].load(Ordering::Relaxed),
    );
    assert!(
        rsp7 == top && rsp8 == top - 64,
        "user: a thread came back from a blocking syscall on the wrong stack ({rsp7:#x}, {rsp8:#x})"
    );
    let unknown = REPORT[11].load(Ordering::Relaxed);
    assert_eq!(
        unknown,
        capability::SysError::InvalidSyscall.code(),
        "user: an unknown syscall did not return InvalidSyscall"
    );
    let killed_by = |t: usize| KILLED_BY[t].load(Ordering::Relaxed).checked_sub(1);
    assert_eq!(
        killed_by(9),
        Some(14),
        "user: a kernel read was not a page fault"
    );
    // Killed by a fault at the far return, so it never ran in compatibility
    // mode: a `syscall` from there would end it through `compat_syscall`,
    // which records no vector.
    assert!(
        killed_by(15).is_some() && KILLED_AT[15].load(Ordering::Relaxed) == USER_CODE_VA + 10,
        "user: a far return to a 32-bit code segment did not fault"
    );
    assert!(
        matches!(killed_by(16), Some(6 | 13)),
        "user: sysenter did not fault in ring 3"
    );
    let chain = REPORT[17].load(Ordering::Relaxed);
    assert_eq!(
        chain, 0,
        "user: the kernel frame chain reached a user rbp, or rbp was not restored"
    );
    let leaked = REPORT[10].load(Ordering::Relaxed);
    assert_eq!(leaked, 0, "user: kernel register values reached ring 3");
    let n = thread::USER_PREEMPTIONS.load(Ordering::Relaxed);
    assert!(!preemptive || n > 0, "user: ring 3 was never preempted");
    let kernel_maps = hal::cpu::without_interrupts(|| {
        KERNEL_SPACE
            .lock()
            .as_ref()
            .is_some_and(|k| k.translate(USER_CODE_VA).is_some())
    });
    assert!(
        !kernel_maps,
        "user: user code is visible in the kernel space"
    );
    assert!(
        thread::user_maps(ThreadId(7), USER_CODE_VA),
        "user: code not in the task's space"
    );
    crate::info!(
        "user: two tasks blocked inside syscalls and came back on their own stacks, were preempted in ring 3 ({n} times) and exited"
    );
    crate::info!("user: no kernel register reached ring 3; RFLAGS.NT did not leak; a kernel read from ring 3 was contained");
    crate::info!("user: compatibility mode and sysenter are unreachable from ring 3");
    crate::info!("user: the kernel frame chain ends at the syscall entry; user rbp preserved");

    // Every frame the eight tasks used (code, stack, page tables, kernel stack)
    // comes back once they are reaped.
    let reclaimed: usize = demo_threads().map(thread::reap).sum();
    assert_eq!(
        DEBUG_THREADS.load(Ordering::Relaxed),
        0,
        "user: reaped threads kept their debug syscalls"
    );
    let free_after = free_frames();
    assert_eq!(
        free_after,
        free_before,
        "user: exited tasks leaked {} frames",
        free_before.saturating_sub(free_after)
    );
    crate::info!("user: reclaimed {reclaimed} frames from 8 exited tasks");
}
