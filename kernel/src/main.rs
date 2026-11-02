#![no_std]
#![no_main]
#![deny(unsafe_code)]
#![deny(missing_docs)]

//! The microkernel binary.
//!
//! This crate is `#![deny(unsafe_code)]`. All `unsafe` is confined to two
//! places, both CI-enforced (see `docs/adr/0007-unsafe-policy.md`): the `hal`
//! crate (hardware access) and the [`boot`] module (bootloader/linker glue).
//! Kernel logic - [`kmain`] and below - contains no `unsafe`.

extern crate alloc;

mod banner;
mod boot;
mod console;
mod invoke;
mod ipc;
mod log;
mod syscall;
mod task;
mod thread;
mod user;

use alloc::boxed::Box;
use alloc::vec::Vec;
use boot_info::BootInfo;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use hal::paging::{AddressSpace, Flags, MapError};
use hal::serial::SerialPort;
use hal::sync::SpinLock;
use thread::CONTEXTS;

/// The kernel's address space, established in [`init_paging`]. Held for later
/// milestones (the heap and guard pages in #41, user address spaces in M4).
static KERNEL_SPACE: SpinLock<Option<AddressSpace>> = SpinLock::new(None);

/// The kernel heap (#41). Backed by frames mapped on demand by [`heap_grow`].
///
/// A failed infallible allocation (`Box::new` and the like) reaches our panic
/// handler with "memory allocation of N bytes failed" and a backtrace: on
/// stable `no_std`, the default allocation-error handler panics, so kernel
/// OOM is already legible and no handler of our own is needed (#68).
#[global_allocator]
static HEAP: hal::heap::KernelHeap = hal::heap::KernelHeap::new();

/// Round-robin scheduler (#78). Thread 0 is the boot thread; 1 and 2 are the
/// cooperative demo workers; 3 and 4 are the preemption demo spinners (#80);
/// 5 and 6 are the IPC demo's server and client (#81).
static SCHED: SpinLock<sched::Scheduler> = SpinLock::new(sched::Scheduler::new());
/// Saved contexts, indexed by [`sched::ThreadId`]: boot, workers A/B, spinners
/// C/D, IPC server and client.
/// Times each demo worker has been scheduled (proof the switch happened).
static A_TICKS: AtomicU64 = AtomicU64::new(0);
static B_TICKS: AtomicU64 = AtomicU64::new(0);

/// LAPIC timer vector (#80): the first vector above the remapped (and masked)
/// legacy PIC range 0x20-0x2F.
const TIMER_VECTOR: u8 = 0x30;
/// Timer interrupt rate: 100 Hz, i.e. a 10 ms round-robin time slice.
const TIMER_HZ: u32 = 100;
/// Kernel virtual address of the LAPIC register page: the base of PML4 slot
/// 508, which neither the bootloader (HHDM, kernel image) nor the heap uses.
const LAPIC_MMIO_VA: u64 = 0xffff_fe00_0000_0000;
/// Timer interrupts taken since the LAPIC timer started.
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Context switches forced by the timer (as opposed to a cooperative yield).
static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

/// Preemption demo (#80): work done by spinners C and D, which never yield.
static SPIN_WORK: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
/// Spinners that have finished their rounds and exited.
static SPINNERS_DONE: AtomicU64 = AtomicU64::new(0);
/// Rounds each spinner runs before exiting.
const SPIN_ROUNDS: u64 = 4;
/// Timer ticks of wall time each spinner round lasts.
const SPIN_ROUND_TICKS: u64 = 5;
/// Kernel stack size for each spinner thread.
const SPIN_STACK_BYTES: usize = 16 * 1024;

/// Virtual base of the kernel heap window. Deliberately placed *below* the
/// KASLR window (the top 2 GiB, from [`HEAP_LIMIT`]): Limine always loads the
/// kernel image at or above `0xffffffff80000000`, so a heap region that lives
/// below that and is capped there can never collide with the slid kernel image,
/// however KASLR places it (#61). This base is in the lower part of the kernel's
/// PML4 slot, which the bootloader leaves unused.
const HEAP_BASE: u64 = 0xffff_ff80_0000_0000;
/// Upper bound of the heap window: the start of the kernel/KASLR top-2-GiB
/// region. The heap never grows into or past this, keeping it clear of the
/// kernel image. (Leaves ~510 GiB of heap space - ample.)
const HEAP_LIMIT: u64 = 0xffff_ffff_8000_0000;
/// Minimum bytes mapped per growth (64 frames).
const HEAP_CHUNK: u64 = 256 * 1024;
/// Next unmapped heap virtual address; grows contiguously upward.
static HEAP_NEXT: AtomicU64 = AtomicU64::new(HEAP_BASE);

/// Architecture-neutral kernel entry, called by the boot code once the
/// bootloader handoff is validated and a [`BootInfo`] has been assembled.
fn kmain(boot_info: &BootInfo) -> ! {
    log::init();
    log::set_max_level(log::Level::Debug); // verbose during early bring-up

    // Bring up the on-screen console as a second log sink, if we have a
    // framebuffer. Serial remains the primary sink.
    if let Some(fb) = boot_info.framebuffer {
        hal::framebuffer::init(hal::framebuffer::FramebufferInfo {
            address: fb.address,
            width: fb.width as usize,
            height: fb.height as usize,
            pitch: fb.pitch as usize,
            bpp: fb.bpp,
            red_shift: fb.red_shift,
            green_shift: fb.green_shift,
            blue_shift: fb.blue_shift,
        });
    }

    // The banner and system summary come first; everything they show is known
    // on entry (boot handoff, CPUID) or stamped in at build time.
    let cpu = hal::cpuid::detect();
    banner::print(boot_info, &cpu);

    info!("hello from the kernel");
    // A bootloader map or module list larger than the handoff's tables is
    // truncated; say so instead of losing memory or a program silently (#124).
    if boot_info.dropped_regions() > 0 {
        warn!(
            "boot: {} memory regions did not fit the handoff table and are unused",
            boot_info.dropped_regions()
        );
    }
    if boot_info.dropped_modules() > 0 {
        warn!(
            "boot: {} boot modules did not fit the handoff table and were not loaded",
            boot_info.dropped_modules()
        );
    }
    debug!("boot info received at {:p}", boot_info);

    hal::gdt::init();
    info!("gdt: loaded GDT, TSS, and segment registers");

    hal::idt::init();
    info!("idt: installed interrupt descriptor table");

    // Required CPU features (identity and the full flag list are in the banner).
    let f = &cpu.features;
    assert!(f.nx, "CPU lacks NX (No-Execute); required for W^X");
    assert!(f.sse2, "CPU lacks SSE2; required baseline for x86-64");

    // Supervisor-access protections (M2, #41): block the kernel from executing
    // or (SMAP) reading user-accessible pages. Safe now - no user pages exist.
    let (smep, smap) = hal::cpu::enable_smep_smap(f.smep, f.smap);
    info!("cpu protections: smep={smep} smap={smap}");

    // Memory, layout and the KASLR slide (#47) are reported in the banner.

    // Physical frame allocator (M2, #38). Translate the memory map into the
    // allocator's region view (only RegionKind::Usable is handed out), then
    // bring it up and exercise it.
    init_frames(boot_info);

    // Virtual memory (M2, #39): move onto page tables we own, then self-test
    // the map/translate/unmap path.
    init_paging(boot_info);
    ipc::init(boot_info.hhdm_offset);
    check_modules(boot_info);

    // W^X + NX hardening (M2, #40): enforce per-section kernel permissions and
    // make the direct physical map non-executable.
    harden_memory(boot_info);

    // Guard pages (M2, #41): unmap the page below each HAL-owned stack so an
    // overflow faults instead of corrupting its neighbor.
    install_guard_pages();

    // Kernel heap (M2, #41): a growable allocator, so Box/Vec work.
    init_heap();

    // Page-fault diagnostics (M2, #42): register named regions and prove the
    // recoverable-probe + classification path.
    init_fault_diagnostics(boot_info);

    // Leave the bootloader's stack, which has no guard page, for a kernel stack
    // that does: an overflow during the rest of boot faults and is named
    // (#66). The boot thread then idles on this stack for good.
    SMEP_SMAP.store(u8::from(smep) | u8::from(smap) << 1, Ordering::Relaxed);
    let top = thread::alloc_kernel_stack(thread::BOOT_STACK_SLOT)
        .expect("boot: no frames for the boot thread's kernel stack");
    hal::context::run_on_stack(top, kmain_guarded, 0)
}

/// Whether SMEP and SMAP were enabled (bits 0 and 1), for the `ready:` line.
static SMEP_SMAP: AtomicU8 = AtomicU8::new(0);

/// The rest of boot, on the boot thread's guarded kernel stack.
extern "C" fn kmain_guarded(_: u64) -> ! {
    let boot_info = boot::info();
    let flags = SMEP_SMAP.load(Ordering::Relaxed);
    let (smep, smap) = (flags & 1 != 0, flags & 2 != 0);

    // Per-thread kernel stacks (#82): each sits above an unmapped guard page.
    assert!(
        thread::selftest(),
        "thread: kernel stack guard page check failed"
    );
    info!("thread: kernel stack guard page verified");

    // The boot thread runs on a kernel stack with a guard page below it, so
    // an overflow faults instead of corrupting memory (#66).
    assert_eq!(
        hal::fault::classify(hal::cpu::stack_pointer()),
        Some("kernel stack"),
        "boot: kmain is not on a guarded kernel stack"
    );

    // Untyped memory & retype (M3, #55): demonstrate carving object storage
    // from an Untyped region with watermark accounting.
    demo_untyped(boot_info);

    // Kernel objects & CNode-backed capability space (M3, #56): retype objects
    // and install/attenuate real capabilities.
    demo_capabilities(boot_info);

    // Capability invocation (M3, #57): dispatch rights-checked methods on
    // capabilities - the surface M4's syscall entry will call.
    demo_invoke(boot_info);

    // Threads & cooperative round-robin scheduling (M4, #78): two kernel
    // threads switching via a hal context switch and the pure scheduler.
    demo_threads(boot_info);

    // Preemptive scheduling (M4, #80): the LAPIC timer drives the round-robin
    // scheduler, so threads are time-sliced without yielding.
    let preemptive = init_preemption();
    if preemptive {
        demo_preemption();
    }

    // Synchronous IPC (M4, #81): a client and a server kernel thread, each with
    // its own capability space, call and reply over an Endpoint.
    ipc::demo(boot_info);

    // User threads (M4, #82): each in its own address space with its own
    // kernel stack, preempted in ring 3; a fault in ring 3 ends only the task.
    // The system-call boundary (#82, ADR 0019): every ring-3 entry to the
    // kernel goes through `syscall::handle` or the user-fault hook.
    hal::syscall::init();
    hal::syscall::register(syscall::handle);
    hal::syscall::register_kill(user::syscall_killed);
    hal::idt::register_user_fault(user::user_fault);
    user::demo(boot_info, preemptive);

    // The first user programs (#82, ADR 0019): loaded from boot modules.
    task::demo(boot_info);

    // Exercise the trap path end to end: a breakpoint (#BP) is a recoverable
    // trap, so the handler prints a report and execution resumes here.
    debug!("triggering a test breakpoint (#BP)...");
    hal::idt::breakpoint();
    info!("resumed after breakpoint");

    // One line with every protection now in force, each verified above.
    let slide = boot_info
        .kernel_virt_base
        .wrapping_sub(banner::KERNEL_LINK_BASE);
    info!(
        "ready: W^X+NX verified, smep={smep} smap={smap}, kaslr slide={slide:#x}, guard pages, {}",
        if preemptive {
            "preemptive scheduling"
        } else {
            "cooperative scheduling"
        }
    );
    info!("kernel: idling (timer running; user demo tasks exited)");

    hal::cpu::halt_loop();
}

/// Bring up the physical frame allocator from the boot memory map and exercise
/// it with a small alloc/free smoke test.
/// The user programs (#82) must have been loaded, and their memory must never
/// be handed out: it lies outside every usable region (Limine reports it as
/// `KernelAndModules`).
fn check_modules(boot_info: &BootInfo) {
    for name in ["root", "fault"] {
        // A missing or malformed program is not fatal (ADR 0019 decision 1):
        // the loader refuses it later and the kernel boots on without it.
        let Some(m) = boot_info.module(name) else {
            warn!("boot: module {name} missing");
            continue;
        };
        let phys = m.address - boot_info.hhdm_offset;
        let end = phys + m.size;
        let overlaps_usable = boot_info.regions().iter().any(|r| {
            r.kind == boot_info::RegionKind::Usable && phys < r.base + r.length && r.base < end
        });
        assert!(
            !overlaps_usable,
            "boot: module {name} lies in usable memory"
        );
        if !boot::module_bytes(m).starts_with(b"\x7fELF") {
            warn!("boot: module {name} is not an ELF file");
        }
    }
    let size = |name| boot_info.module(name).map_or(0, |m| m.size);
    info!(
        "boot: modules root ({} bytes), fault ({} bytes)",
        size("root"),
        size("fault")
    );
}

fn init_frames(boot_info: &BootInfo) {
    use boot_info::RegionKind;
    use hal::frame::Region;

    let src = boot_info.regions();
    let mut regions = [Region {
        base: 0,
        length: 0,
        usable: false,
    }; boot_info::MAX_MEMORY_REGIONS];
    for (dst, r) in regions.iter_mut().zip(src) {
        *dst = Region {
            base: r.base,
            length: r.length,
            usable: r.kind == RegionKind::Usable,
        };
    }

    if !hal::frame::init(&regions[..src.len()], boot_info.hhdm_offset) {
        warn!("frames: allocator init failed (no usable RAM?)");
        return;
    }

    if let Some(s) = hal::frame::stats() {
        info!(
            "frames: {} MiB usable, {} free / {} total frames tracked",
            s.usable * hal::frame::FRAME_SIZE as usize / (1024 * 1024),
            s.free,
            s.total,
        );
    }

    // Smoke test: allocate three frames, confirm they are distinct and zeroed
    // by construction, free them, and confirm the pool returns to its size.
    let before = hal::frame::stats().map_or(0, |s| s.free);
    let a = hal::frame::alloc_frame();
    let b = hal::frame::alloc_frame();
    let c = hal::frame::alloc_frame();
    if let (Some(a), Some(b), Some(c)) = (a, b, c) {
        debug!(
            "frames: smoke test got {:#x} {:#x} {:#x}",
            a.addr(),
            b.addr(),
            c.addr()
        );
        hal::frame::free_frame(a);
        hal::frame::free_frame(b);
        hal::frame::free_frame(c);
        let after = hal::frame::stats().map_or(0, |s| s.free);
        assert_eq!(before, after, "frame smoke test leaked frames");
        info!("frames: alloc/free smoke test passed");
    } else {
        warn!("frames: smoke test could not allocate");
    }
}

/// Build the kernel address space (inheriting the bootloader's higher-half
/// mappings), switch to it, and self-test the mapping path.
fn init_paging(boot_info: &BootInfo) {
    let Some(aspace) = AddressSpace::new_kernel(boot_info.hhdm_offset) else {
        warn!("paging: could not allocate a PML4 (out of frames?)");
        return;
    };
    aspace.switch();
    thread::init_kernel_root(aspace.root_phys());
    info!("paging: running on our own PML4 (higher half inherited)");
    *KERNEL_SPACE.lock() = Some(aspace);

    // The self-test maps and unmaps one page in fresh tables; unmap frees
    // the tables it empties, so every frame comes back (#64).
    let before = hal::frame::stats().map_or(0, |s| s.free);
    let ok = KERNEL_SPACE
        .lock()
        .as_mut()
        .is_some_and(AddressSpace::selftest);
    let after = hal::frame::stats().map_or(0, |s| s.free);
    assert_eq!(after, before, "paging: map then unmap leaked page tables");
    if ok {
        info!("paging: map/translate/unmap self-test passed");
    } else {
        warn!("paging: self-test FAILED");
    }

    // User address spaces (#82, ADR 0019) copy the kernel half's top-level
    // entries, so every one must exist before the first user space: anything
    // the kernel maps later then lands in tables all spaces share.
    let mut guard = KERNEL_SPACE.lock();
    let Some(kernel) = guard.as_mut() else { return };
    assert!(
        kernel.populate_higher_half(),
        "paging: out of frames populating the kernel half"
    );
    let probe = core::ptr::addr_of!(HEAP) as u64;
    // Both self-test spaces are destroyed, and every frame they used comes
    // back (#123).
    let free_before = hal::frame::stats().map_or(0, |s| s.free);
    let isolated = AddressSpace::new_user(kernel).is_some_and(|mut user| {
        let Some(frame) = hal::frame::alloc_frame() else {
            return false;
        };
        const VA: u64 = 0x4000_0000;
        let ok = user
            .map(VA, frame.addr(), Flags::USER | Flags::NO_EXECUTE)
            .is_ok()
            && user.translate(VA) == Some(frame.addr())
            && kernel.translate(VA).is_none()
            && user.translate(probe).is_some()
            && user.translate(probe) == kernel.translate(probe);
        let supervisor_only = user.kernel_half_supervisor_only();
        user.unmap(VA);
        hal::frame::free_frame(frame);
        user.destroy_user();
        ok && supervisor_only
    });
    assert!(isolated, "paging: user space isolation self-test failed");
    // The top user page is never mappable: a `syscall` there would return to a
    // non-canonical address, and `sysretq` faults in ring 0 on Intel with the
    // user's stack pointer already loaded.
    let top_refused = AddressSpace::new_user(kernel).is_some_and(|mut user| {
        let Some(frame) = hal::frame::alloc_frame() else {
            return false;
        };
        let refused = user
            .map(hal::paging::USER_TOP, frame.addr(), Flags::USER)
            .is_err();
        hal::frame::free_frame(frame);
        user.destroy_user();
        refused
    });
    assert!(
        top_refused,
        "paging: the last user page must not be mappable"
    );
    let free_after = hal::frame::stats().map_or(0, |s| s.free);
    assert_eq!(
        free_after, free_before,
        "paging: the user-space self-tests leaked frames"
    );
    // A mapping under one of the HHDM's large pages is refused, never written
    // into the memory the large page maps (#120). Phys 2 MiB + 4 KiB is inside
    // whatever covers it; a 4 KiB mapping there already exists instead.
    let under = boot_info.hhdm_offset + hal::paging::HUGE_2M + hal::paging::PAGE;
    let refused = hal::frame::alloc_frame().is_some_and(|frame| {
        let r = kernel.map(under, frame.addr(), Flags::WRITABLE);
        hal::frame::free_frame(frame);
        matches!(r, Err(MapError::HugePage | MapError::AlreadyMapped))
    });
    assert!(refused, "paging: mapped through an HHDM large page");
    info!(
        "paging: user space isolated (lower half private, kernel half shared and supervisor-only)"
    );
}

/// Enforce W^X + NX: enable NX, set per-section kernel-image permissions
/// (`.text` R-X, `.rodata` R+NX, `.data`/`.bss` RW+NX), and make the HHDM
/// non-executable. Then verify the key invariants.
fn harden_memory(boot_info: &BootInfo) {
    use hal::paging::{Flags, ProtectError};

    hal::paging::enable_nx();

    let s = boot::kernel_sections();
    let mut guard = KERNEL_SPACE.lock();
    let Some(aspace) = guard.as_mut() else {
        warn!("harden: no kernel address space; skipping");
        return;
    };

    // (range, flags) per section. `protect` adds PRESENT; .text gets neither
    // WRITABLE nor NX (R-X), the rest get NX, and .data also WRITABLE.
    // RELRO comes after .data, which contains it: the later entry wins (#126).
    let sections = [
        (s.text, Flags::empty()),
        (s.rodata, Flags::NO_EXECUTE),
        (s.data, Flags::WRITABLE | Flags::NO_EXECUTE),
        (s.relro, Flags::NO_EXECUTE),
    ];
    let mut pages = 0usize;
    let mut huge = 0usize;
    for ((start, end), flags) in sections {
        let mut v = start;
        while v < end {
            match aspace.protect(v, flags) {
                Ok(()) => pages += 1,
                Err(ProtectError::HugePage) => huge += 1,
                Err(ProtectError::NotMapped) => {}
                Err(ProtectError::BadFlags) => unreachable!("harden: section flags never set HUGE"),
            }
            v += hal::paging::PAGE;
        }
    }

    // Make the direct physical map (HHDM) non-executable, then flush the TLB.
    let hhdm = boot_info.hhdm_offset;
    aspace.make_range_nx_toplevel(hhdm, hhdm + boot_info.highest_address());
    aspace.reload();

    info!(
        "harden: NX on; protected {} kernel pages ({} huge skipped)",
        pages, huge
    );

    // The HHDM maps the kernel image's frames a second time, read-write. Make
    // that alias read-only for .text and .rodata (splitting the huge pages
    // that cover them), so a kernel write primitive cannot patch code or
    // read-only tables through it and run the result at the normal address
    // (#119). .data is writable through both mappings either way.
    let mut aliases = 0usize;
    for (start, end) in [s.text, s.rodata, s.relro] {
        for v in (start..end).step_by(hal::paging::PAGE as usize) {
            let Some(phys) = aspace.translate(v) else {
                continue;
            };
            let alias = hhdm + (phys & !(hal::paging::PAGE - 1));
            if aspace.split_huge(alias).is_err() {
                warn!("harden: out of frames splitting the HHDM at {alias:#x}");
                continue;
            }
            if aspace.make_read_only(alias).is_ok() {
                aliases += 1;
            }
        }
    }
    info!("harden: HHDM alias of {aliases} kernel code and read-only pages made read-only");

    // Verify the invariants we most care about.
    let mut ok = true;
    if let Some((_, f)) = aspace.query(s.text.0) {
        ok &= !f.contains(Flags::WRITABLE) && !f.contains(Flags::NO_EXECUTE);
    }
    if let Some((_, f)) = aspace.query(s.data.0) {
        ok &= f.contains(Flags::NO_EXECUTE);
    }
    // RELRO: the GOT and friends are read-only after relocation (#126).
    if s.relro.0 < s.relro.1 {
        ok &= aspace
            .query(s.relro.0)
            .is_some_and(|(_, f)| !f.contains(Flags::WRITABLE) && f.contains(Flags::NO_EXECUTE));
    }
    if let Some(f) = aspace.top_level_flags(hhdm) {
        ok &= f.contains(Flags::NO_EXECUTE);
    }
    assert!(ok, "harden: W^X/NX invariants not satisfied");
    // The HHDM maps the same frames again: no page of .text or .rodata may be
    // writable through it, or a write there would change code (#119).
    let aliases_read_only = [s.text, s.rodata, s.relro].into_iter().all(|(start, end)| {
        (start..end)
            .step_by(hal::paging::PAGE as usize)
            .filter_map(|v| aspace.translate(v))
            .all(|phys| {
                aspace
                    .query(hhdm + phys)
                    .is_none_or(|(_, f)| !f.contains(Flags::WRITABLE))
            })
    });
    assert!(
        aliases_read_only,
        "harden: kernel code or read-only data is writable through the HHDM"
    );
    info!("harden: verified - .text R-X, .rodata/RELRO R+NX, .data NX, HHDM NX, no W+X");
}

/// Unmap the stack guard pages so an overflow of a HAL-owned stack faults.
fn install_guard_pages() {
    let mut guard = KERNEL_SPACE.lock();
    let Some(aspace) = guard.as_mut() else {
        return;
    };
    let mut n = 0;
    for page in hal::gdt::guard_pages() {
        if aspace.unmap(page) {
            n += 1;
        }
    }
    aspace.reload();
    info!("guard: unmapped {} stack guard pages", n);
}

/// Grow the heap on demand: map `max(min_bytes, HEAP_CHUNK)` of fresh,
/// zeroed frames (RW+NX) contiguously at the next heap address, and return the
/// newly mapped `(base, size)`. Called by the allocator when no block fits.
///
/// Must not itself allocate on the heap (it runs while the heap lock is held);
/// it only touches the frame allocator and the kernel page tables.
fn heap_grow(min_bytes: usize) -> Option<(usize, usize)> {
    // A request beyond the whole heap window can never be met; refusing it
    // first keeps every sum below from overflowing (#121).
    let min_bytes = u64::try_from(min_bytes).ok()?;
    if min_bytes > HEAP_LIMIT - HEAP_BASE {
        return None;
    }
    let want = min_bytes.max(HEAP_CHUNK);
    let pages = want.div_ceil(hal::paging::PAGE);
    let size = pages * hal::paging::PAGE;
    let base = HEAP_NEXT.load(Ordering::Relaxed);

    // Never grow into the kernel/KASLR window (#61): the heap stays strictly
    // below it, so it can never overlap the slid kernel image.
    if base.checked_add(size)? > HEAP_LIMIT {
        return None;
    }

    let mut guard = KERNEL_SPACE.lock();
    let aspace = guard.as_mut()?;
    let flags = Flags::WRITABLE | Flags::NO_EXECUTE;
    for i in 0..pages {
        let va = base + i * hal::paging::PAGE;
        let mapped = hal::frame::alloc_frame().is_some_and(|frame| {
            let ok = aspace.map(va, frame.addr(), flags).is_ok();
            if !ok {
                hal::frame::free_frame(frame);
            }
            ok
        });
        if !mapped {
            // Out of frames part-way: undo this call's pages, so nothing is
            // orphaned and the next grow can retry the same base (#63).
            for done in (0..i).map(|p| base + p * hal::paging::PAGE) {
                if let Some(phys) = aspace.translate(done) {
                    aspace.unmap(done);
                    hal::frame::free_frame(hal::frame::PhysFrame::containing(phys));
                }
            }
            return None;
        }
    }
    HEAP_NEXT.store(base + size, Ordering::Relaxed);
    Some((base as usize, size as usize))
}

/// Bring up the kernel heap and exercise it (forcing a growth and verifying
/// `Vec`/`Box` round-trip correctly).
fn init_heap() {
    if !HEAP.init(heap_grow) {
        warn!("heap: init failed (could not map initial region)");
        return;
    }

    // Grow past the initial chunk (40k u64 = 320 KiB > 256 KiB) to exercise the
    // grow + coalesce path, and check the data survives reallocation.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..40_000u64 {
        v.push(i);
    }
    let sum: u64 = v.iter().sum();
    let expected: u64 = (0..40_000u64).sum();
    let boxed = Box::new(0xC0FFEE_u64);
    let ok = sum == expected && *boxed == 0xC0FFEE;
    drop(v);
    drop(boxed);

    // A request too large to meet is refused with null, never an overflow
    // panic or a wrapped size (#121); freed memory comes back zeroed (#69).
    let huge = Vec::<u8>::new().try_reserve(isize::MAX as usize / 2);
    assert!(huge.is_err(), "heap: a huge request did not fail cleanly");
    assert!(
        HEAP.verify_zero_on_free(),
        "heap: a freed block was handed out with its old contents"
    );

    let (live, mapped) = HEAP.stats();
    if ok {
        info!(
            "heap: ok - Vec/Box verified; {} KiB mapped, {} bytes live",
            mapped / 1024,
            live
        );
    } else {
        warn!("heap: self-test FAILED");
    }
}

/// A [`Write`] sink that drops everything, for self-checks that print.
struct Discard;

impl Write for Discard {
    fn write_str(&mut self, _: &str) -> core::fmt::Result {
        Ok(())
    }
}

/// Register named memory regions for page-fault classification, then verify the
/// recoverable probe (a guard page faults and recovers; mapped memory reads
/// fine) and the classifier.
fn init_fault_diagnostics(boot_info: &BootInfo) {
    use hal::fault::{classify, probe_read, register_region};

    let s = boot::kernel_sections();
    register_region(".text", s.text.0, s.text.1);
    register_region(".rodata", s.rodata.0, s.rodata.1);
    register_region(".data/.bss", s.data.0, s.data.1);
    register_region(
        "HHDM",
        boot_info.hhdm_offset,
        boot_info.hhdm_offset + boot_info.highest_address(),
    );
    // The whole window, not the heap's size today: it keeps growing (#125).
    register_region("kernel heap", HEAP_BASE, HEAP_LIMIT);
    hal::fault::register_classifier(thread::classify_stack);
    for g in hal::gdt::guard_pages() {
        register_region("stack guard", g, g + hal::paging::PAGE);
    }

    // The recoverable probe: a guard page must fault (and recover); mapped
    // kernel code must read without faulting.
    let guard = hal::gdt::guard_pages()[0];
    let guard_faults = probe_read(guard);
    let text_ok = !probe_read(s.text.0);
    assert!(guard_faults, "guard page should be unmapped");
    assert!(text_ok, "mapped .text should be readable");
    info!("pf: probe verified - guard page faults & recovers, mapped memory reads ok");

    // A backtrace over a bad frame link stops instead of faulting (#111).
    let frames = hal::backtrace::print_from(&mut Discard, guard);
    assert_eq!(frames, 0, "backtrace walked an unmapped frame");
    info!("backtrace: an unmapped frame link ends the walk without a fault");

    // Show the classifier naming a few representative addresses.
    let samples = [
        ("text", s.text.0),
        ("heap", HEAP_BASE),
        ("hhdm", boot_info.hhdm_offset),
        ("guard", guard),
    ];
    for (label, addr) in samples {
        debug!(
            "pf: {addr:#x} ({label}) -> {}",
            classify(addr).unwrap_or("unmapped/unknown")
        );
    }
    // The heap is named over its whole window, including memory it grows into
    // later, and a hit on a thread's kernel-stack guard is named as one (#125).
    let grown = HEAP_LIMIT - hal::paging::PAGE;
    assert_eq!(
        classify(grown),
        Some("kernel heap"),
        "pf: later heap growth unnamed"
    );
    let (guard_va, stack_va) = thread::kernel_stack_probe(5);
    assert_eq!(
        classify(guard_va),
        Some("kernel stack guard"),
        "pf: a kernel-stack guard hit is unnamed"
    );
    assert_eq!(
        classify(stack_va),
        Some("kernel stack"),
        "pf: a kernel stack is unnamed"
    );
}

/// Demonstrate Untyped retype (#55): take one frame as a small Untyped region,
/// carve object storage from it with watermark accounting, and show that an
/// over-budget request is rejected.
fn demo_untyped(boot_info: &BootInfo) {
    use capability::Untyped;

    let Some(frame) = hal::frame::alloc_frame() else {
        warn!("untyped: no frame for the demo");
        return;
    };
    let hhdm = boot_info.hhdm_offset;
    let mut ut = Untyped::new(frame.addr(), hal::paging::PAGE); // 4 KiB region

    // Carve 8 objects of 256 bytes, 64-byte aligned, and zero them.
    match hal::object::retype(&mut ut, 256, 64, 8, hhdm) {
        Some(base) => info!(
            "untyped: retyped 8x256B at {:#x}; {} B used, {} B free of {}",
            base,
            ut.watermark() - ut.base(),
            ut.free(),
            ut.size()
        ),
        None => warn!("untyped: retype failed unexpectedly"),
    }

    // An over-budget request (a whole 4 KiB object) must be rejected, leaving
    // the watermark untouched.
    let before = ut.free();
    let rejected = hal::object::retype(&mut ut, hal::paging::PAGE, hal::paging::PAGE, 1, hhdm);
    assert!(rejected.is_none(), "over-budget retype should fail");
    assert_eq!(ut.free(), before, "failed retype must not consume space");
    info!("untyped: over-budget retype correctly rejected; accounting intact");

    hal::frame::free_frame(frame);
}

/// Demonstrate the capability path end to end (#56): from one Untyped region,
/// retype a CNode (which backs a capability space), an Endpoint, and a
/// Notification; install full-rights capabilities to the objects; then derive
/// an attenuated (SEND-only) copy and verify the rights.
fn demo_capabilities(boot_info: &BootInfo) {
    use capability::Untyped;
    use capability::{cnode_bytes, slot_align, CapId, Capability, ObjectKind, ObjectRef, Rights};

    const CNODE_SLOTS: usize = 16;

    let Some(frame) = hal::frame::alloc_frame() else {
        warn!("caps: no frame for the demo");
        return;
    };
    let hhdm = boot_info.hhdm_offset;
    let mut ut = Untyped::new(frame.addr(), hal::paging::PAGE);

    // Retype a CNode and form the capability space over its storage.
    let Some(cnode_phys) = hal::object::retype(
        &mut ut,
        cnode_bytes(CNODE_SLOTS) as u64,
        slot_align() as u64,
        1,
        hhdm,
    ) else {
        warn!("caps: CNode retype failed");
        hal::frame::free_frame(frame);
        return;
    };
    // `cnode_phys` is a CNode just retyped for CNODE_SLOTS slots (our exclusive
    // HHDM-mapped storage), and we form exactly one CapSpace over it.
    let mut cspace = hal::object::cnode_cap_space(cnode_phys, CNODE_SLOTS, hhdm);

    // Retype an Endpoint and a Notification.
    let (esz, eal) = ObjectKind::Endpoint.fixed_layout().unwrap();
    let (nsz, nal) = ObjectKind::Notification.fixed_layout().unwrap();
    let (Some(ep_phys), Some(nt_phys)) = (
        hal::object::retype(&mut ut, esz, eal, 1, hhdm),
        hal::object::retype(&mut ut, nsz, nal, 1, hhdm),
    ) else {
        warn!("caps: object retype failed");
        hal::frame::free_frame(frame);
        return;
    };

    // Install full-rights capabilities to the objects.
    let ep = Capability::new(ObjectRef::new(ObjectKind::Endpoint, ep_phys), Rights::ALL);
    let nt = Capability::new(
        ObjectRef::new(ObjectKind::Notification, nt_phys),
        Rights::ALL,
    );
    let ep_id = cspace.insert(ep).unwrap();
    let _nt_id = cspace.insert(nt).unwrap();

    // Derive an attenuated SEND-only copy of the endpoint cap into another slot.
    cspace.copy(ep_id, CapId(10), Rights::SEND).unwrap();

    let full = cspace.get(ep_id).unwrap();
    let weak = cspace.get(CapId(10)).unwrap();
    let ok = full.allows(Rights::SEND | Rights::RECV)
        && weak.allows(Rights::SEND)
        && !weak.allows(Rights::RECV)
        && weak.object == full.object;
    assert!(ok, "attenuated capability derivation is wrong");

    info!(
        "caps: CNode cap space ({} slots) holds endpoint+notification; SEND-only copy attenuated & verified",
        cspace.capacity()
    );

    hal::frame::free_frame(frame);
}

/// Exercise the invocation API end to end (#57): retype a Frame from an Untyped
/// *through* `invoke`, identify it, show a rights-denied invocation, and signal
/// a Notification.
fn demo_invoke(boot_info: &BootInfo) {
    use capability::{
        cnode_bytes, slot_align, CapId, Capability, InvokeError, InvokeOk, Method, ObjectKind,
        ObjectRef, Rights, Untyped,
    };

    const SLOTS: usize = 16;
    let hhdm = boot_info.hhdm_offset;

    let (Some(ctrl), Some(region)) = (hal::frame::alloc_frame(), hal::frame::alloc_frame()) else {
        warn!("invoke: no frames for the demo");
        return;
    };

    // Carve a CNode (cap space), an Untyped control block, and a Notification
    // from the control frame using a local bootstrap region.
    let mut boot = Untyped::new(ctrl.addr(), hal::paging::PAGE);
    let un = core::mem::size_of::<Untyped>() as u64;
    let ua = core::mem::align_of::<Untyped>() as u64;
    let (nsz, nal) = ObjectKind::Notification.fixed_layout().unwrap();
    let (Some(cnode_phys), Some(uctrl_phys), Some(notif_phys)) = (
        hal::object::retype(
            &mut boot,
            cnode_bytes(SLOTS) as u64,
            slot_align() as u64,
            1,
            hhdm,
        ),
        hal::object::retype(&mut boot, un, ua, 1, hhdm),
        hal::object::retype(&mut boot, nsz, nal, 1, hhdm),
    ) else {
        warn!("invoke: bootstrap retype failed");
        hal::frame::free_frame(ctrl);
        hal::frame::free_frame(region);
        return;
    };

    // The Untyped capability's control block describes the (separate) region.
    hal::object::store_untyped(
        uctrl_phys,
        hhdm,
        Untyped::new(region.addr(), hal::paging::PAGE),
    );

    let mut cs = hal::object::cnode_cap_space(cnode_phys, SLOTS, hhdm);
    let ut_id = cs
        .insert(Capability::new(
            ObjectRef::new(ObjectKind::Untyped, uctrl_phys),
            Rights::ALL,
        ))
        .unwrap();
    let nt_id = cs
        .insert(Capability::new(
            ObjectRef::new(ObjectKind::Notification, notif_phys),
            Rights::ALL,
        ))
        .unwrap();

    // 1) Retype a Frame from the Untyped, through invoke.
    let created = invoke::invoke(
        &mut cs,
        ut_id,
        Method::UntypedRetype {
            kind: ObjectKind::Frame,
            count: 1,
            dst: CapId(5),
        },
        hhdm,
    );
    let frame_phys = match created {
        Ok(InvokeOk::Created(obj)) => obj.phys,
        other => {
            warn!("invoke: retype returned {:?}", other);
            hal::frame::free_frame(ctrl);
            hal::frame::free_frame(region);
            return;
        }
    };

    // 2) Identify the new Frame.
    let identified = invoke::invoke(&mut cs, CapId(5), Method::FrameIdentify, hhdm);
    assert_eq!(identified, Ok(InvokeOk::Phys(frame_phys)));

    // 3) Rights denied: a copy of the Untyped cap without RETYPE cannot retype.
    cs.copy(ut_id, CapId(6), Rights::WRITE).unwrap();
    let denied = invoke::invoke(
        &mut cs,
        CapId(6),
        Method::UntypedRetype {
            kind: ObjectKind::Frame,
            count: 1,
            dst: CapId(7),
        },
        hhdm,
    );
    assert_eq!(denied, Err(InvokeError::InsufficientRights));

    // 4) Signal the Notification, then confirm the bits landed.
    let _ = invoke::invoke(
        &mut cs,
        nt_id,
        Method::NotificationSignal { bits: 0b1010 },
        hhdm,
    );
    assert_eq!(
        hal::object::notification_pending(notif_phys, hhdm) & 0b1010,
        0b1010
    );

    info!("invoke: retype->frame, identify, rights-denied, and signal all behaved correctly");

    hal::frame::free_frame(ctrl);
    hal::frame::free_frame(region);
}

/// Advance the round-robin queue: rotate the running thread to the back and
/// return `(current, next)` if that picked a different thread to run.
///
/// The caller must keep interrupts disabled from here until the switch to
/// `next` (a timer preemption in between would rotate the queue again and
/// leave `current` stale); the scheduler lock is released before switching.
fn pick_next() -> Option<(sched::ThreadId, sched::ThreadId)> {
    let mut s = SCHED.lock();
    let cur = s.current()?;
    let next = s.rotate()?;
    (cur != next).then_some((cur, next))
}

/// Cooperatively yield to the next ready thread. Interrupts are off from the
/// pick through the switch, and restored when this thread is resumed.
fn yield_now() {
    hal::cpu::without_interrupts(|| {
        if let Some((cur, next)) = pick_next() {
            thread::switch_to(cur, next);
        }
    });
}

/// Exit the running thread: take it out of the run queue for good and switch
/// to the next one. Its context is never resumed (and its stack is leaked).
fn exit_current() -> ! {
    hal::cpu::disable_interrupts();
    let cur = SCHED
        .lock()
        .current()
        .expect("sched: exit with an empty run queue");
    // A caller waiting on this thread for a reply would wait forever: wake it
    // with an error (lock order: IPC before SCHED).
    let woken = ipc::exit_thread(cur);
    let next = {
        let mut s = SCHED.lock();
        for t in woken.woken() {
            s.admit(t);
        }
        s.remove(cur);
        thread::mark_exited(cur);
        // Removing the front makes the next thread in line current.
        s.current().expect("sched: the last runnable thread exited")
    };
    thread::switch_to(cur, next);
    unreachable!("sched: an exited thread was resumed");
}

/// LAPIC timer interrupt (#80): count the tick and preempt the running thread
/// in favor of the next ready one.
///
/// Runs with interrupts disabled (interrupt gate), on the interrupted thread's
/// kernel stack. If it switches away, the interrupted state stays saved in the
/// `TrapFrame` on that stack; when the thread is next scheduled, `switch`
/// returns here and the stub's `iretq` resumes it exactly where it was. No
/// lock can be held at this point - every `SpinLock` holder runs with
/// interrupts off - so taking the scheduler lock cannot deadlock.
fn timer_interrupt(frame: &mut hal::idt::TrapFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    // EOI before switching: the next thread may run for a whole time slice
    // before this handler resumes, and the APIC delivers no further timer
    // interrupt until this one is acknowledged.
    hal::lapic::eoi();
    // Ring 3 is preempted too: each user thread enters the kernel on its own
    // kernel stack (#82), so its TrapFrame stays put while others run.
    let from_user = frame.cs & 3 == 3;
    if let Some((cur, next)) = pick_next() {
        PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        if from_user {
            thread::USER_PREEMPTIONS.fetch_add(1, Ordering::Relaxed);
        }
        thread::switch_to(cur, next);
    }
}

/// Demo worker A: bump its counter and yield, forever (abandoned when the
/// kernel halts).
extern "C" fn worker_a() -> ! {
    loop {
        A_TICKS.fetch_add(1, Ordering::Relaxed);
        yield_now();
    }
}

/// Demo worker B.
extern "C" fn worker_b() -> ! {
    loop {
        B_TICKS.fetch_add(1, Ordering::Relaxed);
        yield_now();
    }
}

/// Bring up two kernel threads and round-robin among them and the boot thread
/// cooperatively, proving the context switch and scheduler work (#78).
fn demo_threads(boot_info: &BootInfo) {
    use sched::ThreadId;

    let hhdm = boot_info.hhdm_offset;
    let (Some(stack_a), Some(stack_b)) = (hal::frame::alloc_frame(), hal::frame::alloc_frame())
    else {
        warn!("sched: no frames for thread stacks");
        return;
    };
    // One zeroed frame each as a (small) kernel-thread stack, reached via the
    // HHDM. Kept for the rest of boot (the threads are abandoned at halt).
    CONTEXTS[1].init(hhdm + stack_a.addr() + hal::paging::PAGE, worker_a);
    CONTEXTS[2].init(hhdm + stack_b.addr() + hal::paging::PAGE, worker_b);

    {
        let mut s = SCHED.lock();
        s.admit(ThreadId(0)); // the boot thread
        s.admit(ThreadId(1)); // worker A
        s.admit(ThreadId(2)); // worker B
    }

    info!("sched: cooperative round-robin across the boot thread + 2 kernel threads");
    for _ in 0..3 {
        yield_now();
    }

    let (a, b) = (
        A_TICKS.load(Ordering::Relaxed),
        B_TICKS.load(Ordering::Relaxed),
    );
    assert!(a > 0 && b > 0, "worker threads never ran");
    info!("sched: both threads ran via context switch (A={a}, B={b})");

    // Retire the cooperative workers: once the timer runs they would otherwise
    // spin through `yield_now` forever. Their saved contexts are never resumed.
    let mut s = SCHED.lock();
    s.remove(ThreadId(1));
    s.remove(ThreadId(2));
}

/// Bring up preemption (#80): retire the legacy PIC, enable the Local APIC,
/// calibrate its timer against the PIT, start it at [`TIMER_HZ`] on
/// [`TIMER_VECTOR`], and enable interrupts on the boot thread. Returns whether
/// the timer is running.
fn init_preemption() -> bool {
    hal::pic::disable();
    info!("pic: legacy 8259s remapped to 0x20-0x2f and masked");

    let apic_id = {
        let mut guard = KERNEL_SPACE.lock();
        let Some(aspace) = guard.as_mut() else {
            warn!("lapic: no address space");
            return false;
        };
        // The window's top-level table exists for every kernel slot (see
        // `populate_higher_half`), so check the page itself is unused.
        if aspace.translate(LAPIC_MMIO_VA).is_some() {
            warn!("lapic: MMIO window {LAPIC_MMIO_VA:#x} is already in use");
            return false;
        }
        match hal::lapic::init(aspace, LAPIC_MMIO_VA) {
            Ok(id) => id,
            Err(e) => {
                warn!("lapic: init failed: {e:?}");
                return false;
            }
        }
    };
    info!("lapic: enabled (id {apic_id}), registers at {LAPIC_MMIO_VA:#x} (UC)");

    hal::idt::register_handler(TIMER_VECTOR, timer_interrupt);
    let cfg = match hal::lapic::start_periodic(TIMER_VECTOR, TIMER_HZ) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!("lapic: timer start failed: {e:?}");
            return false;
        }
    };
    info!(
        "lapic: timer calibrated against the PIT: {} ticks/ms (div 16); periodic {} Hz, count {}, vector {:#x}",
        cfg.ticks_per_ms, TIMER_HZ, cfg.initial_count, TIMER_VECTOR
    );

    hal::cpu::enable_interrupts();
    true
}

/// Preemption demo body (#80): spin for [`SPIN_ROUNDS`] rounds of
/// [`SPIN_ROUND_TICKS`] timer ticks each, **never yielding**, logging both
/// spinners' work counters after each round. The other spinner's counter
/// advancing between two of our rounds proves the timer switched us out.
fn spin(slot: usize, name: char) -> ! {
    for round in 1..=SPIN_ROUNDS {
        let start = TICKS.load(Ordering::Relaxed);
        while TICKS.load(Ordering::Relaxed).wrapping_sub(start) < SPIN_ROUND_TICKS {
            SPIN_WORK[slot].fetch_add(1, Ordering::Relaxed);
            core::hint::spin_loop();
        }
        info!(
            "preempt: spinner {name} round {round}/{SPIN_ROUNDS}: work C={} D={} (tick {}, {} preemptions)",
            SPIN_WORK[0].load(Ordering::Relaxed),
            SPIN_WORK[1].load(Ordering::Relaxed),
            TICKS.load(Ordering::Relaxed),
            PREEMPTIONS.load(Ordering::Relaxed),
        );
    }
    SPINNERS_DONE.fetch_add(1, Ordering::Release);
    exit_current()
}

/// Spinner C (#80).
extern "C" fn spinner_c() -> ! {
    spin(0, 'C')
}

/// Spinner D (#80).
extern "C" fn spinner_d() -> ! {
    spin(1, 'D')
}

/// Run two kernel threads that never yield alongside the boot thread, and wait
/// until both finish: they can only make progress if the timer preempts (#80).
fn demo_preemption() {
    use sched::ThreadId;

    // Heap-backed 16 KiB stacks: unlike the one-frame cooperative workers,
    // these take interrupt frames and format log lines. Leaked, like the exited
    // threads' contexts.
    let spinners: [(usize, extern "C" fn() -> !); 2] = [(3, spinner_c), (4, spinner_d)];
    for (id, entry) in spinners {
        let stack = Box::leak(alloc::vec![0u8; SPIN_STACK_BYTES].into_boxed_slice());
        let top = stack.as_ptr() as u64 + SPIN_STACK_BYTES as u64;
        CONTEXTS[id].init(top, entry);
    }
    {
        let mut s = SCHED.lock();
        s.admit(ThreadId(3));
        s.admit(ThreadId(4));
    }
    info!("preempt: started spinners C and D (they never yield; only the timer switches them)");

    // The boot thread idles; each tick may preempt it in favor of a spinner.
    while SPINNERS_DONE.load(Ordering::Acquire) < 2 {
        hal::cpu::halt();
    }

    let (c, d) = (
        SPIN_WORK[0].load(Ordering::Relaxed),
        SPIN_WORK[1].load(Ordering::Relaxed),
    );
    let preemptions = PREEMPTIONS.load(Ordering::Relaxed);
    assert!(
        c > 0 && d > 0 && preemptions > 0,
        "spinners were not time-sliced"
    );
    info!(
        "preempt: both spinners finished, time-sliced by the timer (C={c}, D={d}; {preemptions} preemptions in {} ticks)",
        TICKS.load(Ordering::Relaxed)
    );
}

/// Set by the first panic: a panic while reporting one halts at once (#111).
static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    hal::cpu::disable_interrupts();
    if PANICKING.swap(true, Ordering::Relaxed) {
        hal::cpu::halt_loop();
    }

    // Report over COM1 directly (not the logger), so a panic while a log lock
    // is held still reports; as configured, not re-initialized, so the bytes
    // of the line before the panic still in its FIFO are kept (#117).
    let mut com1 = SerialPort::com1_configured();
    let _ = writeln!(com1, "\n*** KERNEL PANIC ***");
    let _ = writeln!(com1, "  {info}");
    hal::backtrace::print(&mut com1);

    hal::cpu::halt_loop();
}
