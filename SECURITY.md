<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brand/banner-dark.svg">
  <img src="docs/assets/brand/banner-light.svg" alt="Magnetite" width="420">
</picture>

**A hardened, capability-based x86-64 microkernel, written from scratch in Rust.**

</div>

---

# Security Policy

This is a hobby/educational security-focused kernel in early development. It is
**not production software** and must only be used for **authorized** security
testing, research and education - against systems you own or have explicit
written permission to test. See the
[threat model](docs/architecture/threat-model.md) for the full ethical and
technical boundary.

## Reporting a vulnerability in Magnetite

Please **do not** open a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/seedon198/magnetite/security/advisories/new).

Include a description, the affected commit, and reproduction steps (ideally a
QEMU command line). Expect an acknowledgement within a week.

## Acceptable use

The offensive capabilities this project builds (raw packet access, scanning,
fuzzing, a scripting runtime) are dual-use. Do not use them against systems you
are not authorized to test. The project will not accept contributions whose
purpose is to harm non-consenting third parties (self-spreading malware,
botnet C2, destructive payloads, indiscriminate mass exploitation).

## Supported versions

Only the latest commit on `main` is supported.
