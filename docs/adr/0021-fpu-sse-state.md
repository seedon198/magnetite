# 0021: FPU and SSE state: enabled for every thread, saved eagerly with fxsave

- **Status:** Accepted
- **Date:** 2026-11-02

## Context

Issue #83 (M4): threads that use SSE registers must not see or corrupt each
other's values across context switches. What exists, and limits this design:

- The kernel is built for `x86_64-unknown-none`, which is soft-float: the
  compiler emits no x87 or SSE instructions in the kernel. User programs use
  the same target today, but a task may use SSE in its own code (inline asm,
  or another target later).
- Until now nothing enabled SSE: `CR4.OSFXSR` was clear, so an SSE instruction
  in ring 3 raised `#UD`.
- QEMU's default CPU model (`qemu64`) reports no XSAVE; real hardware does.
- Every switch, cooperative or preemptive, goes through `thread::switch_to`,
  single core, with interrupts off.

## Options considered

1. **Eager save/restore with `fxsave`/`fxrstor`** at every switch: 512 bytes
   each way, simple, no extra trap path.
2. **Lazy switching** (`CR0.TS` plus a `#NM` handler that swaps state on first
   use): fewer saves when few threads use SSE, but an extra exception path in
   the kernel, and lazy FPU state is a known side channel (CVE-2018-3665).
3. **`xsave`/`xrstor`** with a CPUID-sized area: needed for AVX and wider
   state, but unavailable on the default QEMU CPU, and buys nothing while only
   x87 and SSE are enabled.

## Decision

Option 1. `hal::fpu::init` sets `CR0.MP|NE`, clears `CR0.EM|TS`, sets
`CR4.OSFXSR|OSXMMEXCPT`, and captures a clean default image (`fninit`, MXCSR
`0x1F80`). Each thread has an `FpuArea`; `switch_to` saves the outgoing
thread's registers and restores the incoming thread's, the default image for a
thread that has never run. Reaping a thread resets its area, so a reused slot
never sees its predecessor's registers. The kernel stays soft-float, so
interrupt and syscall entry need not save anything.

## Consequences

- Threads may use SSE freely; a boot check runs two ring-3 threads that keep
  their own `xmm0` across switches (without the save and restore, one reads
  the other's value).
- Every switch costs an `fxsave` and an `fxrstor`, a few hundred cycles; not
  measurable against today's switch rate.
- AVX and wider state: switch `FpuArea` to `xsave` with a CPUID-sized area and
  enable the matching XCR0 bits when a task needs them. That is the extension
  point, not a redesign.
- Eager switching avoids the lazy-FPU state leak by construction.
