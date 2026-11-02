//! The kernel's thread table (#82, [ADR 0019](../../docs/adr/0019-first-user-task.md)).
//!
//! Each thread has a saved context, and optionally its own kernel stack and its
//! own address space. Threads without them run on the stack they were given and
//! in the kernel's address space. [`switch_to`] is the one path every context
//! switch takes: it points ring-3 entries at the next thread's kernel stack,
//! loads its address space if that differs, then switches.
//!
//! Threads stay a kernel table rather than `Thread` objects retyped from
//! Untyped until tasks can create threads (M5); see ADR 0019 decision 3.

use core::sync::atomic::{AtomicU64, Ordering};

use hal::context::Context;
use hal::paging::{AddressSpace, Flags, PAGE};
use hal::sync::SpinLock;
use sched::ThreadId;

use crate::{KERNEL_SPACE, SCHED};

/// Thread slots: boot (0), cooperative workers (1, 2), preemption spinners
/// (3, 4), IPC demo server and client (5, 6), user demo tasks (7 to 11), the
/// console server (12), the fault task (13), the root task (14) and the user
/// demo's entry-path checks (15 to 17).
pub const MAX: usize = 18;

/// Saved contexts, indexed by [`ThreadId`].
pub static CONTEXTS: [Context; MAX] = [const { Context::new() }; MAX];

/// Each thread's kernel stack top (0 = runs on the stack it was started on).
static KSTACK_TOP: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

/// Each thread's kernel-stack window slot, plus one (0 = none), for reaping.
static STACK_SLOT_OF: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

/// Each thread's own address space; `None` runs in the kernel's.
static SPACES: SpinLock<[Option<AddressSpace>; MAX]> = SpinLock::new([const { None }; MAX]);

/// The kernel address space's root, and the root currently in `CR3`.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);
static CURRENT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Threads that have exited, one bit each. A thread blocked in IPC is off the
/// run queue too, so only this says a thread is gone for good.
static EXITED: AtomicU64 = AtomicU64::new(0);

const _: () = assert!(MAX <= 64, "thread: EXITED holds one bit per thread");

/// Record that `t` exited (called by the exit path, before it switches away).
pub fn mark_exited(t: ThreadId) {
    EXITED.fetch_or(1 << t.0, Ordering::Release);
}

/// Whether `t` has exited and not yet been reaped.
pub fn has_exited(t: ThreadId) -> bool {
    EXITED.load(Ordering::Acquire) & (1 << t.0) != 0
}

/// Timer preemptions that interrupted ring 3.
pub static USER_PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// Base of the kernel-stack window (PML4 slot 506). Slot `i` is a 4 KiB
/// unmapped guard page followed by a 16 KiB stack.
const KSTACK_BASE: u64 = 0xffff_fd00_0000_0000;
const KSTACK_BYTES: u64 = 16 * 1024;
const KSTACK_SLOT: u64 = PAGE + KSTACK_BYTES;

/// Record the kernel address space's root, which is in `CR3` now. Call once,
/// right after the kernel space is installed.
pub fn init_kernel_root(root: u64) {
    KERNEL_ROOT.store(root, Ordering::Relaxed);
    CURRENT_ROOT.store(root, Ordering::Relaxed);
}

/// Map a fresh 16 KiB kernel stack in slot `slot` of the kernel-stack window,
/// leaving the guard page below it unmapped. Returns the stack top, or `None`
/// if frames ran out or the slot is taken.
pub fn alloc_kernel_stack(slot: usize) -> Option<u64> {
    let bottom = KSTACK_BASE + slot as u64 * KSTACK_SLOT + PAGE;
    hal::cpu::without_interrupts(|| {
        let mut guard = KERNEL_SPACE.lock();
        let space = guard.as_mut()?;
        for page in 0..KSTACK_BYTES / PAGE {
            let frame = hal::frame::alloc_frame()?;
            space
                .map(
                    bottom + page * PAGE,
                    frame.addr(),
                    Flags::WRITABLE | Flags::NO_EXECUTE,
                )
                .ok()?;
        }
        Some(bottom + KSTACK_BYTES)
    })
}

/// Unmap the kernel stack in window slot `slot` and free its frames. The
/// guard page below it was never mapped.
pub fn free_kernel_stack(slot: usize) -> usize {
    let bottom = KSTACK_BASE + slot as u64 * KSTACK_SLOT + PAGE;
    hal::cpu::without_interrupts(|| {
        let mut guard = KERNEL_SPACE.lock();
        let Some(space) = guard.as_mut() else {
            return 0;
        };
        let mut freed = 0;
        for page in 0..KSTACK_BYTES / PAGE {
            let va = bottom + page * PAGE;
            if let Some(phys) = space.translate(va) {
                space.unmap(va);
                hal::frame::free_frame(hal::frame::PhysFrame::containing(phys));
                freed += 1;
            }
        }
        freed
    })
}

/// Give thread `t` its own kernel stack, from window slot `slot`, used for
/// every ring-3 entry.
pub fn set_kernel_stack(t: ThreadId, slot: usize, top: u64) {
    STACK_SLOT_OF[t.0].store(slot as u64 + 1, Ordering::Relaxed);
    KSTACK_TOP[t.0].store(top, Ordering::Relaxed);
}

/// Reclaim an exited thread `t`: tear down its address space and free its
/// kernel stack. Returns the frames freed.
///
/// # Panics
/// If `t` has not exited, or is still on the run queue: a thread blocked in
/// IPC is off the run queue but still owns its stack and address space.
pub fn reap(t: ThreadId) -> usize {
    let queued = hal::cpu::without_interrupts(|| SCHED.lock().contains(t));
    assert!(
        has_exited(t) && !queued,
        "thread: reaping live thread {}",
        t.0
    );
    let space = hal::cpu::without_interrupts(|| SPACES.lock()[t.0].take());
    let mut freed = space.map_or(0, AddressSpace::destroy_user);
    let slot = STACK_SLOT_OF[t.0].swap(0, Ordering::Relaxed);
    KSTACK_TOP[t.0].store(0, Ordering::Relaxed);
    if slot != 0 {
        freed += free_kernel_stack(slot as usize - 1);
    }
    crate::user::forget(t);
    EXITED.fetch_and(!(1 << t.0), Ordering::Release);
    freed
}

/// Give thread `t` its own address space.
pub fn set_address_space(t: ThreadId, space: AddressSpace) {
    hal::cpu::without_interrupts(|| SPACES.lock()[t.0] = Some(space));
}

/// Whether thread `t`'s own address space maps `virt`.
pub fn user_maps(t: ThreadId, virt: u64) -> bool {
    hal::cpu::without_interrupts(|| {
        SPACES.lock()[t.0]
            .as_ref()
            .is_some_and(|s| s.translate(virt).is_some())
    })
}

/// Switch from thread `cur` to `next`. Points `TSS.rsp0` and the syscall entry
/// at `next`'s kernel stack, loads `next`'s address space (or the kernel's) if
/// it differs from the one in `CR3`, then switches contexts.
///
/// Call with interrupts off and no lock held; returns when `cur` next runs.
pub fn switch_to(cur: ThreadId, next: ThreadId) {
    let top = KSTACK_TOP[next.0].load(Ordering::Relaxed);
    if top != 0 {
        hal::gdt::set_rsp0(top);
        hal::syscall::set_kernel_stack(top);
    }
    let kernel = KERNEL_ROOT.load(Ordering::Relaxed);
    let spaces = SPACES.lock();
    let want = spaces[next.0]
        .as_ref()
        .map_or(kernel, AddressSpace::root_phys);
    if want != 0 && want != CURRENT_ROOT.load(Ordering::Relaxed) {
        match spaces[next.0].as_ref() {
            Some(space) => space.switch(),
            None => {
                if let Some(space) = KERNEL_SPACE.lock().as_ref() {
                    space.switch();
                }
            }
        }
        CURRENT_ROOT.store(want, Ordering::Relaxed);
    }
    drop(spaces);
    hal::context::switch(&CONTEXTS[cur.0], &CONTEXTS[next.0]);
}

/// Check a kernel stack's guard page: the page below the stack faults, the
/// stack itself does not. Uses slot 0, which stays reserved for this test.
pub fn selftest() -> bool {
    let Some(top) = alloc_kernel_stack(0) else {
        return false;
    };
    let guard_faults = hal::fault::probe_read(top - KSTACK_BYTES - 8);
    let stack_reads = !hal::fault::probe_read(top - 8);
    guard_faults && stack_reads
}
