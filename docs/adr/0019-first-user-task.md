# 0019: The first user task: loading, confinement and the syscall ABI

- **Status:** Proposed
- **Date:** 2026-11-01

## Context

Issue #82 closes out M4's user-space half: a task in ring 3, confined to
exactly the capabilities it was given, reaching the kernel only through
syscalls and IPC. What exists, and limits this design:

- A ring-3 round trip works (#79), but only for a 6-byte hand-assembled stub
  mapped into the **kernel's own** address space, and the syscall handler is a
  two-case demo that invokes no capability.
- Every entry to the kernel from ring 3 (syscall, interrupt, fault) lands on
  **one shared kernel stack** (`TSS.rsp0`, and the syscall stub's own stack).
  So the timer does not preempt ring 3, and no user thread can block in a
  syscall.
- Any CPU exception, including one raised by user code, is handled as a kernel
  fault. A user task could crash the kernel.
- Synchronous IPC works between kernel threads (#81, ADR 0018). Its engine is
  ready for user threads, and the kernel wrappers take capability slots in the
  current thread's capability space.
- `hal::paging::AddressSpace` builds only the kernel's own address space
  (`new_kernel`). Threads are a fixed array of contexts in `kmain`, not
  `Thread` objects.

The decisions below were made by the implementer at the maintainer's request
("you plan"), and are recorded here for the maintainer to accept or change.

## Decision

### 1. The first task is a real Rust program, loaded as a boot module

A new crate `user/root` (`no_std`, `#![forbid(unsafe_code)]` except its tiny
syscall stubs, which live in a small `libs/sys` crate with audited `asm!`)
builds as a static, non-PIE ELF for `x86_64-unknown-none`, linked at
`0x0040_0000`. `cargo xtask build` builds it before the ISO, and Limine loads
it as a **boot module** (`module_path` in `boot/limine.conf`). The kernel asks
for it with Limine's module request and copies its location into `BootInfo`.

The kernel loads it with a new pure crate `libs/elf`
(`#![forbid(unsafe_code)]`, host-tested, with adversarial inputs): it validates
the ELF64 header and every `PT_LOAD` segment (bounds, overlap, alignment, user
half only, no segment both writable and executable) before anything is mapped.
A malformed image is refused, and the kernel boots on without it.

*Rejected:* hand-assembled bytes (cannot show a real program running) and
embedding the binary in the kernel image (couples two builds and leaves no path
to loading other servers in M5).

### 2. Every task gets its own address space

`hal::paging` gains `AddressSpace::new_user`: a fresh PML4 that shares the
kernel half (entries 256 to 511, copied from the kernel's PML4) and has an empty
user half. The loader maps each segment's pages with exactly its ELF
permissions (`R`, `RW+NX` or `R-X`, never `W+X`), plus a user stack with a guard
page below it. Frames come from the frame allocator and are zeroed. A context
switch loads the next thread's address space when it differs.

### 3. Threads stay a kernel table for now (amends ADR 0017)

*Decided after the builder review.* ADR 0017 (decision 1) and ADR 0018 expected
#82 to make each thread a control block retyped from Untyped. #82 does not: the
kernel keeps an internal thread table (context, kernel stack, address space,
capability space, IPC state), and the root task's thread, kernel stack and page
tables come from the kernel's own boot-time memory, the way seL4 creates its
initial task before handing out Untyped. "Retyped from Untyped, never the kernel
heap" then applies from the moment a *task* creates a thread, which arrives with
`Thread` capabilities in M5. ADR 0017's decision 1 is amended to that timing.

### 4. Per-thread kernel stacks, and preemption of ring 3

Each thread gets its own kernel stack, with a guard page. On every switch the
kernel points `TSS.rsp0` and the syscall entry stub's stack at the next thread's
kernel stack. With that, the timer may preempt ring 3 (the `cs & 3` early
return goes), and a user thread may block inside a syscall, which IPC needs.

### 5. A user fault ends the task, never the kernel

An exception raised in ring 3 (page fault, `#GP`, `#UD`, divide error, ...)
logs one line (vector, faulting address and `rip`) and terminates that thread:
it leaves the run queue, its IPC state is cleared (a caller waiting in its reply
slot is woken with an error), and the kernel carries on. Exceptions raised in
ring 0 keep today's fatal report.

### 6. The syscall ABI

`syscall` with:

| Register | In | Out |
|---|---|---|
| `rax` | syscall number | error code (0 = success) |
| `rdi` | capability slot | badge (on receive) |
| `rsi` | `MessageInfo` | `MessageInfo` |
| `rdx`, `r8`, `r9`, `r10` | message registers 0 to 3 | message registers 0 to 3 |
| `r12` | receive slot, or `u64::MAX` for none | unchanged |
| `r13` | capability slot to send, if `MessageInfo::caps()` is 1 | unchanged |

Syscall numbers: `Send = 1`, `Recv = 2`, `Call = 3`, `Reply = 4`,
`ReplyRecv = 5`, `Yield = 6`, `Exit = 7`. Errors are small positive numbers,
one per `IpcError` and `InvokeError` variant, plus `InvalidCap` and
`InvalidSyscall`. The ABI lives in `capability::abi`, shared by the kernel and
`libs/sys`.

`Call` on an **Endpoint** capability is IPC (ADR 0018). `Call` on any **other**
capability invokes a kernel object method: the label names the method and the
message registers carry its arguments, decoded into the existing rights-checked
`Method` (ADR 0016). So "talk to the kernel only via syscalls/IPC" holds with no
separate per-object syscall.

**No physical address ever reaches ring 3** (decided after the builder review).
Two kernel methods return one today: retype (`InvokeOk::Created` carries the
object's physical address) and `FrameIdentify`. Through the syscall, a retype
returns only the destination slot, and identifying a frame returns only its
kind and size. Physical addresses help attacks such as rowhammer and are of no
use to a task that cannot map memory.

**Slot 0 is a kernel-enforced null slot** (decided after the builder review).
The syscall layer refuses slot 0 wherever a syscall names a slot: the invoked
capability, a receive slot, the capability to send, and a method's source or
destination slot, all with `InvalidCap`. Kernel-internal code is not limited;
the rule is about what tasks can name. So "slot 0 is empty" holds for the life
of every task, and user code may treat it as null.

#81's deferred items land here: a `ReplyRecv` kernel wrapper, an `InvalidCap`
error (an empty or wrong-kind capability slot) instead of reusing
`MissingRight`, and the HHDM offset set during init rather than by a demo.

### 7. The root task's capabilities

The kernel creates the root task's capability space (a CNode of 64 slots) and
fills fixed slots, which the task knows by convention (`libs/sys` names them):

| Slot | Capability | Rights |
|---|---|---|
| 0 | empty, always: the kernel refuses it (decision 6) | |
| 1 | its own CNode | `READ`, `WRITE` |
| 2 | an Untyped of 64 KiB | `RETYPE` |
| 3 | the console endpoint, badged | `SEND` only |

Nothing else. In particular it gets no capability to its own address space,
its thread or any frame, so it cannot map memory or start threads yet. That is
deliberate for #82, and those come with M5's servers.

### 8. Output goes through IPC to a console server

The kernel cannot print on a task's behalf without becoming a capability-less
side channel. Instead a **console server** kernel thread receives on an
endpoint and writes each message to the log (label = byte count, message
registers = up to 32 bytes of UTF-8). The root task holds a badged, send-only
capability to it, so the first thing a user program does is IPC across the ring
boundary, and the server sees which task wrote by its badge.

### 9. What the root task does (the proof)

In order, each logged through the console:

1. Prints a greeting and the slot layout it was started with. It knows the
   layout from `libs/sys`'s names, not from a kernel query; there is no
   "describe my slots" method (decided after the builder review), and steps 2
   to 4 show the layout holds.
2. Retypes a Notification out of its Untyped, into slot 4 of its own CNode, and
   signals it (an object-method `Call` that works).
3. Tries to invoke the null slot 0, an empty slot, and a slot past its CNode,
   and gets `InvalidCap` each time.
4. Tries to send a capability through the console endpoint (no `GRANT`) and
   gets `NoGrant`.
5. Exits with `Exit`, cleanly.

A second, tiny module, `user/fault`, then reads a kernel address. The kernel
logs the page fault, ends that task, and boots on to `ready:`. That shows
confinement from both sides: refused capabilities, and memory it does not own.

### 10. Delivery in two PRs

1. **Mechanism:** per-thread kernel stacks, `TSS.rsp0` switching, ring-3
   preemption, user address spaces, user-fault containment. Verified with the
   existing stub moved into its own address space.
2. **The task:** `libs/elf`, `libs/sys`, `user/root`, `user/fault`, the boot
   module, the syscall ABI, the console server and the bootstrap.

## Consequences

- The kernel can load and run real, separately built programs. M5 servers
  follow the same path.
- A user bug or attack ends that task, never the kernel. The ELF loader is the
  first parser of untrusted input in the kernel, so it is pure, host-tested and
  checks everything before mapping.
- The syscall ABI becomes a compatibility surface. Changing it later means
  changing `libs/sys` and every user program at once (cheap while there is
  one).
- `Thread` and `AddressSpace` capabilities, and so user-created threads and
  user-controlled mapping, are still deferred to M5. The root task can compute
  and talk, but not grow.
- Revisit: the fixed slot layout (seL4 passes a `BootInfo` page instead), the
  console protocol (a buffer page once long messages exist), and SMP (one kernel
  stack per thread is already per-CPU-safe; `TSS.rsp0` becomes per-CPU).
