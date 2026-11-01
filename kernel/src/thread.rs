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

use crate::KERNEL_SPACE;

/// Thread slots: boot (0), cooperative workers (1, 2), preemption spinners
/// (3, 4), IPC demo server and client (5, 6), user demo tasks (7 to 11).
pub const MAX: usize = 12;

/// Saved contexts, indexed by [`ThreadId`].
pub static CONTEXTS: [Context; MAX] = [const { Context::new() }; MAX];

/// Each thread's kernel stack top (0 = runs on the stack it was started on).
static KSTACK_TOP: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

/// Each thread's own address space; `None` runs in the kernel's.
static SPACES: SpinLock<[Option<AddressSpace>; MAX]> = SpinLock::new([const { None }; MAX]);

/// The kernel address space's root, and the root currently in `CR3`.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);
static CURRENT_ROOT: AtomicU64 = AtomicU64::new(0);

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

/// Give thread `t` its own kernel stack, used for every ring-3 entry.
pub fn set_kernel_stack(t: ThreadId, top: u64) {
    KSTACK_TOP[t.0].store(top, Ordering::Relaxed);
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
