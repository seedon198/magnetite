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

use capability::{ObjectKind, ObjectRef};
use hal::context::Context;
use hal::paging::{AddressSpace, Flags, PAGE};
use hal::sync::SpinLock;
use sched::ThreadId;

use crate::{KERNEL_SPACE, SCHED};

/// Thread slots: boot (0), cooperative workers (1, 2), preemption spinners
/// (3, 4), IPC demo server and client (5, 6), user demo tasks (7 to 11), the
/// console server (12), the fault task (13), the root task (14) and the user
/// demo's entry-path checks (15 to 17), its SSE-state check (18, 19) and the
/// holder the root task's capabilities are granted to (20); then the threads
/// tasks create, as Thread objects (21 to 63,
/// [ADR 0023](../../docs/adr/0023-user-threads-and-address-spaces.md)).
pub const MAX: usize = 64;

/// The first slot for a Thread object.
const FIRST_OBJECT: usize = 21;

/// Where a Thread object's memory is mapped as its kernel stack: window slot
/// `OBJECT_WINDOW + id`, clear of the fixed slots boot threads use.
const OBJECT_WINDOW: usize = 64;

/// The Thread object bound to each slot, by its storage's physical address.
/// A leaf lock, never held while taking another; the callers already hold
/// `CSPACES`, which keeps binds and unbinds from interleaving.
static OBJECTS: SpinLock<[Option<u64>; MAX]> = SpinLock::new([None; MAX]);

/// Each thread's FPU and SSE state (#83), swapped at every context switch.
static FPU: [hal::fpu::FpuArea; MAX] = [const { hal::fpu::FpuArea::new() }; MAX];

/// Saved contexts, indexed by [`ThreadId`].
pub static CONTEXTS: [Context; MAX] = [const { Context::new() }; MAX];

/// The kernel-stack window slot the boot thread moves onto (#66). Slot 0 is
/// the guard self-test's; tasks and demos use slots 1 to 10.
pub const BOOT_STACK_SLOT: usize = 11;

/// Each thread's kernel stack top (0 = runs on the stack it was started on).
static KSTACK_TOP: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

/// Each thread's kernel-stack window slot, plus one (0 = none), for reaping.
static STACK_SLOT_OF: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

/// A thread's own address space.
enum Space {
    /// Built by the kernel's loader, which owns every page of its user half.
    Owned(AddressSpace),
    /// An AddressSpace object, by its root's physical address: every page
    /// belongs to an Untyped (ADR 0023).
    Object(u64),
}

impl Space {
    fn root(&self) -> u64 {
        match self {
            Space::Owned(s) => s.root_phys(),
            Space::Object(root) => *root,
        }
    }
}

/// Each thread's own address space; `None` runs in the kernel's.
static SPACES: SpinLock<[Option<Space>; MAX]> = SpinLock::new([const { None }; MAX]);

/// Where each Thread object starts in ring 3, set by `configure`.
static USER_RIP: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];
static USER_RSP: [AtomicU64; MAX] = [const { AtomicU64::new(0) }; MAX];

/// Threads with one bit each: configured to run; started and not stopped;
/// asked to stop while running (they switch away on their way out of the
/// kernel); and destroyed while running, their stacks to unmap once another
/// thread runs.
static CONFIGURED: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicU64 = AtomicU64::new(0);
static STOP_SELF: AtomicU64 = AtomicU64::new(0);
static ZOMBIES: AtomicU64 = AtomicU64::new(0);

fn bit(t: ThreadId) -> u64 {
    1 << t.0
}

/// A slot whose object is gone but whose stack is still to unmap: it matches
/// no object's address.
const ZOMBIE: u64 = u64::MAX;

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

/// Yields a boot-time wait for a task allows before giving up (#136): far more
/// than any boot task needs, yet a stalled task fails the boot with its name
/// in seconds instead of hanging until CI's timeout. Counted in yields, not
/// timer ticks, since ticks do not advance under cooperative scheduling.
const EXIT_WAIT_YIELDS: u64 = 2_000_000;

/// Yield until `t` (described as `what`) has exited.
///
/// # Panics
/// If it has not exited after [`EXIT_WAIT_YIELDS`] yields.
pub fn wait_exit(t: ThreadId, what: &str) {
    for _ in 0..EXIT_WAIT_YIELDS {
        if has_exited(t) {
            return;
        }
        crate::yield_now();
    }
    let runnable = hal::cpu::without_interrupts(|| SCHED.lock().contains(t));
    panic!(
        "thread: {what} (thread {}) did not exit after {EXIT_WAIT_YIELDS} yields ({})",
        t.0,
        if runnable {
            "still runnable"
        } else {
            "blocked"
        }
    );
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
/// The window's size: its one PML4 slot (512 GiB).
const KSTACK_WINDOW: u64 = 1 << 39;

/// Name `addr` if it lies in the kernel-stack window: a slot's guard page or
/// its stack (#125). Registered with `hal::fault` as a classifier, since the
/// window holds a guard per slot, too many for the region table.
pub fn classify_stack(addr: u64) -> Option<&'static str> {
    let off = addr.checked_sub(KSTACK_BASE)?;
    if off >= KSTACK_WINDOW {
        return None;
    }
    Some(if off % KSTACK_SLOT < PAGE {
        "kernel stack guard"
    } else {
        "kernel stack"
    })
}

/// A guard-page address and a stack address in kernel-stack window slot
/// `slot`, for the fault classifier's self-check.
pub fn kernel_stack_probe(slot: usize) -> (u64, u64) {
    let guard = KSTACK_BASE + slot as u64 * KSTACK_SLOT;
    (guard, guard + PAGE)
}

/// Whether a slot is free for a new Thread object.
pub fn has_free_object_slot() -> bool {
    OBJECTS.lock()[FIRST_OBJECT..].iter().any(Option::is_none)
}

/// How many Thread objects are bound to a slot.
pub fn object_threads() -> usize {
    OBJECTS.lock().iter().flatten().count()
}

/// The slot Thread object `obj` is bound to.
pub fn of_object(obj: ObjectRef) -> Option<ThreadId> {
    OBJECTS
        .lock()
        .iter()
        .position(|o| *o == Some(obj.phys))
        .map(ThreadId)
}

/// Bind the freshly retyped Thread object `obj` to a free slot: its 16 KiB
/// become that slot's kernel stack, mapped in the window behind an unmapped
/// guard page. `None`, with nothing changed, if no slot is free or a window
/// table could not be made.
pub fn bind_object(obj: ObjectRef) -> Option<ThreadId> {
    assert_eq!(obj.kind, ObjectKind::Thread);
    let t = {
        let objects = OBJECTS.lock();
        (FIRST_OBJECT..MAX).find(|&t| objects[t].is_none())?
    };
    let bottom = window_bottom(OBJECT_WINDOW + t);
    let mapped = hal::cpu::without_interrupts(|| {
        let mut guard = KERNEL_SPACE.lock();
        let space = guard.as_mut()?;
        for page in 0..KSTACK_BYTES / PAGE {
            let (va, pa) = (bottom + page * PAGE, obj.phys + page * PAGE);
            if space
                .map(va, pa, Flags::WRITABLE | Flags::NO_EXECUTE)
                .is_err()
            {
                (0..page).for_each(|p| {
                    space.unmap(bottom + p * PAGE);
                });
                return None;
            }
        }
        Some(())
    });
    mapped?;
    OBJECTS.lock()[t] = Some(obj.phys);
    KSTACK_TOP[t].store(bottom + KSTACK_BYTES, Ordering::Relaxed);
    Some(ThreadId(t))
}

/// Unbind Thread object `obj`, being destroyed: stop the thread, then unmap
/// its kernel stack (the frames are the object's, so they stay with its
/// Untyped) and free its slot. If it is the running thread, which is on that
/// stack now, the unmapping waits until another thread runs. Call with `IPC`
/// and `CSPACES` held.
pub fn unbind_object(ipc: &mut ::ipc::Ipc, obj: ObjectRef) {
    let Some(t) = of_object(obj) else {
        return;
    };
    stop(ipc, t);
    CONFIGURED.fetch_and(!bit(t), Ordering::Relaxed);
    hal::cpu::without_interrupts(|| SPACES.lock()[t.0] = None);
    if current() == Some(t) {
        OBJECTS.lock()[t.0] = Some(ZOMBIE);
        ZOMBIES.fetch_or(bit(t), Ordering::Relaxed);
    } else {
        release_slot(t);
    }
}

/// Unmap slot `t`'s object stack and free the slot.
fn release_slot(t: ThreadId) {
    let bottom = window_bottom(OBJECT_WINDOW + t.0);
    hal::cpu::without_interrupts(|| {
        if let Some(space) = KERNEL_SPACE.lock().as_mut() {
            for page in 0..KSTACK_BYTES / PAGE {
                space.unmap(bottom + page * PAGE);
            }
        }
    });
    KSTACK_TOP[t.0].store(0, Ordering::Relaxed);
    FPU[t.0].reset();
    OBJECTS.lock()[t.0] = None;
}

/// Release the slots of destroyed threads that were running when destroyed,
/// now that another thread runs.
fn reap_zombies() {
    let me = current();
    let mut zombies = ZOMBIES.load(Ordering::Relaxed);
    while zombies != 0 {
        let t = ThreadId(zombies.trailing_zeros() as usize);
        zombies &= zombies - 1;
        if Some(t) != me {
            ZOMBIES.fetch_and(!bit(t), Ordering::Relaxed);
            release_slot(t);
        }
    }
}

fn current() -> Option<ThreadId> {
    hal::cpu::without_interrupts(|| SCHED.lock().current())
}

/// Bind Thread `t` to the address space `root` and set where it starts in
/// ring 3. Its capability space is attached separately, in the registry.
pub fn configure(t: ThreadId, root: ObjectRef, entry: u64, stack: u64) {
    assert_eq!(root.kind, ObjectKind::AddressSpace);
    hal::cpu::without_interrupts(|| SPACES.lock()[t.0] = Some(Space::Object(root.phys)));
    USER_RIP[t.0].store(entry, Ordering::Relaxed);
    USER_RSP[t.0].store(stack, Ordering::Relaxed);
    CONFIGURED.fetch_or(bit(t), Ordering::Relaxed);
}

/// Whether Thread `t` has started and is neither stopped nor exited.
pub fn is_running(t: ThreadId) -> bool {
    RUNNING.load(Ordering::Relaxed) & bit(t) != 0 && !has_exited(t)
}

/// Start configured Thread `t` at its entry, unless it is running. Returns
/// `false` if it was never configured.
pub fn resume(t: ThreadId) -> bool {
    if CONFIGURED.load(Ordering::Relaxed) & bit(t) == 0 {
        return false;
    }
    if is_running(t) {
        return true;
    }
    EXITED.fetch_and(!bit(t), Ordering::Release);
    RUNNING.fetch_or(bit(t), Ordering::Relaxed);
    CONTEXTS[t.0].init(KSTACK_TOP[t.0].load(Ordering::Relaxed), object_entry);
    hal::cpu::without_interrupts(|| SCHED.lock().admit(t));
    true
}

/// Stop thread `t` wherever it is: out of IPC, and off the run queue. A
/// running thread stops itself on its way out of the kernel
/// ([`stop_if_requested`]). Call with `IPC` held.
pub fn stop(ipc: &mut ::ipc::Ipc, t: ThreadId) {
    crate::ipc::stop(ipc, t);
    RUNNING.fetch_and(!bit(t), Ordering::Relaxed);
    hal::cpu::without_interrupts(|| {
        let mut sched = SCHED.lock();
        if sched.current() == Some(t) {
            STOP_SELF.fetch_or(bit(t), Ordering::Relaxed);
        } else {
            sched.remove(t);
        }
    });
}

/// Stop every thread running in address space `root`, which is being
/// destroyed, and unbind them from it. Call with `IPC` held.
pub fn stop_space_users(ipc: &mut ::ipc::Ipc, root: ObjectRef) {
    for t in (0..MAX).map(ThreadId) {
        let uses = hal::cpu::without_interrupts(
            || matches!(SPACES.lock()[t.0], Some(Space::Object(r)) if r == root.phys),
        );
        if uses {
            stop(ipc, t);
            CONFIGURED.fetch_and(!bit(t), Ordering::Relaxed);
            hal::cpu::without_interrupts(|| SPACES.lock()[t.0] = None);
        }
    }
}

/// If the running thread was stopped while it ran, leave the run queue and
/// switch away for good, never returning to ring 3. Call on the way out of a
/// syscall, with no lock held.
pub fn stop_if_requested() {
    hal::cpu::without_interrupts(|| {
        let mut sched = SCHED.lock();
        let Some(me) = sched.current() else {
            return;
        };
        if STOP_SELF.fetch_and(!bit(me), Ordering::Relaxed) & bit(me) == 0 {
            return;
        }
        sched.remove(me);
        let next = sched
            .current()
            .expect("thread: the boot thread never stops");
        drop(sched);
        switch_to(me, next);
        unreachable!("thread: a stopped thread was resumed without a restart");
    });
}

/// A Thread object's first code, on its own kernel stack and in its own
/// address space: drop to ring 3 at its configured entry.
extern "C" fn object_entry() -> ! {
    reap_zombies();
    let t = current().expect("thread: a running thread");
    let entry = hal::syscall::UserEntry {
        rip: USER_RIP[t.0].load(Ordering::Relaxed),
        rsp: USER_RSP[t.0].load(Ordering::Relaxed),
        cap: 0,
        msg_info: 0,
        mr: [0; 4],
    };
    hal::syscall::enter_user_once(&entry)
}


/// The lowest address of window slot `slot`'s stack, above its guard page.
fn window_bottom(slot: usize) -> u64 {
    KSTACK_BASE + slot as u64 * KSTACK_SLOT + PAGE
}

/// Record the kernel address space's root, which is in `CR3` now. Call once,
/// right after the kernel space is installed.
pub fn init_kernel_root(root: u64) {
    KERNEL_ROOT.store(root, Ordering::Relaxed);
    CURRENT_ROOT.store(root, Ordering::Relaxed);
}

/// Map a fresh 16 KiB kernel stack in slot `slot` of the kernel-stack window,
/// leaving the guard page below it unmapped. Returns the stack top, or `None`
/// if frames ran out or the slot is taken. On `None` nothing is left
/// allocated, and a taken slot is never touched (#130), so a caller must not
/// call [`free_kernel_stack`] for it.
pub fn alloc_kernel_stack(slot: usize) -> Option<u64> {
    let bottom = KSTACK_BASE + slot as u64 * KSTACK_SLOT + PAGE;
    let pages = KSTACK_BYTES / PAGE;
    hal::cpu::without_interrupts(|| {
        let mut guard = KERNEL_SPACE.lock();
        let space = guard.as_mut()?;
        if (0..pages).any(|p| space.translate(bottom + p * PAGE).is_some()) {
            return None;
        }
        for page in 0..pages {
            let va = bottom + page * PAGE;
            let mapped = hal::frame::alloc_frame().is_some_and(|frame| {
                let ok = space
                    .map(va, frame.addr(), Flags::WRITABLE | Flags::NO_EXECUTE)
                    .is_ok();
                if !ok {
                    hal::frame::free_frame(frame);
                }
                ok
            });
            if !mapped {
                // Undo the pages this call mapped; the slot was free before.
                for done in (0..page).map(|p| bottom + p * PAGE) {
                    if let Some(phys) = space.translate(done) {
                        space.unmap(done);
                        hal::frame::free_frame(hal::frame::PhysFrame::containing(phys));
                    }
                }
                return None;
            }
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
    let mut freed = match space {
        Some(Space::Owned(s)) => s.destroy_user(),
        _ => 0,
    };
    let slot = STACK_SLOT_OF[t.0].swap(0, Ordering::Relaxed);
    KSTACK_TOP[t.0].store(0, Ordering::Relaxed);
    if slot != 0 {
        freed += free_kernel_stack(slot as usize - 1);
    }
    crate::user::forget(t);
    FPU[t.0].reset();
    EXITED.fetch_and(!(1 << t.0), Ordering::Release);
    freed
}

/// Give thread `t` its own address space.
pub fn set_address_space(t: ThreadId, space: AddressSpace) {
    hal::cpu::without_interrupts(|| SPACES.lock()[t.0] = Some(Space::Owned(space)));
}

/// The root of the address space the kernel's loader built for `t`.
pub fn owned_root(t: ThreadId) -> Option<u64> {
    hal::cpu::without_interrupts(|| match &SPACES.lock()[t.0] {
        Some(Space::Owned(s)) => Some(s.root_phys()),
        _ => None,
    })
}

/// Whether thread `t`'s own address space maps `virt`.
pub fn user_maps(t: ThreadId, virt: u64) -> bool {
    hal::cpu::without_interrupts(|| match &SPACES.lock()[t.0] {
        Some(Space::Owned(s)) => s.translate(virt).is_some(),
        Some(Space::Object(root)) => {
            let root = ObjectRef::new(ObjectKind::AddressSpace, *root);
            hal::vspace::translate(root, virt, crate::ipc::hhdm()).is_some()
        }
        None => false,
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
    let want = spaces[next.0].as_ref().map_or(kernel, Space::root);
    if want != 0 && want != CURRENT_ROOT.load(Ordering::Relaxed) {
        match spaces[next.0].as_ref() {
            Some(Space::Owned(space)) => space.switch(),
            Some(Space::Object(root)) => {
                hal::vspace::activate(ObjectRef::new(ObjectKind::AddressSpace, *root));
            }
            None => {
                if let Some(space) = KERNEL_SPACE.lock().as_ref() {
                    space.switch();
                }
            }
        }
        CURRENT_ROOT.store(want, Ordering::Relaxed);
    }
    drop(spaces);
    // FPU and SSE registers belong to the thread: save them for the one
    // leaving and load the one arriving (#83). The kernel never uses them.
    FPU[cur.0].save();
    FPU[next.0].restore();
    hal::context::switch(&CONTEXTS[cur.0], &CONTEXTS[next.0]);
    // Back in this thread: the one that destroyed its own Thread object is
    // no longer running, so its stack can go.
    reap_zombies();
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
