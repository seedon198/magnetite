# Memory Layout

> Status: **implemented in milestone M2.** See
> [memory-management.md](memory-management.md) for how each region is set up and
> protected.

x86-64 with 4-level paging gives a 48-bit virtual address space in two
canonical halves: user space in the lower half, the kernel in the upper half.
Because this is a **microkernel**, most "system" code (drivers, net, fs) runs
in ordinary **user** address spaces, not in the kernel half.

## Virtual address map (kernel view)

<p align="center">
  <img src="../assets/memory-layout.svg" alt="Virtual address space: kernel image with W^X sections in the KASLR window, kernel heap, LAPIC page, HHDM, non-canonical hole, user space, unmapped NULL page" width="820">
</p>

| Start | End | Size | Contents |
|---|---|---|---|
| `0x0000000000000000` | `0x0000000000000fff` | 4 KiB | Unmapped - catches NULL dereferences |
| `0x0000000000001000` | `0x00007fffffffffff` | ~128 TiB | **User space** (per task: a server or tool) |
| `0x0000800000000000` | `0xffff7fffffffffff` | - | Non-canonical hole (#GP on access) |
| `0xffff800000000000` + random | HHDM + RAM | ≤ 64 TiB | **HHDM** - direct map of physical memory; Limine randomizes its base in 1 GiB steps within the first 32 TiB (#70) |
| `0xfffffd0000000000` | grows up | 20 KiB per thread | **Kernel stacks** - per thread: an unmapped guard page, then 16 KiB RW+NX (PML4 slot 506, #82) |
| `0xfffffe0000000000` | `0xfffffe0000000fff` | 4 KiB | **LAPIC registers** - xAPIC MMIO page, uncached RW+NX (PML4 slot 508, #80) |
| `0xffffff8000000000` | `0xffffffff7fffffff` | ~510 GiB | **Kernel heap** - grows up from the base, frames mapped on demand (RW+NX) |
| `0xffffffff80000000` | `0xffffffffffffffff` | 2 GiB | **Kernel image** - the KASLR window; Limine places the image anywhere inside it |

The kernel image sits in the top 2 GiB so code uses `-mcmodel=kernel`-style
sign-extended 32-bit addressing. **KASLR** randomizes its base at boot (the
kernel is a PIE that Limine slides; see
[ADR 0015](../adr/0015-kaslr.md)), so the table shows the window, not a fixed
address. The kernel heap cannot collide with the slid image: it starts at
`HEAP_BASE` (`0xffffff8000000000`) and `heap_grow` refuses to grow past
`HEAP_LIMIT` (`0xffffffff80000000`), the bottom of that window (#61).

## Kernel image sections and W^X

| Section | Permissions |
|---|---|
| `.text` | R-X |
| `.rodata` | R-- (NX) |
| RELRO (`.data.rel.ro`, `.dynamic`, `.got`) | R-- (NX), once Limine has relocated it (#126) |
| `.data`, `.bss` | RW- (NX) |

Sections are page-aligned in the linker script and mapped with exactly these
permissions once the kernel installs its own page tables. **No page is ever
both writable and executable** (W^X), enforced for kernel and user mappings
alike.

## User address spaces (#82)

Each user task has its own PML4 ([ADR 0019](../adr/0019-first-user-task.md)).
Its kernel half (entries 256 to 511) is copied from the kernel's, and every one
of those entries is populated once at boot, so anything the kernel maps later
lands in tables all spaces share. The kernel half is supervisor-only: a user
access faults and ends the task.

A user program is a static ELF executable linked at `0x400000` (`user/link.ld`).
The kernel validates it (`libs/elf`) and maps each segment's pages with exactly
its permissions: text read and execute, read-only data read only, data and bss
read and write, never execute. Its stack is four pages, read and write, never
execute, ending at `0x7fff_ffff_0000`, with the page below left unmapped as a
guard; segments must end below that guard. No page is both writable and
executable, and the top user page (`USER_TOP`) is never mappable. When a task
ends, every frame of its user half, its page tables and its kernel stack are
freed (`AddressSpace::destroy_user`, `thread::reap`).

## Isolation

Each user task (server or tool) has its **own page table root** (an
`AddressSpace` capability). Tasks share nothing by default; shared memory
exists only where one task explicitly maps a `Frame` capability it received
over IPC. This is the memory-level expression of the
[capability security model](security-model.md).
