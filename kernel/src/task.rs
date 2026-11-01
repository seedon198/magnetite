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
use elf::ElfError;
use hal::paging::{AddressSpace, Flags, PAGE};
use hal::syscall::UserEntry;
use sched::ThreadId;

use crate::thread::{self, CONTEXTS};
use crate::{boot, KERNEL_SPACE, SCHED};

/// The top of every task's stack.
pub const STACK_TOP: u64 = 0x7fff_ffff_0000;
/// Pages of stack below [`STACK_TOP`]; the page below them stays unmapped.
pub const STACK_PAGES: u64 = 4;
/// Segments must end at or below this: under the stack and its guard page.
const ELF_LIMIT: u64 = STACK_TOP - (STACK_PAGES + 1) * PAGE;

/// The fault task's thread and kernel-stack slot.
const FAULT: ThreadId = ThreadId(13);
const FAULT_STACK_SLOT: usize = 6;

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
    let Some(kstack) = thread::alloc_kernel_stack(stack_slot) else {
        thread::free_kernel_stack(stack_slot);
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

/// Frames currently free.
fn free_frames() -> usize {
    hal::frame::stats().map_or(0, |s| s.free)
}

/// Load the fault task, and a corrupted copy of it: the copy is refused before
/// anything is mapped, the task is killed by its page fault, and every frame
/// it used comes back.
pub fn demo(boot_info: &BootInfo) {
    let fault = boot::module_bytes(boot_info.module("fault").expect("task: no fault module"));
    let before = free_frames();

    let mut corrupted: Vec<u8> = fault.to_vec();
    corrupted[0] ^= 0xff;
    let refused = spawn(FAULT, FAULT_STACK_SLOT, &corrupted);
    assert_eq!(refused, Err(LoadError::Elf(ElfError::BadMagic)));
    drop(corrupted);
    crate::info!("task: refused a corrupted image (BadMagic)");

    spawn(FAULT, FAULT_STACK_SLOT, fault).expect("task: loading the fault task");
    while hal::cpu::without_interrupts(|| SCHED.lock().contains(FAULT)) {
        crate::yield_now();
    }
    thread::reap(FAULT);
    assert_eq!(free_frames(), before, "task: the fault task leaked frames");
    crate::info!("task: fault task contained and reclaimed");
}
