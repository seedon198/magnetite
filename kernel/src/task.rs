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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ::ipc::{IpcError, Message};
use boot_info::BootInfo;
use capability::{
    cnode_bytes, mdb, CapError, CapId, Capability, MessageInfo, ObjectKind, ObjectRef, Rights,
    SlotRef, Untyped,
};
use elf::ElfError;
use hal::frame::PhysFrame;
use hal::paging::{AddressSpace, Flags, PAGE};
use hal::syscall::UserEntry;
use sched::ThreadId;

use crate::thread::{self, CONTEXTS};
use crate::{boot, console, cspace, KERNEL_SPACE, SCHED};

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
        // Validated: the segment lies inside the file and below ELF_LIMIT,
        // and the plan's arithmetic is host-tested in `elf` (#133).
        for load in seg.load_plan() {
            let frame = hal::frame::alloc_frame().ok_or(LoadError::OutOfMemory)?;
            if load.len > 0 {
                let off = load.file_offset as usize;
                let bytes = &image[off..off + load.len as usize];
                hal::object::write_phys(frame.addr() + load.frame_offset, hhdm, bytes);
            }
            if space.map(load.page, frame.addr(), flags).is_err() {
                hal::frame::free_frame(frame);
                return Err(LoadError::Map);
            }
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
/// [`reap_root`]; and the kernel's own CNode holding the originals of the
/// task's CNode and Untyped capabilities, out of the task's reach.
pub struct Bootstrap {
    cnode: PhysFrame,
    control: PhysFrame,
    untyped: PhysFrame,
    keeper: PhysFrame,
    /// The keeper CNode's registry index.
    kept: u16,
}

/// Slots in a task's keeper CNode: its CNode's original, then its Untyped's.
const KEEPER_SLOTS: usize = 2;

/// Create the root task as thread `t` (ADR 0019 decision 7): a 64-slot CNode
/// holding exactly its CNode (slot 1, `READ|WRITE`), 64 KiB of Untyped memory
/// (slot 2, `RETYPE`) and a send-only console capability with badge 1
/// (slot 3), minted from the console endpoint's original at `console`, with
/// slot 0 empty; then load `image` into it.
///
/// The task's CNode and Untyped capabilities are copies. The originals stay
/// in a kernel-held keeper CNode (ADR 0020), so everything the task makes or
/// hands out descends from them, even after the task deletes its own copies,
/// and revoking them at reap takes it all back.
///
/// # Errors
/// Memory ran out or the image was refused; nothing of the task remains.
pub fn bootstrap_root(t: ThreadId, console: SlotRef, image: &[u8]) -> Result<Bootstrap, LoadError> {
    let hhdm = crate::ipc::hhdm();
    assert!(cnode_bytes(ROOT_SLOTS) as u64 <= PAGE);
    let frames = [(); 3].map(|()| hal::frame::alloc_frame());
    let untyped = hal::frame::alloc_contiguous(ROOT_UNTYPED_FRAMES);
    let ([Some(cnode), Some(control), Some(keeper)], Some(untyped)) = (frames, untyped) else {
        frames
            .into_iter()
            .flatten()
            .for_each(hal::frame::free_frame);
        if let Some(u) = untyped {
            hal::frame::free_contiguous(u, ROOT_UNTYPED_FRAMES);
        }
        return Err(LoadError::OutOfMemory);
    };
    let region = ROOT_UNTYPED_FRAMES as u64 * PAGE;
    let ut = ObjectRef::new(ObjectKind::Untyped, control.addr());
    hal::object::store_untyped(ut, hhdm, Untyped::new(untyped.addr(), region));

    // `cnode` and `keeper` are fresh frames, each holding a CNode.
    let own = ObjectRef::new(ObjectKind::CNode, cnode.addr());
    let space = hal::object::cnode_cap_space(own, ROOT_SLOTS, hhdm);
    cspace::install(t, own, space);
    let keep = ObjectRef::new(ObjectKind::CNode, keeper.addr());
    let keep_space = hal::object::cnode_cap_space(keep, KEEPER_SLOTS, hhdm);
    let kept =
        cspace::with(|r| r.register(keep, keep_space)).expect("task: the CNode registry is full");
    let boot = Bootstrap {
        cnode,
        control,
        untyped,
        keeper,
        kept,
    };
    cspace::with(|r| {
        let (own_at, ut_at) = (SlotRef::new(kept, 0), SlotRef::new(kept, 1));
        mdb::insert_root(r, own_at, Capability::new(own, Rights::ALL))?;
        mdb::insert_root(r, ut_at, Capability::new(ut, Rights::ALL))?;
        mdb::copy(
            r,
            own_at,
            r.slot(t, CapId(1))?,
            Rights::READ | Rights::WRITE,
        )?;
        mdb::copy(r, ut_at, r.slot(t, CapId(2))?, Rights::RETYPE)?;
        let at = r.slot(t, CapId(3))?;
        mdb::mint(r, console, at, Rights::SEND, console::ROOT_BADGE)
    })
    .expect("task: root slots");
    if let Err(e) = spawn(t, ROOT_STACK_SLOT, image) {
        cspace::release(t);
        cspace::remove(kept);
        if let Err(held) = reclaim_memory(boot) {
            crate::warn!(
                "task: a refused root task's memory is still named, at {:?}; kept",
                held.holder
            );
        }
        return Err(e);
    }
    Ok(boot)
}

/// Memory behind an exited task's capabilities that something still names, so
/// it was kept, never to be freed: the capability that names it.
pub struct Unreclaimed {
    /// A capability into the task's memory.
    pub holder: SlotRef,
}

/// Reclaim the exited root task: its address space and kernel stack, its
/// capability space, and the memory behind its capabilities. Returns the
/// frames freed.
///
/// First the originals in the task's keeper CNode are revoked (ADR 0020):
/// everything derived or retyped from the task's CNode and Untyped goes,
/// wherever it went. A server the task granted an endpoint loses it, and a
/// thread blocked on that endpoint wakes with `Revoked`. Each round of a
/// revoke is bounded, and the reap yields between rounds. Then the task's
/// remaining capabilities and the originals are deleted.
///
/// # Errors
/// [`Unreclaimed`] if a capability still names the task's memory after all
/// that, which would be a kernel bug (#105): the memory is kept for ever
/// rather than let that capability reach whatever reuses the frames.
pub fn reap_root(t: ThreadId, boot: Bootstrap) -> Result<usize, Unreclaimed> {
    let freed = thread::reap(t);
    for i in 0..KEEPER_SLOTS {
        let at = SlotRef::new(boot.kept, i as u16);
        while !crate::ipc::with_caps(|ipc, r| cspace::revoke(ipc, r, at))
            .expect("task: an original")
        {
            crate::yield_now();
        }
    }
    cspace::release(t);
    cspace::remove(boot.kept);
    reclaim_memory(boot).map(|n| n + freed)
}

impl Bootstrap {
    /// The task's memory: its CNode, Untyped control block, keeper CNode and
    /// Untyped region.
    fn regions(&self) -> [(u64, u64); 4] {
        let page = |f: PhysFrame| (f.addr(), f.addr() + PAGE);
        let untyped = self.untyped.addr();
        [
            page(self.cnode),
            page(self.control),
            page(self.keeper),
            (untyped, untyped + ROOT_UNTYPED_FRAMES as u64 * PAGE),
        ]
    }
}

/// Free a task's CNode, Untyped control block, keeper CNode and Untyped
/// region, unless a capability in any capability space still names an object
/// inside them.
///
/// # Errors
/// [`Unreclaimed`], keeping the memory, if one does.
pub fn reclaim_memory(boot: Bootstrap) -> Result<usize, Unreclaimed> {
    for (start, end) in boot.regions() {
        if let Some(holder) = cspace::with(|r| r.find_reference(start, end)) {
            return Err(Unreclaimed { holder });
        }
    }
    hal::frame::free_frame(boot.cnode);
    hal::frame::free_frame(boot.control);
    hal::frame::free_frame(boot.keeper);
    hal::frame::free_contiguous(boot.untyped, ROOT_UNTYPED_FRAMES);
    Ok(3 + ROOT_UNTYPED_FRAMES)
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

/// Where the root task maps a frame in the address space it builds.
const ROOT_MAPPED: u64 = 0x4000_1000;
/// The slot the root task retypes an Endpoint into.
const ENDPOINT: CapId = CapId(7);
/// The holder: a kernel thread standing in for a server the root task
/// granted capabilities to (#105). Its slots: the granted endpoint and
/// Notification.
const HOLDER: ThreadId = ThreadId(20);
const HELD_EP: CapId = CapId(1);
const HELD_NOTE: CapId = CapId(2);
/// Set when the holder saw everything granted to it taken back.
static HOLDER_OK: AtomicBool = AtomicBool::new(false);

/// Give the holder a 4-slot CNode and a stack, for the life of the system;
/// it is admitted once something has been granted to it.
fn start_holder() {
    use alloc::boxed::Box;

    const SLOTS: usize = 4;
    const STACK_BYTES: usize = 16 * 1024;
    let hhdm = crate::ipc::hhdm();
    let frame = hal::frame::alloc_frame().expect("task: no frame for the holder");
    assert!(cnode_bytes(SLOTS) as u64 <= PAGE);
    // A fresh frame, holding the holder's CNode.
    let cnode = ObjectRef::new(ObjectKind::CNode, frame.addr());
    let space = hal::object::cnode_cap_space(cnode, SLOTS, hhdm);
    cspace::install(HOLDER, cnode, space);
    let stack = Box::leak(alloc::vec![0u8; STACK_BYTES].into_boxed_slice());
    CONTEXTS[HOLDER.0].init(stack.as_ptr() as u64 + STACK_BYTES as u64, holder);
}

/// Wait on the granted endpoint until it is destroyed, then check that both
/// granted capabilities are gone.
extern "C" fn holder() -> ! {
    let woke = crate::ipc::recv(HELD_EP, None);
    let plain = Message {
        info: MessageInfo::new(0, 0, 0),
        mr: [0; 4],
    };
    let send = crate::ipc::send(HELD_EP, plain, None);
    let note = cspace::with(|r| r.lookup(HOLDER, HELD_NOTE));
    crate::info!("task: holder woke with {woke:?}; then a send gave {send:?}");
    let ok = woke == Err(IpcError::Revoked)
        && send == Err(IpcError::InvalidCap)
        && note == Err(CapError::Empty);
    HOLDER_OK.store(ok, Ordering::Release);
    crate::exit_current()
}

/// The root task, talking to the console server over IPC.
fn root_demo(console: SlotRef, root: &[u8]) {
    start_holder();
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
    let signalled = cspace::with_space(ROOT, |cs, _| {
        cs.get(CapId(4)).is_ok_and(|c| {
            c.object.kind == ObjectKind::Notification
                && hal::object::load_notification(c.object, hhdm).signal == 0b101
        })
    });
    assert_eq!(
        signalled,
        Some(true),
        "task: slot 4 does not hold the Notification root signalled"
    );
    // The address space root built (ADR 0023): the frame copy in slot 16 is
    // mapped read-and-execute, the deleted frame and the unmapped one are
    // gone. Reaping root must then free its ASID.
    let (space, copy) = cspace::with(|r| (r.lookup(ROOT, CapId(11)), r.lookup(ROOT, CapId(16))));
    let (space, copy) = (
        space.expect("task: root's AddressSpace").object,
        copy.expect("task: root's frame copy").object,
    );
    let at = |v| hal::vspace::translate(space, v, hhdm);
    let mapped = at(ROOT_MAPPED).expect("task: root's frame copy is mapped");
    assert_eq!(mapped.0, copy.phys, "task: the wrong frame is mapped");
    assert!(
        !mapped.1.contains(Flags::WRITABLE) && !mapped.1.contains(Flags::NO_EXECUTE),
        "task: the mapping is not read and execute"
    );
    assert!(
        at(ROOT_MAPPED - PAGE).is_none(),
        "task: a deleted frame capability stayed mapped"
    );
    assert!(
        at(ROOT_MAPPED + PAGE).is_none(),
        "task: an unmapped frame stayed mapped"
    );
    assert_eq!(
        crate::vspace::live(),
        1,
        "task: root's address space has no ASID"
    );
    // The Thread object root made (slot 20) is a thread slot whose kernel
    // stack is the object's own memory.
    let made = cspace::with(|r| r.lookup(ROOT, CapId(20)))
        .expect("task: root's Thread")
        .object;
    let t = thread::of_object(made).expect("task: root's Thread has no slot");
    let top = thread::kernel_stack_top(t);
    let base = hal::cpu::without_interrupts(|| {
        KERNEL_SPACE
            .lock()
            .as_ref()
            .and_then(|s| s.translate(top - PAGE))
    });
    assert_eq!(
        base,
        Some(made.phys + 3 * PAGE),
        "task: a Thread's stack is not its memory"
    );
    assert_eq!(
        thread::object_threads(),
        1,
        "task: one Thread object is bound"
    );

    // The #105 scenario: the root task made an Endpoint (slot 7), and it
    // and its Notification are granted to a server, each a child of the
    // root's capability as an IPC transfer makes it. The server then waits
    // on the endpoint. Reaping the task must take both back, wake the
    // server with `Revoked`, and still free all of the task's memory.
    let ep = cspace::with(|r| {
        let (ep, note) = (r.slot(ROOT, ENDPOINT)?, r.slot(ROOT, CapId(4))?);
        let (to_ep, to_note) = (r.slot(HOLDER, HELD_EP)?, r.slot(HOLDER, HELD_NOTE)?);
        mdb::copy(r, ep, to_ep, Rights::SEND | Rights::RECV)?;
        mdb::copy(r, note, to_note, Rights::SEND)?;
        r.cap(ep).map(|c| c.object)
    })
    .expect("task: grant the root task's objects to the holder");
    assert_eq!(
        ep.kind,
        ObjectKind::Endpoint,
        "task: slot 7 is not the root task's Endpoint"
    );
    hal::cpu::without_interrupts(|| SCHED.lock().admit(HOLDER));
    while crate::ipc::waiting_on(ep) != Some(HOLDER) {
        crate::yield_now();
    }
    if let Err(kept) = reap_root(ROOT, boot) {
        panic!(
            "task: a capability still names the reaped task's memory, at {:?}",
            kept.holder
        );
    }
    assert_eq!(
        crate::vspace::live(),
        0,
        "task: the reap left an ASID in use"
    );
    assert_eq!(
        thread::object_threads(),
        0,
        "task: the reap left a Thread bound"
    );
    thread::wait_exit(HOLDER, "the holder");
    thread::reap(HOLDER);
    assert!(
        HOLDER_OK.load(Ordering::Acquire),
        "task: the holder kept what the reaped task granted it"
    );
    crate::info!(
        "task: reaping the root task revoked what it granted; the waiting holder woke with Revoked"
    );
    assert_eq!(free_frames(), before, "task: the root task leaked frames");
    crate::info!(
        "task: root task confined to 4 capabilities; console via IPC; all memory reclaimed"
    );
}
