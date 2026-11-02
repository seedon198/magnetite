//! Global Descriptor Table and Task State Segment (x86-64).
//!
//! In long mode segmentation is mostly vestigial, but we still need a GDT with
//! valid 64-bit code/data descriptors, user-mode descriptors (ordered for a
//! future `syscall`/`sysret`, Intel SDM Vol. 3A §5.8.8), and a TSS. The TSS
//! holds the stacks the CPU switches to on privilege changes (`rsp0`) and on
//! specific vectors via the Interrupt Stack Table (IST) - we dedicate IST
//! entries to the double fault, NMI, and machine check so those always run on a
//! known-good stack even if the kernel stack is corrupt.
//!
//! Hand-rolled per ADR 0009. References: Intel SDM Vol. 3A ch. 3 (segmentation)
//! and §8.7 (64-bit TSS).

use core::arch::asm;

use crate::cell::Racy;

/// Kernel code selector (GDT index 1).
pub const KERNEL_CODE: u16 = 0x08;
/// Kernel data selector (GDT index 2).
pub const KERNEL_DATA: u16 = 0x10;
/// User data selector (index 4, RPL 3).
pub const USER_DATA: u16 = 0x20 | 3;
/// User 64-bit code selector (index 5, RPL 3).
pub const USER_CODE: u16 = 0x28 | 3;
/// TSS selector (index 6; the descriptor occupies indices 6 and 7).
pub const TSS_SELECTOR: u16 = 0x30;

/// Size of each dedicated stack (IST entries and `rsp0`).
const STACK_SIZE: usize = 16 * 1024;

/// 64-bit Task State Segment (Intel SDM Vol. 3A §8.7).
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    privilege_stacks: [u64; 3], // rsp0..rsp2
    reserved1: u64,
    interrupt_stacks: [u64; 7], // ist1..ist7
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

const _: () = assert!(core::mem::size_of::<Tss>() == 104);

impl Tss {
    const fn empty() -> Self {
        Self {
            reserved0: 0,
            privilege_stacks: [0; 3],
            reserved1: 0,
            interrupt_stacks: [0; 7],
            reserved2: 0,
            reserved3: 0,
            iomap_base: 0,
        }
    }
}

/// The GDT: 6 segment descriptors plus a 16-byte (two-slot) TSS descriptor.
#[repr(C, align(16))]
struct Gdt {
    entries: [u64; 8],
}

/// Build an 8-byte code/data descriptor with base 0 and a 4 GiB granular limit.
/// `access` is the access byte; `flags` is the 4-bit flags nibble (G, DB, L, AVL).
const fn segment(access: u8, flags: u8) -> u64 {
    let limit: u64 = 0xF_FFFF;
    (limit & 0xFFFF)
        | ((access as u64) << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((flags as u64) & 0xF) << 52)
}

// Access bytes: P|S set; code adds E; user sets DPL=3.
// Flags nibble: G=0x8, DB=0x4, L=0x2.
//
// Slot 0x18 is only the base `STAR` hands `sysretq`, which loads fixed CS/SS
// values (base + 16, base + 8) without reading the descriptor. It must stay
// null: a present 32-bit DPL 3 code segment there would let ring 3 far-return
// into compatibility mode and `syscall` to `IA32_CSTAR` on a stack it chose
// (#102). No ring-3 code descriptor here is anything but 64-bit.
static GDT: Racy<Gdt> = Racy::new(Gdt {
    entries: [
        0,                  // 0x00 null
        segment(0x9A, 0xA), // 0x08 kernel code (64-bit: L)
        segment(0x92, 0xC), // 0x10 kernel data
        0,                  // 0x18 null: STAR's sysret base, never loaded (#102)
        segment(0xF2, 0xC), // 0x20 user data
        segment(0xFA, 0xA), // 0x28 user code (64-bit: L)
        0,                  // 0x30 TSS low  (filled in init)
        0,                  // 0x38 TSS high (filled in init)
    ],
});

static TSS: Racy<Tss> = Racy::new(Tss::empty());

/// One 4 KiB page, the unit of a guard page.
const PAGE: usize = 4096;

/// Backing memory for the dedicated stacks. Page-aligned, and each stack is
/// preceded by a one-page **guard** that [`guard_pages`] hands to the memory
/// manager to unmap (#41): because stacks grow downward, an overflow runs off
/// the bottom into the (unmapped) guard and faults instead of silently
/// corrupting whatever lies below.
#[repr(C, align(4096))]
struct Stacks {
    guard_df: [u8; PAGE],
    double_fault: [u8; STACK_SIZE],
    guard_nmi: [u8; PAGE],
    nmi: [u8; STACK_SIZE],
    guard_mc: [u8; PAGE],
    machine_check: [u8; STACK_SIZE],
    guard_k: [u8; PAGE],
    kernel: [u8; STACK_SIZE],
}
static STACKS: Racy<Stacks> = Racy::new(Stacks {
    guard_df: [0; PAGE],
    double_fault: [0; STACK_SIZE],
    guard_nmi: [0; PAGE],
    nmi: [0; STACK_SIZE],
    guard_mc: [0; PAGE],
    machine_check: [0; STACK_SIZE],
    guard_k: [0; PAGE],
    kernel: [0; STACK_SIZE],
});

/// Top (highest address) of a stack buffer; stacks grow downward.
fn stack_top(buf: &[u8; STACK_SIZE]) -> u64 {
    buf.as_ptr() as u64 + STACK_SIZE as u64
}

/// Virtual addresses of the four stack guard pages (page-aligned). The memory
/// manager unmaps these so a stack overflow faults on the guard (#41).
#[must_use]
pub fn guard_pages() -> [u64; 4] {
    use core::ptr::addr_of;
    let s = STACKS.get();
    // SAFETY: `STACKS` is a live static; we only read the addresses of its
    // (page-aligned) guard fields, never their contents.
    unsafe {
        [
            addr_of!((*s).guard_df) as u64,
            addr_of!((*s).guard_nmi) as u64,
            addr_of!((*s).guard_mc) as u64,
            addr_of!((*s).guard_k) as u64,
        ]
    }
}

/// Encode a 16-byte TSS system descriptor (type = available 64-bit TSS).
fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = ((limit as u64) & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (0x89_u64 << 40) // present, type 0x9 (available 64-bit TSS)
        | ((((limit as u64) >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = (base >> 32) & 0xFFFF_FFFF;
    (low, high)
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Point `TSS.rsp0`, the stack the CPU switches to on an interrupt or
/// exception taken in ring 3, at `top`. The kernel calls this on every context
/// switch with the next thread's kernel stack (ADR 0019). Call with interrupts
/// off, so no ring-3 entry can observe a half-updated value.
pub fn set_rsp0(top: u64) {
    // SAFETY: the TSS is initialized by `init` and only ever written here and
    // there, on this single CPU with interrupts off (see `Racy`). The packed
    // field is written unaligned through a raw pointer.
    unsafe {
        let tss = TSS.get();
        core::ptr::addr_of_mut!((*tss).privilege_stacks[0]).write_unaligned(top);
    }
}

/// Install our GDT and TSS, reload the segment registers, and load the task
/// register. Call once, early in boot, before setting up the IDT.
pub fn init() {
    // SAFETY: single-threaded boot-time initialization (see `Racy`). We write
    // the TSS and GDT TSS-descriptor once, then load them into the CPU.
    unsafe {
        let tss = TSS.get();
        let stacks = STACKS.get();

        // Point the IST entries and rsp0 at the tops of their stacks. Packed
        // fields are written unaligned via raw pointers.
        let df = stack_top(&(*stacks).double_fault);
        let nmi = stack_top(&(*stacks).nmi);
        let mc = stack_top(&(*stacks).machine_check);
        let k0 = stack_top(&(*stacks).kernel);
        core::ptr::addr_of_mut!((*tss).interrupt_stacks[0]).write_unaligned(df);
        core::ptr::addr_of_mut!((*tss).interrupt_stacks[1]).write_unaligned(nmi);
        core::ptr::addr_of_mut!((*tss).interrupt_stacks[2]).write_unaligned(mc);
        core::ptr::addr_of_mut!((*tss).privilege_stacks[0]).write_unaligned(k0);
        // No I/O permission bitmap: point past the TSS limit.
        core::ptr::addr_of_mut!((*tss).iomap_base)
            .write_unaligned(core::mem::size_of::<Tss>() as u16);

        // Fill the TSS descriptor in the GDT.
        let gdt = GDT.get();
        let (low, high) = tss_descriptor(tss as u64, core::mem::size_of::<Tss>() as u32 - 1);
        core::ptr::addr_of_mut!((*gdt).entries[6]).write(low);
        core::ptr::addr_of_mut!((*gdt).entries[7]).write(high);

        let pointer = DescriptorTablePointer {
            limit: (core::mem::size_of::<[u64; 8]>() - 1) as u16,
            base: core::ptr::addr_of!((*gdt).entries) as u64,
        };

        load_gdt(&pointer);
        reload_segments(KERNEL_CODE, KERNEL_DATA);
        load_tr(TSS_SELECTOR);
    }
}

/// # Safety
/// `pointer` must describe a valid GDT that remains live for as long as it is
/// loaded.
unsafe fn load_gdt(pointer: &DescriptorTablePointer) {
    // SAFETY: `lgdt` loads the GDTR from the given pointer; the caller
    // guarantees it refers to a valid, live GDT.
    unsafe {
        asm!("lgdt [{}]", in(reg) pointer, options(readonly, nostack, preserves_flags));
    }
}

/// # Safety
/// `code_sel`/`data_sel` must be valid selectors in the currently loaded GDT.
unsafe fn reload_segments(code_sel: u16, data_sel: u16) {
    // SAFETY: reloads CS via a far return to the next instruction, then loads
    // the data segment registers. Selectors are valid per the caller's
    // contract; the far return target is the local label, so control flow is
    // unchanged apart from CS.
    unsafe {
        asm!(
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "mov fs, {data:x}",
            "mov gs, {data:x}",
            code = in(reg) code_sel as u64,
            tmp = lateout(reg) _,
            data = in(reg) data_sel,
            options(preserves_flags),
        );
    }
}

/// # Safety
/// `selector` must reference a valid, present TSS descriptor in the GDT.
unsafe fn load_tr(selector: u16) {
    // SAFETY: `ltr` loads the task register from a valid TSS selector per the
    // caller's contract.
    unsafe {
        asm!("ltr {0:x}", in(reg) selector, options(nostack, preserves_flags));
    }
}
