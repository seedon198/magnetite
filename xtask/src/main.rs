#![forbid(unsafe_code)]

//! Workspace task runner.
//!
//! Invoked as `cargo xtask <command>` (see `.cargo/config.toml`). Runs on the
//! host and orchestrates cross-compiling the kernel, fetching Limine,
//! assembling a bootable ISO, and launching it in QEMU.
//!
//! Commands:
//! - `test`                     run the host workspace tests
//! - `build`                    cross-compile the kernel
//! - `iso`                      build `build/kernel.iso`
//! - `run [--bios|--kvm] [-- <qemu args>]`    build the ISO and boot in QEMU
//! - `debug [--bios|--kvm] [-- <qemu args>]`  boot paused with a GDB stub

use std::path::{Path, PathBuf};
use std::process::{exit, Command};

/// Bare-metal target the kernel is compiled for (ADR 0002).
const KERNEL_TARGET: &str = "x86_64-unknown-none";

/// Pinned Limine release. The binary distribution is published on per-major
/// branches; `v11.x-binary` is the latest prebuilt branch (v12 is source-only).
/// See docs/adr/0003-bootloader.md.
const LIMINE_REPO: &str = "https://github.com/limine-bootloader/limine.git";
const LIMINE_BRANCH: &str = "v11.x-binary";

fn main() {
    let mut args = std::env::args().skip(1);
    let task = args.next().unwrap_or_default();
    let rest: Vec<String> = args.collect();
    match task.as_str() {
        "test" => run(&cargo(), &["test"]),
        "build" => {
            build();
        }
        "iso" => {
            iso();
        }
        "run" => boot(&rest, false),
        "debug" => boot(&rest, true),
        "" => usage(2),
        other => {
            eprintln!("unknown xtask command: {other:?}");
            usage(2);
        }
    }
}

fn usage(code: i32) -> ! {
    eprintln!("usage: cargo xtask <test|build|iso|run|debug> [--bios|--kvm] [-- <qemu args>]");
    exit(code);
}

/// Run a command, inheriting stdio, and abort the task if it fails.
fn run(program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .status()
        .unwrap_or_else(|e| fail(&format!("failed to spawn `{program}`: {e}")));
    if !status.success() {
        fail(&format!(
            "`{program}` exited with {}",
            status.code().unwrap_or(-1)
        ));
    }
}

/// Run a command (owned args), inheriting stdio, and return its exit code.
fn exec(program: &str, args: &[String]) -> i32 {
    let status = Command::new(program)
        .args(args)
        .status()
        .unwrap_or_else(|e| fail(&format!("failed to spawn `{program}`: {e}")));
    status.code().unwrap_or(1)
}

fn fail(msg: &str) -> ! {
    eprintln!("xtask: {msg}");
    exit(1);
}

/// `cargo` honoring the toolchain that invoked us.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// Cross-compile the kernel and return the path to the resulting ELF.
fn build() -> PathBuf {
    run(
        &cargo(),
        &["build", "--package", "kernel", "--target", KERNEL_TARGET],
    );
    let elf = PathBuf::from("target")
        .join(KERNEL_TARGET)
        .join("debug")
        .join("kernel");
    if !elf.exists() {
        fail(&format!("expected kernel ELF at {}", elf.display()));
    }
    println!("kernel built: {}", elf.display());
    elf
}

/// Clone the pinned Limine binary branch and build its host `limine` tool.
/// Idempotent: skips the clone if `limine/` already exists.
fn fetch_limine() -> PathBuf {
    let dir = PathBuf::from("limine");
    if !dir.exists() {
        run(
            "git",
            &[
                "clone",
                LIMINE_REPO,
                "--branch",
                LIMINE_BRANCH,
                "--depth",
                "1",
                dir.to_str().unwrap(),
            ],
        );
    }
    // Build the `limine` host utility used to install the BIOS stage.
    run("make", &["-C", dir.to_str().unwrap()]);
    dir
}

fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) {
    let (from, to) = (from.as_ref(), to.as_ref());
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| fail(&format!("mkdir {}: {e}", parent.display())));
    }
    std::fs::copy(from, to)
        .unwrap_or_else(|e| fail(&format!("copy {} -> {}: {e}", from.display(), to.display())));
}

/// Build a bootable ISO (BIOS + UEFI) at `build/kernel.iso`.
fn iso() {
    let kernel = build();
    let limine = fetch_limine();

    // Lay out the ISO root.
    let root = PathBuf::from("build/iso_root");
    let _ = std::fs::remove_dir_all(&root);
    copy(&kernel, root.join("boot/kernel"));
    copy("boot/limine.conf", root.join("boot/limine/limine.conf"));
    for f in [
        "limine-bios.sys",
        "limine-bios-cd.bin",
        "limine-uefi-cd.bin",
    ] {
        copy(limine.join(f), root.join("boot/limine").join(f));
    }
    for f in ["BOOTX64.EFI", "BOOTIA32.EFI"] {
        copy(limine.join(f), root.join("EFI/BOOT").join(f));
    }

    // Build the hybrid El Torito (BIOS) + EFI ISO.
    let out = "build/kernel.iso";
    run(
        "xorriso",
        &[
            "-as",
            "mkisofs",
            "-b",
            "boot/limine/limine-bios-cd.bin",
            "-no-emul-boot",
            "-boot-load-size",
            "4",
            "-boot-info-table",
            "--efi-boot",
            "boot/limine/limine-uefi-cd.bin",
            "-efi-boot-part",
            "--efi-boot-image",
            "--protective-msdos-label",
            root.to_str().unwrap(),
            "-o",
            out,
        ],
    );

    // Install the BIOS boot stage into the ISO.
    let limine_tool = limine.join("limine");
    run(limine_tool.to_str().unwrap(), &["bios-install", out]);

    println!("ISO built: {out}");
}

/// Candidate locations for the OVMF (edk2) x86-64 firmware code file, across
/// Homebrew (macOS) and common Linux distributions.
const OVMF_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/qemu/share/qemu/edk2-x86_64-code.fd",
    "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
    "/usr/local/opt/qemu/share/qemu/edk2-x86_64-code.fd",
    "/usr/local/share/qemu/edk2-x86_64-code.fd",
    "/usr/share/qemu/edk2-x86_64-code.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/share/edk2/x64/OVMF_CODE.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
];

/// Locate OVMF firmware for UEFI boot: the `OVMF`/`OVMF_CODE` env var if set,
/// else the first existing well-known path.
fn find_ovmf() -> Option<PathBuf> {
    for var in ["OVMF", "OVMF_CODE"] {
        if let Ok(path) = std::env::var(var) {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
    }
    OVMF_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

/// Build a fresh ISO and boot it in QEMU. With `debug`, QEMU starts paused with
/// a GDB stub on `localhost:1234`. Defaults to UEFI (OVMF); `--bios` uses
/// SeaBIOS, `--kvm` enables KVM (Linux). Arguments after `--` pass to QEMU.
fn boot(opts: &[String], debug: bool) -> ! {
    let mut bios = false;
    let mut kvm = false;
    let mut passthrough: Vec<String> = Vec::new();
    let mut iter = opts.iter();
    while let Some(opt) = iter.next() {
        match opt.as_str() {
            "--bios" => bios = true,
            "--uefi" => bios = false,
            "--kvm" => kvm = true,
            "--" => {
                passthrough.extend(iter.cloned());
                break;
            }
            other => fail(&format!(
                "unknown option {other:?} (use: --bios | --uefi | --kvm | -- <qemu args>)"
            )),
        }
    }

    iso(); // always boot a fresh image

    let iso_path = "build/kernel.iso";
    let mut qargs: Vec<String> = [
        "-M",
        "q35",
        // Expose SMEP/SMAP (absent from the default qemu64) so the kernel's
        // supervisor-access protections are actually enabled and exercised.
        "-cpu",
        "qemu64,+smep,+smap",
        "-m",
        "256M",
        "-cdrom",
        iso_path,
        "-serial",
        "stdio",
        "-no-reboot",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();

    if kvm {
        qargs.push("-enable-kvm".to_string());
    }

    if bios {
        qargs.push("-boot".to_string());
        qargs.push("d".to_string());
    } else {
        let ovmf = find_ovmf().unwrap_or_else(|| {
            fail(
                "OVMF firmware not found for UEFI boot. Set OVMF=/path/to/edk2-x86_64-code.fd, \
                 or use `--bios`.",
            )
        });
        qargs.push("-drive".to_string());
        qargs.push(format!(
            "if=pflash,unit=0,format=raw,file={},readonly=on",
            ovmf.display()
        ));
    }

    if debug {
        // GDB stub on :1234, CPU halted until the debugger continues.
        qargs.push("-s".to_string());
        qargs.push("-S".to_string());
        let kernel = PathBuf::from("target")
            .join(KERNEL_TARGET)
            .join("debug")
            .join("kernel");
        eprintln!("xtask: QEMU paused with a GDB stub on :1234. Connect with:");
        eprintln!(
            "    x86_64-elf-gdb {} -ex 'target remote :1234'",
            kernel.display()
        );
        eprintln!("    (or: lldb {} -o 'gdb-remote 1234')", kernel.display());
    }

    qargs.extend(passthrough);

    let mode = if bios { "BIOS" } else { "UEFI" };
    println!(
        "xtask: booting {iso_path} in QEMU ({mode}{})",
        if debug { ", paused for debugger" } else { "" }
    );
    let code = exec("qemu-system-x86_64", &qargs);
    exit(code);
}
