//! User threads (#82, [ADR 0019](../../docs/adr/0019-first-user-task.md),
//! PR 1): each in its own address space with its own kernel stack, entering
//! ring 3 once and returning to the kernel only through syscalls and
//! interrupts. A fault in ring 3 ends the thread, never the kernel.
//!
//! PR 1 proves the mechanism with hand-assembled stubs and two syscalls
//! (`hello` and `exit`). The Rust root task, the ELF loader and the full
//! syscall ABI arrive in PR 2.

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

/// Syscall numbers PR 1 understands (the full ABI is PR 2). `report` and
/// `yield` exist for the demo's regression checks.
const SYS_HELLO: u64 = 1;
const SYS_REPORT: u64 = 2;
const SYS_YIELD: u64 = 6;
const SYS_EXIT: u64 = 7;

/// Each user thread's first message register, handed to it in `rdx`.
static MR0: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];
/// How far below the top of its stack page each user thread starts.
static RSP_OFFSET: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];
/// Values user threads reported with `report`, OR-ed together per thread.
static REPORT: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];

/// User demo threads that exited or were killed.
static DONE: AtomicU64 = AtomicU64::new(0);

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
    MR0[t.0].store(mr0, Ordering::Relaxed);
    RSP_OFFSET[t.0].store(rsp_offset, Ordering::Relaxed);
    thread::set_kernel_stack(t, kstack);
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

/// The system-call handler (PR 1): `hello` logs, `exit` ends the thread.
fn syscall(args: &SyscallArgs) -> u64 {
    let t = SCHED.lock().current().expect("user: a running thread");
    match args.number {
        SYS_HELLO => {
            crate::info!(
                "user: thread {} in ring 3, own address space (mr0={:#x})",
                t.0,
                args.mr[0]
            );
            0
        }
        SYS_REPORT => {
            REPORT[t.0].fetch_or(args.mr[0], Ordering::Relaxed);
            0
        }
        SYS_YIELD => {
            // Block inside the kernel while other threads enter and leave
            // their own syscalls: each must come back to its own user stack.
            crate::yield_now();
            0
        }
        SYS_EXIT => {
            DONE.fetch_add(1, Ordering::Release);
            crate::exit_current()
        }
        _ => u64::MAX,
    }
}

/// A CPU exception raised in ring 3: log it and end the thread. The kernel
/// carries on.
fn user_fault(frame: &mut hal::idt::TrapFrame) -> ! {
    let t = SCHED.lock().current().expect("user: a running thread");
    crate::info!(
        "user: thread {} killed: {} at {:#x}, rip={:#x}",
        t.0,
        hal::idt::exception_name(frame.vector),
        hal::idt::fault_address(),
        frame.rip
    );
    DONE.fetch_add(1, Ordering::Release);
    crate::exit_current()
}

/// Prove the mechanism, and pin the failure modes a review found:
///
/// - threads 7 and 8 say hello, block inside the kernel (`yield`) across each
///   other's syscalls, report the user stack pointer they came back with, spin
///   long enough to be preempted in ring 3, and exit;
/// - thread 9 reads kernel memory and is killed;
/// - thread 10 reports every register at ring-3 entry and after a syscall: no
///   kernel value may reach ring 3, so both reports are zero;
/// - thread 11 sets `RFLAGS.NT` and exits: the flag must not leak into the
///   kernel, where a later `iretq` would fault in ring 0.
pub fn demo(boot_info: &BootInfo, preemptive: bool) {
    // hello; yield; report rsp; spin 0x2000000; exit
    const YIELD_REPORT: [u8; 44] = [
        0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48,
        0x89, 0xe2, 0xb8, 0x02, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x41, 0xbc, 0x00, 0x00, 0x00, 0x02,
        0x41, 0xff, 0xcc, 0x75, 0xfb, 0xb8, 0x07, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xfe,
    ];
    // rax = OR of all registers at entry; report; rax = OR of rdi, rsi, r8 to
    // r10, rbx, rbp, r12 to r15 after the syscall; report; exit
    const LEAK: [u8; 104] = [
        0x48, 0x89, 0xe8, 0x48, 0x09, 0xd8, 0x48, 0x09, 0xc8, 0x48, 0x09, 0xd0, 0x48, 0x09, 0xf0,
        0x48, 0x09, 0xf8, 0x4c, 0x09, 0xc0, 0x4c, 0x09, 0xc8, 0x4c, 0x09, 0xd0, 0x4c, 0x09, 0xd8,
        0x4c, 0x09, 0xe0, 0x4c, 0x09, 0xe8, 0x4c, 0x09, 0xf0, 0x4c, 0x09, 0xf8, 0x48, 0x89, 0xc2,
        0xb8, 0x02, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x89, 0xf8, 0x48, 0x09, 0xf0, 0x4c, 0x09,
        0xc0, 0x4c, 0x09, 0xc8, 0x4c, 0x09, 0xd0, 0x48, 0x09, 0xd8, 0x48, 0x09, 0xe8, 0x4c, 0x09,
        0xe0, 0x4c, 0x09, 0xe8, 0x4c, 0x09, 0xf0, 0x4c, 0x09, 0xf8, 0x48, 0x89, 0xc2, 0xb8, 0x02,
        0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x07, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb, 0xfe,
    ];
    // pushfq; or qword [rsp], 0x4000 (NT); popfq; exit
    const SET_NT_EXIT: [u8; 19] = [
        0x9c, 0x48, 0x81, 0x0c, 0x24, 0x00, 0x40, 0x00, 0x00, 0x9d, 0xb8, 0x07, 0x00, 0x00, 0x00,
        0x0f, 0x05, 0xeb, 0xfe,
    ];
    // movabs rax, [hhdm]; jmp $
    let mut fault = [0u8; 12];
    fault[..2].copy_from_slice(&[0x48, 0xA1]);
    fault[2..10].copy_from_slice(&boot_info.hhdm_offset.to_le_bytes());
    fault[10..].copy_from_slice(&[0xEB, 0xFE]);

    hal::syscall::init();
    hal::syscall::register(syscall);
    hal::idt::register_user_fault(user_fault);

    let spawned = spawn_user(ThreadId(7), 1, &YIELD_REPORT, 0xA1, 0)
        && spawn_user(ThreadId(8), 2, &YIELD_REPORT, 0xA2, 64)
        && spawn_user(ThreadId(9), 3, &fault, 0, 0)
        && spawn_user(ThreadId(10), 4, &LEAK, 0, 0)
        && spawn_user(ThreadId(11), 5, &SET_NT_EXIT, 0, 0);
    assert!(spawned, "user: could not create the demo threads");

    while DONE.load(Ordering::Acquire) < 5 {
        crate::yield_now();
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
}
