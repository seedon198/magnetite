# 0017: Threads, context switch, and scheduling

- **Status:** Accepted; decision 1's timing amended by [0019](0019-first-user-task.md)
- **Date:** 2026-10-28

## Context

M4 brings the capability model to life: runnable threads, a scheduler, ring 3,
and (on top of threads) blocking IPC and the `syscall` entry. This ADR records
the thread/scheduler shape and the build order.

## Decisions

**1. A `Thread` is a capability object (TCB).** The `Thread` kind reserved in
M3 becomes a Thread Control Block retyped from Untyped (no kernel heap), holding:
the saved register context, its kernel stack pointer, a reference to its
capability space (root CNode) and address space (PML4), and scheduling state
(run-queue links, state: Running/Ready/Blocked). Creating a thread is a
capability operation, like every other object.

**2. Context switch in `hal`, run queue pure.** The register save/restore and
stack switch are a small assembly routine in `hal` (save callee-saved registers
+ `rsp` into the current TCB, load the next). The *scheduler* - the round-robin
run queue and the pick-next decision - is pure logic over thread ids, so it is
host-testable; the kernel wires it to the `hal` switch.

**3. Round-robin to start.** A single run queue; each ready thread gets a time
slice in turn. Simple and predictable. Priorities / a richer policy are a later
refinement (a new ADR) when servers need them.

**4. Cooperative first, then preemptive.** First land threads + context switch
with a cooperative (`yield`-based) scheduler and verify two kernel threads
switching. Then add ring 3 + `syscall`, then the **LAPIC timer** for preemption,
then blocking `Endpoint` IPC, then the first user task, then FPU/SSE (XSAVE).
Each step is a small, bootable, QEMU-verified PR.

**5. Ring 3 via `syscall`/`sysret`.** The user→kernel boundary uses the
`syscall`/`sysret` fast path (the GDT was already ordered for it in M1); the
handler decodes registers into a capability [`Method`] and calls the M3
invocation path. SMEP/SMAP (M2) are active across the boundary; deliberate
user-memory access uses the recoverable-probe/fixup mechanism (M2 #42) extended
to `stac`/`clac` windows.

## Consequences

- Threads are capability objects, so thread creation and management are
  capability-gated like everything else; the TCB lives in Untyped memory.
- The scheduler policy is isolated and host-testable; swapping round-robin for
  priorities later touches only that pure module.
- The incremental order keeps `main` bootable at every step and each PR small.
- Blocking IPC (`Endpoint` send/receive/call) and the `syscall` entry, deferred
  from M3 (ADR 0016), are built here on top of threads.
- Preemption introduces interrupts arriving while the kernel runs; the
  interrupt-safe `SpinLock` (#62) and the lock-ordering discipline become
  load-bearing here.
