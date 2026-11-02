# Development Environment

You do **not** need an x86 machine to work on this kernel. Rust cross-compiles
to bare-metal x86-64 from any host, and QEMU runs the result. Real hardware is
only needed for occasional validation and for USB-boot testing.

## Contents

- [How it works](#how-it-works)
- [macOS (Apple Silicon and Intel)](#macos-apple-silicon-and-intel)
- [Linux](#linux)
- [Windows (WSL2)](#windows-wsl2)
- [Debugging](#debugging)
- [Testing on real hardware](#testing-on-real-hardware)
- [Troubleshooting](#troubleshooting)

## How it works

<p align="center">
  <img src="assets/dev-toolchain.svg" alt="Toolchain: cargo builds the kernel ELF, cargo xtask iso makes kernel.iso with Limine and xorriso, cargo xtask run boots it in QEMU with serial output and a GDB stub" width="820">
</p>

- **Rust is a cross-compiler by default.** `rustup target add x86_64-unknown-none`
  installs a bare-metal (`no_std`, no OS) target. One `cargo build
  --target x86_64-unknown-none` on an Apple Silicon Mac produces an x86-64 ELF.
  Verified: `rustc` 1.94 builds and links a `no_std` x86-64 kernel on arm64 macOS.
- **No C cross-toolchain is required** for the kernel itself. If we later need a
  custom target spec (e.g. to disable the red zone or SSE), that uses
  `-Zbuild-std` on a **nightly** toolchain, pinned in `rust-toolchain.toml`.
- **QEMU** emulates a full x86-64 PC. On Apple Silicon it uses software
  emulation (TCG) - slower than native but deterministic and easy to debug.

## macOS (Apple Silicon and Intel)

```sh
# Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add x86_64-unknown-none
rustup component add rust-src llvm-tools clippy rustfmt

# Emulator + ISO tooling + a debugger
brew install qemu xorriso x86_64-elf-gdb
```

| Tool | Purpose |
|---|---|
| `rustup` / `cargo` / `rustc` | the Rust toolchain and cross-compilation |
| `rust-src` | required for `-Zbuild-std` if we use a custom target |
| `llvm-tools` | `llvm-objdump`, `llvm-nm`, `llvm-size` via `cargo` |
| `qemu` | `qemu-system-x86_64` |
| `xorriso` | builds the bootable ISO |
| `x86_64-elf-gdb` | source-level kernel debugging (`lldb` also works) |

The exact toolchain (a specific stable release today, or a pinned nightly if
ever needed) is set by `rust-toolchain.toml` in the repo, so `cargo` and
`rustup` select and install it automatically.

## Linux

Debian / Ubuntu:

```sh
sudo apt install qemu-system-x86 xorriso gdb
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add x86_64-unknown-none
rustup component add rust-src llvm-tools clippy rustfmt
```

On an x86-64 Linux host QEMU can use KVM for near-native speed
(`cargo xtask run --kvm`).

## Windows (WSL2)

Install Ubuntu under WSL2 and follow the Linux steps. QEMU's display works via
WSLg; serial output works everywhere.

## Running

`cargo xtask run` builds the ISO and boots it in QEMU, with the kernel's serial
output on your terminal. It defaults to **UEFI** (auto-locating OVMF firmware;
set `OVMF=/path/to/edk2-x86_64-code.fd` to override). Flags:

```sh
cargo xtask run                 # UEFI boot (recommended)
cargo xtask run --bios          # SeaBIOS boot (flaky on QEMU/macOS; fine on Linux)
cargo xtask run --kvm           # enable KVM (Linux x86-64 hosts)
cargo xtask run -- -d int,cpu_reset   # pass extra flags through to QEMU
```

## Debugging

`cargo xtask debug` boots QEMU paused with a GDB remote stub on `:1234`:

```sh
cargo xtask debug          # boots QEMU paused, listening on :1234
# another terminal:
x86_64-elf-gdb target/x86_64-unknown-none/debug/kernel -ex "target remote :1234"
# or
lldb target/x86_64-unknown-none/debug/kernel -o "gdb-remote 1234"
```

Useful QEMU options (passthrough after `--`): `-d int,cpu_reset` (trace
interrupts, stop on triple fault), `-monitor stdio`.

## Testing on real hardware

Write the ISO to a USB stick and boot a generic x86-64 PC from it. Watch the
serial port (USB-serial adapter) or the framebuffer.

```sh
# macOS - verify the disk number with `diskutil list` first!
sudo dd if=build/kernel.iso of=/dev/rdiskN bs=4m
```

> [!CAUTION]
> `dd` to the wrong disk destroys data. Double-check the device name.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `error[E0463]: can't find crate for core` | missing `rustup target add x86_64-unknown-none` |
| `-Zbuild-std` errors | not on the pinned nightly, or `rust-src` not installed |
| QEMU reboots in a loop | triple fault - rerun with `-d int,cpu_reset -no-reboot` |
| Black screen, no serial | check `limine.conf` path and that the kernel is on the ISO |
| `unsafe` won't compile in `kernel` | intended - `unsafe` belongs in `hal` (see ADR 0007) |
