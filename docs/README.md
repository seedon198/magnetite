# Documentation

| Document | Description |
|---|---|
| [Development environment](development-environment.md) | macOS / Linux / WSL2 setup, running, debugging |
| [Roadmap](roadmap.md) | Milestones and the continuous hardening checklist |
| [Coding style](coding-style.md) | Rust conventions and the `unsafe` rule |
| **Architecture** | |
| [Overview](architecture/overview.md) | Microkernel model and how the pieces fit |
| [Security model](architecture/security-model.md) | Capabilities and least privilege |
| [Threat model](architecture/threat-model.md) | What we harden against, and the ethical boundary |
| [Boot process](architecture/boot.md) | From power-on to the root server |
| [Interrupts & exceptions](architecture/interrupts.md) | IDT, trap frames, dispatch |
| [Memory layout](architecture/memory-layout.md) | Address-space map and isolation |
| [System calls](architecture/syscalls.md) | The syscall ABI, kernel methods, the root task |
| **Decisions** | |
| [ADR index](adr/README.md) | Architecture Decision Records |

## Documentation rules

1. **Docs live next to code, in this repository**, and change in the same PR
   as the code they describe.
2. Any significant or hard-to-reverse decision gets an **ADR**
   ([template](adr/0000-template.md)) - and is discussed with the maintainer
   before being accepted.
3. Each subsystem gets an `architecture/` page once it exists.
4. Cite the spec. When code implements hardware behaviour, reference the manual
   (e.g. *Intel SDM Vol. 3A §6.14*).
5. Security-relevant changes update the [threat model](architecture/threat-model.md).
