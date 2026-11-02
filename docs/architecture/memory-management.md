# Memory Management

> Status: **implemented in milestone M2.** This describes the kernel's physical
> and virtual memory subsystem as it stands; see [memory-layout.md](memory-layout.md)
> for the address map and the [ADRs](../adr/) for the rationale behind each
> choice.

The kernel owns its memory from the ground up: it allocates physical frames,
builds and switches to its own page tables, enforces W^X/NX in hardware,
randomizes its own load address, runs a growable heap, and renders page faults
legibly. Every piece follows one structural rule - **pure, host-tested policy in
a `libs/` crate; the `unsafe` hardware binding in `hal`** - so the parts that can
be unit-tested on the host are, and the `unsafe` stays confined to `hal` (and the
boot module), CI-enforced (ADR 0007).

## 1. Physical frame allocator - `frame-alloc` + `hal::frame`

A **bitmap** allocator (one bit per 4 KiB frame) built from the Limine memory
map ([ADR 0010](../adr/0010-bitmap-frame-allocator.md)).

- The *policy* (which frame to hand out, bookkeeping, double-free detection)
  lives in the pure, `forbid(unsafe)`, host-tested `frame-alloc` crate.
- `hal::frame` backs the bitmap with real RAM through the HHDM and **zeroes every
  frame on allocation**, so a new owner never reads a previous owner's data.
- Frame 0 and the bitmap's own storage are reserved. A second bitmap records
  the **pool** (usable RAM minus reserved ranges), and `free` accepts only pool
  frames: a double free, or a free of frame 0, the bitmap, a reserved range or
  a hole such as MMIO (`FreeError::NotPoolFrame`, #122), is treated as a
  kernel bug instead of quietly adding the frame to the pool.
- Single frames, plus `alloc_contiguous(n)`: `n` (a power of two) physically
  contiguous frames aligned to `n`, which backs the root task's Untyped
  region. Large-page and DMA-specific allocation is deferred until a consumer
  needs it.

## 2. Address spaces & page tables - `page-table` + `hal::paging`

The kernel runs on 4-level page tables it owns
([ADR 0011](../adr/0011-page-tables.md)).

- `page-table` (pure, host-tested) encodes entries, flags, and the
  virtual-address → `(PML4, PDPT, PD, PT, offset)` split.
- `hal::paging::AddressSpace` provides `map` / `unmap` / `translate` /
  `protect`, walking tables **through the HHDM** and allocating intermediate
  tables from the frame allocator.
- The kernel address space is built by `new_kernel`, which allocates a fresh
  PML4, **inherits the bootloader's higher-half entries** (kernel image + HHDM),
  and switches `CR3` to it. The lower half is managed at 4 KiB granularity;
  `translate` also resolves inherited 2 MiB / 1 GiB huge pages.
- Each user task has its own PML4 (`AddressSpace::new_user`, #82): a private
  lower half and the kernel half's top-level entries copied in, supervisor-only.
  Tasks share nothing by default (the memory-level expression of the
  [capability model](security-model.md)).

## 3. W^X + NX - `hal::paging` + `kmain`

The kernel sets **and verifies** its own permissions
([ADR 0012](../adr/0012-wx-nx-enforcement.md)).

| Region | Permissions |
|---|---|
| `.text` | R-X |
| `.rodata` | R + NX |
| RELRO: `.data.rel.ro`, `.dynamic`, `.got` (#126) | R + NX, after Limine relocates |
| `.data` / `.bss` | RW + NX |
| HHDM (direct physical map) | RW + NX |
| HHDM alias of `.text` / `.rodata` frames | R + NX |

- `EFER.NXE` is enabled so the NX bit is honored.
- Section bounds come from page-aligned **linker symbols**; each page is
  re-permissioned in place with `AddressSpace::protect`.
- The HHDM is made non-executable by setting NX on its top-level entries (NX is
  cumulative across paging levels), closing the *execute-from-physmap-alias*
  primitive.
- The HHDM maps the kernel image's frames a second time. For `.text` and
  `.rodata`, that alias is made **read-only** (`AddressSpace::split_huge`
  breaks the covering 1 GiB / 2 MiB pages down to 4 KiB, then
  `make_read_only`), so a write primitive cannot patch code or read-only
  tables through the HHDM and run the result at the normal address (#119).
  `.data` is writable through both mappings, as it is meant to be.
- **No mapping is ever both writable and executable**, and no writable
  mapping reaches kernel code or read-only data. `kmain` reads the resulting
  PTE/PML4 flags back, including every HHDM alias page, and asserts the
  invariants.

## 4. KASLR - PIE + Limine

The kernel is a position-independent executable that Limine slides to a random
base each boot ([ADR 0015](../adr/0015-kaslr.md)).

- Built `relocation-model=pic`, linked `-pie`, with a `PT_DYNAMIC` segment and
  `R_X86_64_RELATIVE` relocations; Limine applies them before handoff
  (`kaslr: yes` in `boot/limine.conf`).
- `kmain` logs the **slide** (`kernel_virt_base − 0xffffffff80000000`) so a
  backtrace address can be mapped back to the link base for `llvm-addr2line`.
- W^X and the rest compose for free: permissions key off relocating linker
  symbols, and the HHDM is always read dynamically from `boot_info.hhdm_offset`.
- *Known limitation:* HHDM-base randomization is configured but not yet
  effective with the stable-pinned `limine` 0.5 protocol revision.

## 5. SMEP / SMAP - `hal::cpu`

`CR4.SMEP`/`CR4.SMAP` are enabled where CPUID reports support
([ADR 0013](../adr/0013-kernel-heap.md)). SMEP stops the kernel executing
user-accessible pages; SMAP stops it reading them without an explicit
`stac`/`clac` window. Safe to enable now (no user mappings exist); deliberate
user-memory access in M4 will bracket with `stac`/`clac`.

## 6. Stack guard pages - `hal::gdt` + `kmain`

Each HAL-owned stack (the #DF / NMI / #MC IST stacks and `rsp0`) is laid out with
a page-aligned **guard page** below it, which the kernel unmaps. A stack
overflow runs off the bottom into the unmapped guard and faults, instead of
silently corrupting the neighboring stack ([ADR 0013](../adr/0013-kernel-heap.md)).

## 7. Kernel heap - `hal::heap`

A hand-rolled, **growable first-fit free-list allocator** with coalescing, wired
to `GlobalAlloc`, so `Box` / `Vec` / `String` work in the kernel
([ADR 0013](../adr/0013-kernel-heap.md)).

- Free blocks are threaded through the heap memory (address-sorted); freeing
  **coalesces** adjacent blocks. An allocation header before each block makes
  `dealloc` alignment-independent.
- When no block fits, a kernel-supplied **grow hook** maps more frames (RW+NX)
  contiguously at the next heap address and coalesces them in - no fixed ceiling.
  If frames run out part-way, the grow undoes the pages it mapped, so the heap
  can retry rather than wedge (#63).
- A block is **zeroed when freed**, and a list header absorbed by a merge is
  cleared, so no allocation sees an earlier one's data (#69).
- Every size sum is checked: a `Layout` too large to meet gets null, never an
  overflow panic or a wrapped, too-small block (#121).
- An infallible allocation that fails panics with the failed size, through the
  panic handler and its backtrace (stable `no_std` behaviour, #68).
- The heap lives in its own window, `HEAP_BASE` (`0xffffff8000000000`) up to
  `HEAP_LIMIT` (`0xffffffff80000000`), the bottom of the top-2 GiB KASLR
  window, and never grows past it, so it cannot collide with the slid kernel
  image (#61).

## 8. Page-fault diagnostics - `page-table` + `hal::fault` + `hal::idt`

A #PF produces a legible report ([ADR 0014](../adr/0014-page-fault-diagnostics.md)):

- the **decoded error code** (not-present vs protection violation, read / write /
  instruction-fetch, user / supervisor), via a host-tested `PageFaultError`;
- the **region** `CR2` landed in (kernel sections, HHDM, heap, stack guard, …)
  via a `hal::fault` registry - the *narrowest* match wins, so a guard-page hit
  is named as such;
- the register dump and a frame-pointer backtrace.

`hal::fault::probe_read` is a **recoverable probe**: a byte read whose faulting
instruction the handler recognizes and resumes past, instead of halting. It
proves a mapping is absent without crashing, and is the seed of the
fault-tolerant accessors user-memory access needs (M4).

## Layering & testing summary

| Concern | Pure crate (host-tested) | `hal` binding (`unsafe`) |
|---|---|---|
| Physical frames | `frame-alloc` | `hal::frame` |
| Page tables | `page-table` | `hal::paging` |
| #PF decode | `page-table` (`PageFaultError`) | `hal::idt` / `hal::fault` |
| Kernel heap | - (bookkeeping lives in the managed memory) | `hal::heap` |

All of M2 is verified end to end by booting in QEMU (UEFI + BIOS) and checking
the serial log; the pure crates add host unit tests.

## Known limitations / future work

- Large-page and DMA-specific physical allocation (deferred from #38;
  `alloc_contiguous` covers aligned power-of-two runs).
- `unmap` frees the intermediate tables it empties (#64), but never the root
  or a kernel-half PDPT (shared by every user space).
- HHDM-base randomization pending a newer Limine protocol revision.
- A negative W^X/NX fault test (deliberately faulting) builds on the recoverable
  probe and the #PF handler; the probe primitive exists, broader fault-injection
  testing is future work.
