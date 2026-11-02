//! Loading user programs (#82, [ADR 0019](../../docs/adr/0019-first-user-task.md)
//! decisions 1 and 2).
//!
//! A task is a separately built ELF program the bootloader loaded as a boot
//! module. [`spawn`] validates it with the pure `elf` crate before anything is
//! mapped, then builds the task's own address space: every segment's pages
//! with exactly its permissions (never `W+X`), and a stack with an unmapped
//! guard page below it. A refused or half-built image is torn down and the
//! kernel boots on.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use boot_info::BootInfo;
use capability::{cnode_bytes, CapId, Capability, ObjectKind, ObjectRef, Rights, Untyped};
use elf::ElfError;
use hal::frame::PhysFrame;
use hal::paging::{AddressSpace, Flags, PAGE};
use hal::syscall::UserEntry;
use sched::ThreadId;

use crate::thread::{self, CONTEXTS};
use crate::{boot, console, KERNEL_SPACE, SCHED};

/// The top of every task's stack.
pub const STACK_TOP: u64 = 0x7fff_ffff_0000;
/// Pages of stack below [`STACK_TOP`]; the page below them stays unmapped.
pub const STACK_PAGES: u64 = 4;
/// Segments must end at or below this: under the stack and its guard page.
const ELF_LIMIT: u64 = STACK_TOP - (STACK_PAGES + 1) * PAGE;

/// The fault task's thread and kernel-stack slot.
const FAULT: ThreadId = ThreadId(13);
const FAULT_STACK_SLOT: usize = 6;
/// The console server's thread, and the root task's thread and stack slot.
const CONSOLE: ThreadId = ThreadId(12);
const ROOT: ThreadId = ThreadId(14);
const ROOT_STACK_SLOT: usize = 7;

/// Slots in the root task's CNode, and its Untyped memory in frames (64 KiB).
const ROOT_SLOTS: usize = 64;
const ROOT_UNTYPED_FRAMES: usize = 16;

/// Each task's entry point, for its first switch into ring 3.
static ENTRY: [AtomicU64; thread::MAX] = [const { AtomicU64::new(0) }; thread::MAX];

/// Why a program could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// The image broke a rule of the ELF validator.
    Elf(ElfError),
    /// Frames ran out.
    OutOfMemory,
    /// A page could not be mapped.
    Map,
}

/// Load `image` as thread `t`, with its kernel stack in window slot
/// `stack_slot`, and make it runnable.
///
/// # Errors
/// The image is refused, or memory ran out; nothing of the task remains.
pub fn spawn(t: ThreadId, stack_slot: usize, image: &[u8]) -> Result<(), LoadError> {
    let elf = elf::parse(image, ELF_LIMIT).map_err(LoadError::Elf)?;
    let mut space = hal::cpu::without_interrupts(|| {
        KERNEL_SPACE
            .lock()
            .as_ref()
            .and_then(AddressSpace::new_user)
    })
    .ok_or(LoadError::OutOfMemory)?;
    if let Err(e) = map_image(&mut space, &elf, image) {
        space.destroy_user();
        return Err(e);
    }
    // A refused slot leaves nothing allocated, so there is nothing to free
    // here, and never another thread's stack (#130).
    let Some(kstack) = thread::alloc_kernel_stack(stack_slot) else {
        space.destroy_user();
        return Err(LoadError::OutOfMemory);
    };
    ENTRY[t.0].store(elf.entry, Ordering::Relaxed);
    thread::set_kernel_stack(t, stack_slot, kstack);
    thread::set_address_space(t, space);
    CONTEXTS[t.0].init(kstack, task_entry);
    hal::cpu::without_interrupts(|| SCHED.lock().admit(t));
    Ok(())
}

/// Map every page of every segment of `elf` into `space`, with the file's
/// bytes copied in and the rest zero, and then the stack. On error, the pages
/// already mapped stay in `space` for the caller to tear down.
fn map_image(space: &mut AddressSpace, elf: &elf::Image, image: &[u8]) -> Result<(), LoadError> {
    let hhdm = crate::ipc::hhdm();
    for seg in elf.segments() {
        let mut flags = Flags::USER;
        if seg.writable {
            flags |= Flags::WRITABLE;
        }
        if !seg.executable {
            flags |= Flags::NO_EXECUTE;
        }
        // Validated: the segment lies inside the file and below ELF_LIMIT.
        let file_end = seg.vaddr + seg.file_size;
        let mut page = seg.vaddr & !(PAGE - 1);
        while page < seg.vaddr + seg.mem_size {
            let frame = hal::frame::alloc_frame().ok_or(LoadError::OutOfMemory)?;
            let (from, to) = (page.max(seg.vaddr), (page + PAGE).min(file_end));
            if from < to {
                let off = (seg.offset + (from - seg.vaddr)) as usize;
                let bytes = &image[off..off + (to - from) as usize];
                hal::object::write_phys(frame.addr() + (from - page), hhdm, bytes);
            }
            if space.map(page, frame.addr(), flags).is_err() {
                hal::frame::free_frame(frame);
                return Err(LoadError::Map);
            }
            page += PAGE;
        }
    }
    for i in 1..=STACK_PAGES {
        let frame = hal::frame::alloc_frame().ok_or(LoadError::OutOfMemory)?;
        let stack = Flags::USER | Flags::WRITABLE | Flags::NO_EXECUTE;
        if space
            .map(STACK_TOP - i * PAGE, frame.addr(), stack)
            .is_err()
        {
            hal::frame::free_frame(frame);
            return Err(LoadError::Map);
        }
    }
    Ok(())
}

/// A task's first code, on its own kernel stack and in its own address space
/// (`switch_to` installed both): drop to ring 3 at the program's entry point,
/// for good.
extern "C" fn task_entry() -> ! {
    let t =
        hal::cpu::without_interrupts(|| SCHED.lock().current()).expect("task: a running thread");
    let entry = UserEntry {
        rip: ENTRY[t.0].load(Ordering::Relaxed),
        rsp: STACK_TOP,
        cap: 0,
        msg_info: 0,
        mr: [0; 4],
    };
    hal::syscall::enter_user_once(&entry)
}

/// The kernel memory behind the root task's capabilities, freed by
/// [`reap_root`].
pub struct Bootstrap {
    cnode: PhysFrame,
    control: PhysFrame,
    untyped: PhysFrame,
}

/// Create the root task as thread `t` (ADR 0019 decision 7): a 64-slot CNode
/// holding exactly its CNode (slot 1, `READ|WRITE`), 64 KiB of Untyped memory
/// (slot 2, `RETYPE`) and a send-only console capability with badge 1
/// (slot 3), with slot 0 empty; then load `image` into it.
///
/// # Errors
/// Memory ran out or the image was refused; nothing of the task remains.
pub fn bootstrap_root(
    t: ThreadId,
    console: ObjectRef,
    image: &[u8],
) -> Result<Bootstrap, LoadError> {
    let hhdm = crate::ipc::hhdm();
    assert!(cnode_bytes(ROOT_SLOTS) as u64 <= PAGE);
    let (Some(cnode), Some(control)) = (hal::frame::alloc_frame(), hal::frame::alloc_frame())
    else {
        return Err(LoadError::OutOfMemory);
    };
    let Some(untyped) = hal::frame::alloc_contiguous(ROOT_UNTYPED_FRAMES) else {
        hal::frame::free_frame(cnode);
        hal::frame::free_frame(control);
        return Err(LoadError::OutOfMemory);
    };
    let boot = Bootstrap {
        cnode,
        control,
        untyped,
    };
    let region = ROOT_UNTYPED_FRAMES as u64 * PAGE;
    hal::object::store_untyped(control.addr(), hhdm, Untyped::new(untyped.addr(), region));

    // `cnode` is a fresh frame, ours alone, and gets exactly one CapSpace.
    let mut space = hal::object::cnode_cap_space(cnode.addr(), ROOT_SLOTS, hhdm);
    let own = ObjectRef::new(ObjectKind::CNode, cnode.addr());
    let caps = [
        (1, Capability::new(own, Rights::READ | Rights::WRITE)),
        (
            2,
            Capability::new(
                ObjectRef::new(ObjectKind::Untyped, control.addr()),
                Rights::RETYPE,
            ),
        ),
        (
            3,
            Capability {
                object: console,
                rights: Rights::SEND,
                badge: console::ROOT_BADGE,
            },
        ),
    ];
    for (slot, cap) in caps {
        space.insert_at(CapId(slot), cap).expect("task: root slot");
    }
    crate::ipc::set_cspace(t, own, space);
    if let Err(e) = spawn(t, ROOT_STACK_SLOT, image) {
        crate::ipc::take_cspace(t);
        if reclaim_memory(boot).is_err() {
            crate::warn!("task: a refused root task's memory is still named; kept");
        }
        return Err(e);
    }
    Ok(boot)
}

/// Memory behind an exited task's capabilities that something still names, so
/// it was not freed: the capability that holds it, and the memory to retry.
pub struct Unreclaimed {
    /// The thread and slot of a capability into the task's memory.
    pub holder: (ThreadId, CapId),
    /// The task's memory, still allocated.
    pub boot: Bootstrap,
}

/// Reclaim the exited root task: its address space and kernel stack, its
/// capability space, and the memory behind its capabilities. Returns the
/// frames freed.
///
/// # Errors
/// [`Unreclaimed`] if a capability elsewhere still names the task's memory:
/// there is no revoke yet (#105), so freeing it would let that capability
/// reach whatever reuses the frames. The thread and capability space are
/// reclaimed either way; retry the memory with [`reclaim_memory`].
pub fn reap_root(t: ThreadId, boot: Bootstrap) -> Result<usize, Unreclaimed> {
    let freed = thread::reap(t);
    crate::ipc::take_cspace(t);
    reclaim_memory(boot).map(|n| n + freed)
}

/// Free a task's CNode, Untyped control block and Untyped region, unless a
/// capability in any capability space still names an object inside them.
///
/// # Errors
/// [`Unreclaimed`], with the memory untouched, if one does.
pub fn reclaim_memory(boot: Bootstrap) -> Result<usize, Unreclaimed> {
    let untyped = boot.untyped.addr();
    let regions = [
        (boot.cnode.addr(), boot.cnode.addr() + PAGE),
        (boot.control.addr(), boot.control.addr() + PAGE),
        (untyped, untyped + ROOT_UNTYPED_FRAMES as u64 * PAGE),
    ];
    for (start, end) in regions {
        if let Some(holder) = crate::ipc::find_reference(start, end) {
            return Err(Unreclaimed { holder, boot });
        }
    }
    hal::frame::free_frame(boot.cnode);
    hal::frame::free_frame(boot.control);
    hal::frame::free_contiguous(boot.untyped, ROOT_UNTYPED_FRAMES);
    Ok(2 + ROOT_UNTYPED_FRAMES)
}

/// Frames currently free.
fn free_frames() -> usize {
    hal::frame::stats().map_or(0, |s| s.free)
}

/// Load the fault task, and a corrupted copy of it: the copy is refused before
/// anything is mapped, the task is killed by its page fault, and every frame
/// it used comes back.
///
/// A missing or malformed module is logged and skipped, and the kernel boots
/// on without it (ADR 0019 decision 1); CI's boot test still fails, since it
/// requires the root task's success line.
pub fn demo(boot_info: &BootInfo) {
    match boot_info.module("fault").map(boot::module_bytes) {
        Some(fault) => fault_demo(fault),
        None => crate::warn!("task: no fault module; skipped"),
    }
    let console = console::start(CONSOLE);
    match boot_info.module("root").map(boot::module_bytes) {
        Some(root) => root_demo(console, root),
        None => crate::warn!("task: no root module; skipped"),
    }
}

/// The fault task, and a corrupted copy of it.
fn fault_demo(fault: &[u8]) {
    if let Err(e) = elf::parse(fault, ELF_LIMIT) {
        crate::warn!("task: fault module refused ({e:?}); skipped");
        return;
    }
    let before = free_frames();

    let mut corrupted: Vec<u8> = fault.to_vec();
    corrupted[0] ^= 0xff;
    let refused = spawn(FAULT, FAULT_STACK_SLOT, &corrupted);
    assert_eq!(refused, Err(LoadError::Elf(ElfError::BadMagic)));
    drop(corrupted);
    crate::info!("task: refused a corrupted image (BadMagic)");

    if let Err(e) = spawn(FAULT, FAULT_STACK_SLOT, fault) {
        crate::warn!("task: fault task not loaded ({e:?}); skipped");
        return;
    }
    thread::wait_exit(FAULT, "the fault task");
    thread::reap(FAULT);
    assert_eq!(free_frames(), before, "task: the fault task leaked frames");
    crate::info!("task: fault task contained and reclaimed");
}

/// The root task, talking to the console server over IPC.
fn root_demo(console: ObjectRef, root: &[u8]) {
    let before = free_frames();
    let boot = match bootstrap_root(ROOT, console, root) {
        Ok(boot) => boot,
        Err(e) => {
            assert_eq!(
                free_frames(),
                before,
                "task: a refused root task leaked frames"
            );
            crate::warn!("task: root module refused ({e:?}); skipped");
            return;
        }
    };
    thread::wait_exit(ROOT, "the root task");
    assert!(
        console::ROOT_PASSED.load(Ordering::Acquire),
        "task: the root task did not report its checks passed"
    );
    let hhdm = crate::ipc::hhdm();
    let signalled = crate::ipc::with_cspace(ROOT, |cs, _| {
        cs.get(CapId(4)).is_ok_and(|c| {
            c.object.kind == ObjectKind::Notification
                && hal::object::notification_pending(c.object.phys, hhdm) == 0b101
        })
    });
    assert_eq!(
        signalled,
        Some(true),
        "task: slot 4 does not hold the Notification root signalled"
    );
    // A capability that outlives the task must keep its memory alive (#105):
    // plant one into root's Untyped region in the console server's space, as
    // a GRANT would, and the memory must stay allocated until it is gone.
    let planted = ObjectRef::new(ObjectKind::Endpoint, boot.untyped.addr() + 2 * PAGE);
    let slot = crate::ipc::with_cspace(CONSOLE, |cs, _| {
        cs.insert(Capability::new(planted, Rights::SEND))
    })
    .flatten()
    .expect("task: the console server has a free slot");
    let Err(kept) = reap_root(ROOT, boot) else {
        panic!("task: freed memory that a capability still names");
    };
    assert_eq!(kept.holder, (CONSOLE, slot), "task: wrong holder reported");
    crate::ipc::with_cspace(CONSOLE, |cs, _| cs.delete(slot)).map(|r| r.expect("planted"));
    if reclaim_memory(kept.boot).is_err() {
        panic!("task: memory still held after its last capability went");
    }
    crate::info!("task: memory a capability still names is kept until the capability goes");
    assert_eq!(free_frames(), before, "task: the root task leaked frames");
    crate::info!(
        "task: root task confined to 4 capabilities; console via IPC; all memory reclaimed"
    );
}
