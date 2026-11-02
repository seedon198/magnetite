<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brand/banner-dark.svg">
  <img src="docs/assets/brand/banner-light.svg" alt="Magnetite" width="420">
</picture>

**A hardened, capability-based x86-64 microkernel, written from scratch in Rust.**

</div>

---

# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/) (pre-1.0: anything may change).

## [Unreleased]

### Project: renamed to Magnetite

- The project is now **Magnetite** (Fe₃O₄), at
  <https://github.com/seedon198/magnetite>. GitHub redirects the old
  `seedon198/kernel` URLs. New brand assets in `docs/assets/`, all with text
  converted to outlines: an element-tile logo and README banner in light and
  dark, a small icon, and a social preview. Alongside them, an animated
  recording of a real UEFI boot (`boot.gif`: the serial console with real,
  lightly compressed timing, in the brand palette and window frame) and an
  architecture diagram, plus the scripts that generate them
  (`scripts/brand/`). The README is rewritten around them, with current
  status (v0.3.0, M4 under way) and a milestone table.
- The changelog, code of conduct, contributing guide and security policy open
  with the same brand banner and tagline as the README.

### M4: Scheduling & userspace

- The first user task (#82, PR 2 of 2), per
  [ADR 0019](docs/adr/0019-first-user-task.md). Two separately built Rust
  programs, `user/root` and `user/fault`, are loaded from Limine boot modules
  and run in ring 3.
  - A new pure crate `elf` (`#![forbid(unsafe_code)]`, 22 host tests) validates
    each image before anything is mapped: header, bounds, page congruence,
    user range, no overlap, no `W+X`, entry in an executable segment.
  - The syscall ABI lives in `capability::abi` (numbers, 19 error codes, method
    labels, decoding, the null slot) and `kernel/src/syscall.rs` dispatches it:
    IPC on endpoints, kernel object methods on anything else, and no physical
    address ever returned to ring 3. The entry stub now carries `r12`/`r13`
    and returns the badge and message in registers.
  - `libs/sys` is the user side: syscall stubs, the `_start` entry and
    `sys::entry!`, so user programs stay `#![forbid(unsafe_code)]` (ADR 0007
    amended for it).
  - The root task starts with four slots (null, its CNode, 64 KiB of Untyped,
    a badged send-only console endpoint) and prints through a console server
    over IPC. At every boot it retypes and signals a Notification, is refused
    slots it does not hold (`InvalidCap`) and capability passing without
    `GRANT` (`NoGrant`), and sends malformed console messages, which are
    clamped and repaired. The fault task reads kernel memory and is ended.
  - Every frame an exited task used is reclaimed (address space, page tables,
    kernel stack, and the root task's bootstrap memory); the boot checks the
    free-frame count returns to its starting value.
  - #81 follow-ups: an `InvalidCap` IPC error, a `ReplyRecv` kernel wrapper,
    and the HHDM offset set during init.
  - The frame allocator gains aligned contiguous runs.
  - CI's boot test now requires the root task's success line and `ready:`
    (every boot-time self-check passed), matched as whole kernel log lines so
    console output cannot fake them, and fails at once on a kernel panic;
    before, it passed at the first log line.
  - A missing or malformed program module is refused and the kernel boots on
    without it; a refused retype leaves the task's Untyped untouched.
  - New page: [System calls](docs/architecture/syscalls.md).

- User-mode mechanism (#82, PR 1 of 2), per
  [ADR 0019](docs/adr/0019-first-user-task.md). Each user task gets its own
  address space (`AddressSpace::new_user`), sharing a kernel half whose
  top-level entries are all populated once at boot. Each thread gets its own
  16 KiB kernel stack above an unmapped guard page, and one `switch_to` path
  points `TSS.rsp0` and the syscall entry at it and loads the thread's address
  space. So the timer now preempts ring 3, and the syscall stub keeps the user
  `rsp` on the thread's own stack, ready for threads that block in syscalls.
  A CPU exception raised in ring 3 ends only that task; a caller waiting on it
  for a reply is woken with `PeerExited` (`Ipc::exit`). Verified in QEMU: two
  hand-assembled tasks say hello from ring 3, are preempted there and exit,
  and a third that reads kernel memory is killed while the kernel boots on.
  Hardened after review: `syscall` entry masks `RFLAGS.NT` (user code could
  otherwise crash the kernel through a later `iretq`), no kernel register
  value reaches ring 3 on entry or after a syscall, only exceptions user code
  can cause end a task (never NMI, `#DF` or `#MC`), and the top user page is
  never mappable (no non-canonical `sysretq`). A user address space's copy of
  the kernel half is supervisor-only (Limine leaves `USER` set on some of its
  top-level entries). Each is pinned by a QEMU check.

- Blocking synchronous IPC over Endpoints (#81), per
  [ADR 0018](docs/adr/0018-synchronous-ipc.md). A new pure crate `ipc`
  (`#![forbid(unsafe_code)]`, 31 host tests) holds the engine: Send, Recv,
  Call, Reply and ReplyRecv, a one-shot reply slot, FIFO endpoint queues
  stored in `EndpointState`, and receiver-visible badges. Only the message
  registers in use are delivered, the rest zeroed. A message carries one
  capability, copied under `GRANT` into an empty slot the receiver named; a
  transfer that cannot complete fails the sender and moves nothing, and a
  reply carries a capability only into a slot the caller named. The kernel
  (`kernel/src/ipc.rs`) applies the engine's verdict to the scheduler,
  blocking and waking threads with interrupts off. `kmain` proves it with a
  client and a server kernel thread, each with its own capability space.
  Verified in QEMU. CI now lints and tests every host crate (`cargo xtask
  test`), not only `boot-info`.

- Boot banner: `kmain` opens with the Fe₃O₄ tile logo in ASCII, the wordmark,
  version (`git describe`, stamped by the new `kernel/build.rs` with the
  `rustc` version and build profile) and pre-alpha status, then a system
  summary. The summary covers bootloader and firmware, CPU and its features,
  memory, layout and the KASLR slide, replacing the separate `cpu`, `memory`,
  `layout` and `kaslr` log lines. Boot ends with one `ready:` line listing the
  verified protections. `BootInfo` gains `firmware` and the bootloader's name
  and version, copied out of bootloader-reclaimable memory (`ShortStr`,
  host-tested). See [boot process](docs/architecture/boot.md#boot-banner).

- Preemptive scheduling via the LAPIC timer (#80), per
  [ADR 0017](docs/adr/0017-threads-and-scheduling.md). `hal::pic` remaps the
  legacy 8259s to `0x20`-`0x2F` and masks them; `hal::lapic` maps the xAPIC
  register page uncached at a dedicated kernel VA, enables the APIC (spurious
  vector `0xFF`), calibrates its timer against PIT channel 2 (`hal::pit`, a
  polled 10 ms one-shot), and runs it periodic at 100 Hz on vector `0x30`. The
  kernel's timer handler EOIs and switches to the next round-robin thread;
  fresh threads start with interrupts enabled (`hal::context` `thread_start`
  trampoline), and `yield_now`/thread exit keep interrupts off from picking the
  next thread through the switch (`hal::cpu::without_interrupts`). `kmain` runs
  two kernel threads that never yield and both make interleaved progress,
  verified in QEMU (UEFI + BIOS), plus a 1 kHz lock-contention stress run with
  no deadlock. Ring 3 is not preempted yet (needs per-thread kernel stacks,
  #82). The interrupt-safety rules are documented in
  `docs/architecture/interrupts.md`.

- Ring 3 + the syscall boundary (#79), per
  [ADR 0017](docs/adr/0017-threads-and-scheduling.md). `hal::syscall` programs
  `EFER.SCE`/`STAR`/`LSTAR`/`FMASK`, provides the `syscall` entry stub (switch to
  a kernel stack, build the args, dispatch, `sysret`) and `enter_user` (first
  drop to ring 3 via `iretq`); `hal::context::resume` hands control back to a
  saved thread one-way. The message-register ABI is the pure `capability::abi`
  (`MessageInfo`: label/length/caps, host-tested). `kmain` maps a tiny user stub
  (R-X user) and stack (RW+NX user), enters ring 3, and the stub `syscall`s in
  and out - the kernel decodes the message and returns to the boot thread.
  Verified in QEMU. Also a paging fix: `map` now upgrades an existing
  intermediate table's USER/WRITABLE bits when a user mapping reuses it (U/S and
  R/W are cumulative across levels).

- Threads, context switch, and a cooperative round-robin scheduler (#78), per
  [ADR 0017](docs/adr/0017-threads-and-scheduling.md). A new pure `sched` crate
  (`#![forbid(unsafe_code)]`, host-tested) is the round-robin policy - a ready
  queue of `ThreadId`s with admit/rotate/remove. `hal::context` saves/restores a
  thread's callee-saved registers and stack pointer (`Context` + `switch`), and
  `Context::init` crafts a fresh thread's stack to begin at an entry point.
  `kmain` runs two kernel threads cooperatively round-robining with the boot
  thread, verified in QEMU (both ran via the context switch). Preemption, ring
  3, and IPC build on this.

### Security

- Closed the compatibility-mode `syscall` entry (#102). GDT slot `0x18` was a
  present DPL 3 32-bit code segment and `IA32_CSTAR` was never set, so a ring-3
  task could far-return into compatibility mode, `syscall`, and enter ring 0 at
  a firmware-chosen address on a stack it picked (a kernel double fault at
  least). Slot `0x18` is now null (`sysretq` never reads it), `CSTAR` points at
  a stub that switches to the thread's kernel stack and kills it, and the
  `SYSENTER` MSRs are zeroed so `sysenter` faults in ring 3. The user-mode demo
  gains two tasks that try each path and must die by a fault in ring 3: at
  the far return itself, and at `sysenter`.
- The IDT's `global_asm!` switched to `.rodata` for its stub table and never
  switched back, so asm the compiler placed after it in the same codegen unit
  (the syscall and ring-3 entry stubs) could land in non-executable `.rodata`
  and fault on first use, depending on how the build was partitioned. It now
  uses `.pushsection`/`.popsection`.
- The crash backtrace could follow a user-controlled `rbp` and recurse on a
  nested page fault (#111). `syscall_entry` called Rust with the user's `rbp`
  loaded and the IDT common stub set `rbp` to the trap frame, so a walk read
  user-chosen or meaningless links, and an unmapped one faulted inside the
  reporter, which reported again until a double fault. Both entries now zero
  `rbp` (the syscall entry saves and restores the user's), the walk probes
  each frame with `fault::probe_read` before reading it, the exception
  reporter and the panic handler halt on reentry, and the reporter prints no
  kernel backtrace for a ring-3 frame. Two boot checks cover it: a walk from
  an unmapped link returns no frames, and a ring-3 task with a marker `rbp`
  checks that the kernel chain ends at 0 and its `rbp` comes back.
- Limine was cloned from the moving `v11.x-binary` branch head with no
  verification, and its Makefile runs on the host (#112): a force-push or
  compromise upstream reached every CI job and contributor build, and two
  builds of one commit could boot different bootloaders. `xtask` now pins the
  v11.4.1 commit, fetches exactly that commit, and on every build checks
  that `limine/` is still at it with no tracked file modified.
- Kernel `.text` and `.rodata` stayed writable through the HHDM alias (#119),
  bypassing W^X: a write to `hhdm + phys` changed code that then ran at the
  normal address. `harden_memory` now splits the HHDM huge pages covering the
  image down to 4 KiB (a new pure `page_table::split_huge`, host-tested, and
  `AddressSpace::split_huge`) and makes the alias of every code and read-only
  page read-only and NX (`AddressSpace::make_read_only`, which keeps cache
  and global bits). The boot verification checks every alias page.
- `reap_root` freed the root task's capability-backed memory with no revoke
  and no check that anything else still named it (#105). Safe today only
  because no capability with `GRANT` leaves a task; once one did, a stale
  capability would reach whatever reused the frames. Reclaim now scans every
  capability space (`ipc::find_reference`, using a new `CapSpace::iter`)
  and keeps the memory, reporting the holder, while any capability names an
  object inside it. A boot check plants such a capability and verifies the
  memory is kept, then freed once it is deleted. Revoke itself is still to
  come.

### Fixed

- LAPIC timer calibration accepted a missing PIT (#109). With no 8254 (for
  example `-M q35,pit=off`), port `0x61` reads OUT2 high at once, the
  calibration window ended immediately, and the timer was programmed about
  500x too fast while the log still said 100 Hz. Calibration now requires
  OUT2 to read low once the one-shot is programmed, bounds its wait, and
  rejects any result outside 625 to 625,000 ticks/ms; on failure the kernel
  warns and keeps cooperative scheduling. A new CI boot mode, `uefi-nopit`,
  checks this.
- `cli`, `sti`, `hlt`, `int3` and the CR4 write were marked `nomem` (#110),
  which lets the compiler move loads and stores across them; atomic orderings
  do not order such asm. A `SpinLock` release store could in principle be
  scheduled after its `sti`, letting the timer interrupt spin forever on a
  lock still marked held. They are now compiler barriers. Current codegen was
  already correct, so no boot behaviour changes.
- The Rust toolchain was not pinned (#114): `rust-toolchain.toml` said
  `channel = "stable"`, so each new release could change lints under
  `-D warnings` and the binaries contributors built. It is now pinned to
  `1.99.1`, to be bumped deliberately in its own PR.
- Syscall boundary hardening and fixes (#103, #104, #131, #135):
  - The `sysretq` epilogue checks that the return address is canonical and
    kills the caller otherwise, rather than raising `#GP` in ring 0 on the
    user's stack. The compat-mode hook from #102 becomes a general
    `hal::syscall::register_kill` with a `Kill` reason. A compile-time check
    pins the `USER_TOP` invariant that keeps this unreachable today.
  - A ring-3 fault report prints CR2 only for `#PF`, plus the error code; for
    other exceptions CR2 held some other context's last fault.
  - Reaping a thread clears its debug-syscall permission, so a later task
    given the same id cannot use them.
  - `sys::invoke` refuses more than four argument words with
    `InvalidSyscall` instead of silently dropping the rest; the root task
    checks it.
- Console server (#128, #129): every control character a task sends is
  escaped in the log (C0 and DEL as `\xNN`, C1 as `\u{NN}`), so a `\r` or an
  ANSI escape can no longer overwrite the `console[N]:` prefix and forge a
  kernel line; and line buffers track ownership with an `in_use` flag rather
  than treating badge 0 as free. A boot self-check covers both.
- Scheduling (#107, #130, #136): ADR 0018 now states that the boot thread is
  the idle thread (it never blocks or exits, and idles in `hlt`), and
  blocking asserts it. `alloc_kernel_stack` refuses an occupied slot up front
  and undoes its own partial allocation, so a failed `spawn` can never free
  another thread's live stack; a boot check allocates over thread 7's slot.
  Boot-time waits for tasks go through `thread::wait_exit`, which gives up
  after a yield budget and panics naming the task and whether it was
  blocked, instead of hanging until CI's timeout.
- Paging (#120, #123): `map` returns `MapError::HugePage` instead of writing
  a page-table entry into the memory a large page maps, and `map`/`protect`
  refuse the `HUGE` bit as a leaf flag (it is PAT on a 4 KiB entry);
  `translate` and `query` mask large-page addresses with a new
  `PageTableEntry::huge_addr`, so a PAT bit no longer shifts the result by
  4 KiB; `free_subtree` accepts a PAT leaf. The two `init_paging` self-test
  spaces are now destroyed (they leaked five frames per boot), the boot
  asserts the frame count around them, and `AddressSpace` is `#[must_use]`.
- Kernel heap (#63, #69, #121): every size and address sum in `Heap::alloc`,
  the `GlobalAlloc` growth request and `heap_grow` is checked, so a huge
  `Layout` gets null instead of an overflow panic (dev) or a wrapped,
  too-small block (release). Blocks are zeroed when freed, and list headers
  absorbed by a merge are cleared, so freed memory keeps no stale kernel
  data. `heap_grow` undoes a partial growth when frames run out, instead of
  orphaning the pages and wedging later growth. The heap self-test checks
  the first two.
- The frame allocator accepted frees of frames it never owned (#122): frame
  0, the bitmap's own storage, reserved ranges and MMIO holes all went into
  the pool. A second bitmap now records the pool, and `free` returns a new
  `FreeError::NotPoolFrame` for anything outside it (host-tested).
- `unmap` frees each intermediate page table it leaves empty (#64), from the
  page table up, but never the root or a kernel-half PDPT (shared by every
  user space); the paging self-test checks the frame count around a map and
  unmap (it lost three tables before).
- ELF loading (#132, #133): `elf::parse` refuses an image whose segments
  cover more than `MAX_IMAGE_PAGES` (4096 pages, 16 MiB) with
  `ElfError::TooLarge`, before anything is mapped, so a hostile image cannot
  drain the frame allocator first. The per-page copy plan moves into `elf`
  as `Segment::load_plan`, host-tested with segments that start mid-page,
  straddle a page, and mix file bytes with bss; `map_image` follows it.

## [0.3.0] - 2026-10-28

M3: the capability model - the microkernel's security core. Unforgeable
capabilities with attenuation-only rights, a per-task capability space backed
by CNodes, Untyped memory and retype (the kernel never heap-allocates objects),
the core object kinds, and a rights-checked invocation API. Also fixes two
issues found in a code review. Blocking IPC and the ring-3 `syscall` entry need
threads and are part of M4.

### Fixed

- Kernel heap could collide with the KASLR-slid kernel image (#61): the heap
  base moved below the top-2-GiB KASLR window (to `0xffffff8000000000`) and heap
  growth is now capped at the window start, so the heap can never overlap the
  slid kernel image by construction.
- SpinLock is now interrupt-safe (#62): holding a lock disables interrupts on
  the local core and restores the prior state on release, so an interrupt
  handler cannot preempt a holder and deadlock re-acquiring it. The lock-order
  discipline (frame/page-table locks below the heap lock) is documented in
  `hal::sync`.

### M3: Capabilities & IPC

- Documentation (#58): `docs/architecture/capabilities.md` documents the whole
  capability model (rights/attenuation, cap space & CNodes, Untyped/retype,
  object kinds, the invocation API, and the pure-crate/hal layering), and what
  is deferred to M4 (blocking IPC, the syscall entry, Thread, Untyped
  splitting/revoke). The security-model doc, threat model, roadmap, and README
  are updated to match. **M3's capability model is complete.**

- Capability invocation API (#57): a new pure `capability::invoke` module -
  `Method` (CNode copy/mint/move/delete, `FrameIdentify`, `UntypedRetype`,
  `NotificationSignal`), `InvokeOk`/`InvokeError`, and `Method::authorize`
  (checks the invoked capability's kind and required rights) - host-tested. The
  kernel's `invoke(cspace, cap, method, hhdm)` resolves the capability,
  authorizes it, and performs the effect (CNode ops on the cap space; retype
  via the Untyped control block; notification signal through the HHDM). This is
  the surface M4's ring-3 `syscall` will call. `kmain` drives it: retype a Frame
  through `invoke`, identify it, show a rights-denied invocation, and signal a
  Notification - all verified.

- Core kernel object types & CNode-backed capability space (#56): `capability::
  object` gains per-kind storage layouts (`ObjectKind::fixed_layout`; `Frame`/
  `AddressSpace` = a page, `Endpoint`/`Notification` zeroed state structs) and
  CNode sizing (`cnode_bytes`/`slot_align`). `hal::object::cnode_cap_space`
  forms a `CapSpace` over a CNode's retyped storage (HHDM-backed, slots
  initialized to empty). `kmain` now runs the full path end to end: retype a
  CNode, an Endpoint, and a Notification from one Untyped, install full-rights
  capabilities, and derive an attenuated SEND-only copy - verified. Objects, the
  cap space, and the caps all live in Untyped memory.

- Untyped memory & retype (#55): a new `capability::object` module (pure,
  host-tested) adds `ObjectKind`, `ObjectRef { kind, phys }` (the kernel's
  object handle), and `Untyped` - a physical region with a bump watermark whose
  `allocate` carves naturally-aligned, fixed-size object storage (checked for
  alignment, overflow, and exhaustion; reclaim is whole-region only, per
  ADR 0016). `hal::object::retype` carves from an `Untyped` and **zeroes** the
  storage through the HHDM. `kmain` demonstrates a retype and that an
  over-budget request is rejected with accounting intact.

- Per-task capability space (#54): a `CapSpace` in the `capability` crate - a
  flat table of capability slots (`CapId` = index, O(1) lookup) over
  caller-provided storage, with `insert`/`get`/`delete`/`move`/`copy`/`mint`.
  A slot fills only when empty; `copy`/`mint` may only **attenuate** the source
  (never widen). Pure and host-tested; the kernel composes a growable two-level
  space from these tables, backed by Untyped memory (#55).

- Capability core (#53): a new pure `capability` crate (`#![forbid(unsafe_code)]`,
  host-tested) with the capability algebra - `Rights` bit flags with
  intersection/attenuation, and a `Capability<Obj>` (object handle + rights +
  badge) whose `derive`/`mint` can only weaken rights (never widen) and set a
  badge once. Generic over the object handle so the kernel binds it to real
  objects. Per [ADR 0016](docs/adr/0016-capability-implementation.md).

## [0.2.0] - 2026-10-28

M2: memory & isolation. The kernel takes full control of physical and virtual
memory and enforces hardware hardening - its own frame allocator and page
tables, W^X/NX (including a non-executable physmap), SMEP/SMAP, kernel KASLR,
stack guard pages, zeroed-on-allocation frames, a growable kernel heap, and
legible page-fault diagnostics.

### M2: Memory & isolation

- Documentation (#43): `docs/architecture/memory-management.md` describes the
  whole M2 subsystem (frame allocator, page tables, W^X/NX, KASLR, SMEP/SMAP,
  guard pages, heap, page-fault diagnostics) and its layering; the threat model,
  memory-layout doc, README, and roadmap are updated to match. **M2 complete.**
- Kernel KASLR (#47), per [ADR 0015](docs/adr/0015-kaslr.md): the kernel is now
  built as a position-independent executable (`relocation-model=pic`, linked
  `-pie`, with a `PT_DYNAMIC` segment and `R_X86_64_RELATIVE` relocations), so
  Limine slides it to a random base each boot and applies its relocations.
  `boot/limine.conf` enables `kaslr`; `kmain` logs the slide so backtrace
  addresses map back to the link base for `llvm-addr2line`. Verified in QEMU:
  the kernel base differs across boots and the whole memory subsystem (paging,
  W^X/NX, guard pages, heap, #PF diagnostics) runs correctly at the slid base.
  (HHDM-base randomization is configured but not yet effective with the
  stable-pinned Limine 0.5 protocol revision; the kernel reads the HHDM offset
  dynamically regardless.)
- Page-fault diagnostics (#42), per
  [ADR 0014](docs/adr/0014-page-fault-diagnostics.md): the #PF handler now
  decodes the error code into a plain-language cause (not-present vs protection
  violation, read/write/instruction-fetch, user/supervisor) using a new
  host-tested `PageFaultError` in the `page-table` crate, and names the region
  `CR2` landed in via a `hal::fault` registry (kernel sections, HHDM, heap,
  stack guards - narrowest match wins, so a guard page is reported as such). A
  recoverable `probe_read` (a one-entry exception table) lets code touch a
  possibly-unmapped address without crashing - the handler recognizes the probe
  and resumes at a fixup - which `kmain` uses to verify a stack guard faults and
  recovers while mapped memory reads cleanly. This is the seed of the
  fault-tolerant accessors user-memory access will need (M4).
- SMEP/SMAP, guard pages, and the kernel heap (#41), per
  [ADR 0013](docs/adr/0013-kernel-heap.md):
  - **SMEP/SMAP** are enabled in `CR4` where CPUID reports them, blocking the
    kernel from executing or reading user-accessible pages (safe now - no user
    mappings exist). The QEMU test VM runs `-cpu qemu64,+smep,+smap`.
  - **Stack guard pages**: each HAL-owned stack (the #DF/NMI/#MC IST stacks and
    `rsp0`) gets a page-aligned guard below it that the kernel unmaps, so an
    overflow faults instead of corrupting its neighbor.
  - **Kernel heap**: a hand-rolled, growable first-fit free-list allocator
    (`hal::heap`, zero dependencies) wired to `GlobalAlloc`, so `Box`/`Vec`/
    `String` work. Freeing coalesces adjacent blocks; when exhausted it maps
    more frames (RW+NX) on demand via a kernel-supplied grow hook. Verified in
    `kmain` by a self-test that forces a growth and checks full reclamation.
- W^X + NX enforcement (#40): the kernel now sets and verifies its own memory
  permissions, per [ADR 0012](docs/adr/0012-wx-nx-enforcement.md). `EFER.NXE` is
  enabled; the kernel image is repermissioned per section from linker symbols
  (`.text` R-X, `.rodata` R+NX, `.data`/`.bss` RW+NX) via a new
  `AddressSpace::protect`; and the HHDM (the direct physical map) is made
  non-executable by setting NX on its top-level entries - closing the
  "execute from the physmap alias" primitive. `kmain` applies this and then
  reads the resulting PTE/PML4 flags back to assert the invariants (no W+X,
  `.text` executable, data and HHDM NX). KASLR is tracked separately (#47).
- Page tables & address spaces (#39): the kernel now runs on page tables it
  owns, per [ADR 0011](docs/adr/0011-page-tables.md). A new pure `page-table`
  crate (`#![forbid(unsafe_code)]`, host-unit-tested) encodes entries, flags,
  and the virtual-address split; `hal::paging` adds an `AddressSpace` with
  `map`/`unmap`/`translate`, walking tables through the HHDM and allocating
  intermediate tables from the frame allocator. `AddressSpace::new_kernel`
  allocates a fresh PML4, inherits the bootloader's higher-half mappings
  (kernel image + HHDM), and `switch`es `CR3` to it; the lower half is managed
  at 4 KiB granularity (`translate` also resolves inherited 2 MiB/1 GiB huge
  pages). `kmain` switches to the kernel address space and runs a
  map/write/translate/unmap self-test. Strict per-section kernel permissions
  (W^X/NX) and KASLR come in #40.
- Physical frame allocator (#38): a bitmap allocator (one bit per 4 KiB frame)
  built from the memory map, per [ADR 0010](docs/adr/0010-bitmap-frame-allocator.md).
  The allocation *policy* lives in the new `frame-alloc` crate -
  `#![forbid(unsafe_code)]`, host-unit-tested - which decides which frame to
  hand out over an abstract bitmap. `hal::frame` backs that bitmap with real
  RAM through the HHDM and zeroes every frame on allocation (no stale data
  leaks to a new owner). Frame 0 and the bitmap's own storage are reserved;
  double-free and free-of-untracked are detected as bugs. `kmain` brings the
  allocator up from `BootInfo` and runs an alloc/free smoke test. Single-frame
  only; large-page / DMA-contiguous allocation is deferred (ADR 0010).

## [0.1.0] - 2026-10-28

M1: the kernel takes ownership of the CPU. It installs its own GDT/TSS and IDT,
handles exceptions with diagnostics and backtraces, understands its physical
memory and CPU features, and logs to both serial and an on-screen framebuffer
console.

### Added

- Structured kernel logging (#12): `hal::sync::SpinLock<T>` (minimal spinlock)
  and a `kernel::log` module with severity levels, a runtime level filter, a
  spinlock-guarded serial sink, and `error!`/`warn!`/`info!`/`debug!`/`trace!`
  macros. `kmain` now logs `[INFO]`/`[DEBUG]` records; the panic handler
  deliberately bypasses the logger (writes to its own serial port) to stay
  deadlock-free.
- GDT and TSS (#9): `hal::gdt` installs our own GDT (kernel + user code/data
  descriptors ordered for a future `syscall`/`sysret`) and a 64-bit TSS with
  dedicated IST stacks for #DF, NMI, and #MC plus an `rsp0` stack, then reloads
  the segment registers and the task register. Hand-rolled per
  [ADR 0009](docs/adr/0009-hand-rolled-cpu-tables.md).
- IDT and exception handling (#10): `hal::idt` installs a 256-entry IDT whose
  gates point at generated asm stubs that normalize the error code, build a
  `TrapFrame`, and call a table-driven dispatcher. `register_handler(vector, fn)`
  lets IRQ drivers hook in later (M3); CPU exceptions without a handler print a
  diagnostic report (vector, error code, CR2 for #PF, register dump) and halt,
  except #BP which resumes. Critical faults use the IST stacks from #9. A
  shared `Racy` one-time-init cell was factored out of `gdt`. `kmain` triggers a
  test `#BP` and resumes.
- Panic reporting with backtrace (#11): `hal::backtrace` walks the frame-pointer
  chain (the kernel is now built with `force-frame-pointers=yes`). The panic
  handler prints the message, location, and a backtrace; fatal exceptions add a
  backtrace to their register dump. Addresses resolve with
  `llvm-addr2line -e <kernel> <addr>` (verified against a real call chain).
- Memory map & CPU features (#13): the boot code parses the Limine memory map
  into `BootInfo` (regions copied into a fixed array; usable/total RAM, HHDM
  offset, kernel phys/virt base), and `hal::cpuid` detects the CPU vendor, brand,
  and features (NX, SSE2, SMEP, SMAP, x2APIC, XSAVE, PCID, 1 GiB pages, RDRAND,
  invariant TSC, syscall). `kmain` logs a RAM and CPU summary and asserts NX +
  SSE2 are present. Host unit tests cover the `BootInfo` accounting.
- Framebuffer console (#14): `hal::framebuffer` renders the bundled public-domain
  8x8 bitmap font (`hal::font`) onto the Limine framebuffer - glyphs, newlines,
  tabs, and scrolling - and registers as a second log sink, so kernel output
  appears on screen as well as serial (the path that matters on real hardware
  without a serial port). `BootInfo` carries the framebuffer geometry.

## [0.0.1] - 2026-10-26

M0: the project boots. Repository foundation, toolchain, and a bootable image
that reaches the kernel and prints over serial in QEMU.

### Added

- Project foundation for a security-focused Rust microkernel: README,
  contributing guide, code of conduct, security policy, changelog.
- Development environment guide (Rust bare-metal on macOS/Linux/WSL2).
- Architecture docs: overview (microkernel), security model (capabilities),
  threat model, boot process, memory layout.
- ADRs 0001-0008: Rust, Rust+LLVM toolchain, Limine, microkernel, Cargo+xtask,
  capability security model, `forbid(unsafe)` policy, native tooling runtime.
- Roadmap with milestones M0-M7 and a continuous hardening checklist.
- GitHub issue forms (bug, feature, security/tooling) and PR template.
- Repo config: `rust-toolchain.toml`, `rustfmt.toml`, `.editorconfig`,
  `.gitignore`, `.gitattributes`.
- Cargo workspace skeleton (#1): `kernel`, `hal`, `libs/boot-info`, `xtask`
  crates; `cargo xtask build` cross-compiles the kernel to
  `x86_64-unknown-none`. `hal` owns the only `unsafe` (a `hlt` halt loop); the
  `kernel` binary links with `_start` → `kmain` and a halting panic handler.
- CI pipeline (#8, part of #7): GitHub Actions running rustfmt, clippy
  (`-D warnings`) for the bare-metal and host crates, `cargo xtask build`, the
  `scripts/check-unsafe.sh` policy check, and `cargo deny` / `cargo audit` for
  supply chain. `deny.toml` added. CI badge in the README. The QEMU boot smoke
  test is deferred to when the boot path (#2-#5) lands.
- Higher-half linker script and Limine requests (#2): `kernel/arch/x86_64/linker.ld`
  links the kernel at `0xffffffff80000000` with separate R-X / R / RW segments.
  `kernel/src/boot/` declares the Limine requests (base revision, memory map,
  HHDM, executable address) and the ELF `_start`, which validates the protocol
  and calls `kmain`. `limine` 0.5 dependency (stable-compatible). All
  bootloader/linker `unsafe` is confined to the `boot` module, per the updated
  ADR 0007.
- Bootable ISO (#3): `cargo xtask iso` fetches the pinned Limine binary
  (`v11.x-binary`), assembles an `iso_root` with the kernel and `boot/limine.conf`,
  and builds a hybrid BIOS + UEFI image at `build/kernel.iso` via `xorriso` +
  `limine bios-install`. UEFI boot verified in QEMU (reaches long mode, clean
  handoff to the kernel).
- 16550 UART serial driver (#5): `hal::io` (raw port I/O) and `hal::serial`
  (polled COM1 driver with a loopback self-test, `\n`→`\r\n`, and a
  `core::fmt::Write` impl). `kmain` prints "hello from the kernel" and the panic
  handler reports over serial. First visible end-to-end boot, verified in QEMU
  (UEFI): `hello from the kernel`.
- `cargo xtask run` / `debug` (#6): build the ISO and boot it in QEMU in one
  command. Defaults to UEFI with automatic OVMF discovery (`OVMF` env override);
  `--bios` and `--kvm` flags; extra args pass through after `--`. `debug` boots
  paused with a GDB stub on `:1234`.
- CI boot smoke test (#7, completes #7): `scripts/boot-test.sh` boots the ISO
  headless in QEMU and asserts the kernel prints its serial marker; a CI matrix
  runs it for both BIOS and UEFI (BIOS boot is now covered on Linux runners).
  Serial logs are uploaded as artifacts. **M0 complete.**

[0.3.0]: https://github.com/seedon198/magnetite/releases/tag/v0.3.0
[0.2.0]: https://github.com/seedon198/magnetite/releases/tag/v0.2.0
[0.1.0]: https://github.com/seedon198/magnetite/releases/tag/v0.1.0
[0.0.1]: https://github.com/seedon198/magnetite/releases/tag/v0.0.1
