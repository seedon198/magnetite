//! Bootloader handoff: the Limine protocol boundary.
//!
//! This is the kernel's only bootloader-specific code. It declares the Limine
//! requests, provides the ELF entry point `_start`, validates the protocol
//! revision, and (as later milestones need) translates Limine's responses into
//! the architecture-neutral [`BootInfo`] consumed by the rest of the kernel.
//!
//! ## `unsafe` in this module
//!
//! This module carries `#![allow(unsafe_code)]` - the only place outside `hal`
//! permitted to, and CI-enforced to stay that way (see
//! `docs/adr/0007-unsafe-policy.md`). The `unsafe` here is limited to linker
//! glue: the `#[unsafe(link_section = ...)]` placement of the Limine request
//! statics (so the bootloader can find them) and the `#[unsafe(no_mangle)]`
//! entry symbol, plus one slice over the boot modules Limine loaded
//! ([`module_bytes`]). Real hardware access still goes through `hal`.
#![allow(unsafe_code)]

use boot_info::{BootInfo, Firmware, Framebuffer, MemoryRegion, Module, RegionKind, ShortStr};
use limine::firmware_type::FirmwareType;
use limine::memory_map::EntryType;
use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, FirmwareTypeRequest, FramebufferRequest,
    HhdmRequest, MemoryMapRequest, ModuleRequest, RequestsEndMarker, RequestsStartMarker,
};
use limine::BaseRevision;

use core::sync::atomic::{AtomicPtr, Ordering};

/// `_start`'s [`BootInfo`], for [`info`] once `kmain` has left the bootloader's
/// stack (#66).
static BOOT_INFO: AtomicPtr<BootInfo> = AtomicPtr::new(core::ptr::null_mut());

/// The boot handoff `_start` assembled.
///
/// # Panics
/// If called before `_start` has recorded it.
#[must_use]
pub fn info() -> &'static BootInfo {
    let p = BOOT_INFO.load(Ordering::Acquire);
    assert!(
        !p.is_null(),
        "boot: BootInfo used before _start recorded it"
    );
    // SAFETY: `p` points at `_start`'s `boot_info` local. `_start` never
    // returns (it ends in `kmain`, which is `-> !`), so that stack frame lives
    // as long as the kernel; the bootloader's stack stays mapped (in the HHDM)
    // and is never reclaimed. The value is not mutated after it is recorded.
    unsafe { &*p }
}

/// The base revision of the Limine protocol this kernel is built against. The
/// bootloader refuses to boot (or reports unsupported) if it cannot satisfy it.
#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

/// Map of physical memory: which regions are usable, reserved, or reclaimable.
/// Consumed by the physical frame allocator (M2).
#[used]
#[unsafe(link_section = ".requests")]
static MEMORY_MAP: MemoryMapRequest = MemoryMapRequest::new();

/// Offset of the Higher-Half Direct Map, through which the kernel reaches
/// physical memory (M2).
#[used]
#[unsafe(link_section = ".requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

/// Physical and virtual base of the loaded kernel image, for phys<->virt
/// translation and the KASLR base (M2).
#[used]
#[unsafe(link_section = ".requests")]
static KERNEL_ADDRESS: ExecutableAddressRequest = ExecutableAddressRequest::new();

/// A linear framebuffer for the on-screen console (M1, #14).
#[used]
#[unsafe(link_section = ".requests")]
static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

/// The bootloader's name and version, for the boot banner.
#[used]
#[unsafe(link_section = ".requests")]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

/// Whether we were started from BIOS or UEFI, for the boot banner.
#[used]
#[unsafe(link_section = ".requests")]
static FIRMWARE_TYPE: FirmwareTypeRequest = FirmwareTypeRequest::new();

/// The user programs loaded alongside the kernel (#82).
#[used]
#[unsafe(link_section = ".requests")]
static MODULES: ModuleRequest = ModuleRequest::new();

/// Markers bounding the request section so the bootloader's scan is fast.
#[used]
#[unsafe(link_section = ".requests_start_marker")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();
#[used]
#[unsafe(link_section = ".requests_end_marker")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// The ELF entry point. Limine hands control here in 64-bit long mode with the
/// kernel mapped at its higher-half link address.
///
/// It validates the protocol revision, assembles a [`BootInfo`], and calls the
/// architecture-neutral [`crate::kmain`]. If the bootloader could not satisfy
/// our base revision, there is nothing safe to do but halt.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    if !BASE_REVISION.is_supported() {
        hal::cpu::halt_loop();
    }

    let mut boot_info = BootInfo::new();

    if let Some(hhdm) = HHDM.get_response() {
        boot_info.hhdm_offset = hhdm.offset();
    }
    if let Some(addr) = KERNEL_ADDRESS.get_response() {
        boot_info.kernel_phys_base = addr.physical_base();
        boot_info.kernel_virt_base = addr.virtual_base();
    }
    if let Some(mmap) = MEMORY_MAP.get_response() {
        for entry in mmap.entries() {
            boot_info.push_region(MemoryRegion {
                base: entry.base,
                length: entry.length,
                kind: region_kind(entry.entry_type),
            });
        }
    }
    if let Some(fb) = FRAMEBUFFER
        .get_response()
        .and_then(|r| r.framebuffers().next())
    {
        boot_info.framebuffer = Some(Framebuffer {
            address: fb.addr() as u64,
            width: fb.width(),
            height: fb.height(),
            pitch: fb.pitch(),
            bpp: fb.bpp(),
            red_shift: fb.red_mask_shift(),
            green_shift: fb.green_mask_shift(),
            blue_shift: fb.blue_mask_shift(),
        });
    }

    if let Some(info) = BOOTLOADER_INFO.get_response() {
        boot_info.bootloader_name = ShortStr::new(info.name());
        boot_info.bootloader_version = ShortStr::new(info.version());
    }
    if let Some(fw) = FIRMWARE_TYPE.get_response() {
        boot_info.firmware = match fw.firmware_type() {
            FirmwareType::X86_BIOS => Firmware::Bios,
            FirmwareType::UEFI_32 | FirmwareType::UEFI_64 => Firmware::Uefi,
            _ => Firmware::Unknown,
        };
    }

    if let Some(modules) = MODULES.get_response() {
        for file in modules.modules() {
            let path = file.path().to_str().unwrap_or("");
            let name = path.rsplit('/').next().unwrap_or(path);
            boot_info.push_module(Module {
                address: file.addr() as u64,
                size: file.size(),
                name: ShortStr::new(name),
            });
        }
    }

    BOOT_INFO.store(&raw mut boot_info, Ordering::Release);
    crate::kmain(info())
}

// Section boundary symbols defined by `kernel/arch/x86_64/linker.ld`. Each is
// page-aligned; they carry no data, only addresses.
extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
}

/// The virtual-address ranges (`start..end`, page-aligned) of the kernel's
/// loadable sections, read from linker symbols. Used by the memory manager to
/// apply W^X permissions (#40).
#[derive(Clone, Copy)]
pub struct KernelSections {
    /// `.text` - executable code (maps R-X).
    pub text: (u64, u64),
    /// `.rodata` - read-only data (maps R, NX).
    pub rodata: (u64, u64),
    /// `.data` + `.bss` - writable data (maps RW, NX).
    pub data: (u64, u64),
}

/// Read the kernel's section boundaries from the linker-defined symbols.
#[must_use]
pub fn kernel_sections() -> KernelSections {
    use core::ptr::addr_of;
    // These are linker-defined boundary symbols; we only take their addresses
    // (never dereference them), which is safe even for `extern` statics.
    KernelSections {
        text: (addr_of!(__text_start) as u64, addr_of!(__text_end) as u64),
        rodata: (
            addr_of!(__rodata_start) as u64,
            addr_of!(__rodata_end) as u64,
        ),
        data: (addr_of!(__data_start) as u64, addr_of!(__data_end) as u64),
    }
}

/// Translate a Limine memory-map entry type into our architecture-neutral kind.
/// The bytes of boot module `m`.
pub fn module_bytes(m: &Module) -> &'static [u8] {
    // SAFETY: Limine loaded the module at `m.address` (an HHDM address) with
    // `m.size` bytes and reports that memory as `KernelAndModules`, which the
    // frame allocator never hands out (kmain checks the module lies outside
    // every usable region), and nothing writes it: so it is valid, unaliased
    // by any `&mut`, for the life of the kernel.
    unsafe { core::slice::from_raw_parts(m.address as *const u8, m.size as usize) }
}

fn region_kind(entry_type: EntryType) -> RegionKind {
    match entry_type {
        EntryType::USABLE => RegionKind::Usable,
        EntryType::RESERVED => RegionKind::Reserved,
        EntryType::BOOTLOADER_RECLAIMABLE => RegionKind::BootloaderReclaimable,
        EntryType::EXECUTABLE_AND_MODULES => RegionKind::KernelAndModules,
        EntryType::FRAMEBUFFER => RegionKind::Framebuffer,
        EntryType::ACPI_RECLAIMABLE => RegionKind::AcpiReclaimable,
        EntryType::ACPI_NVS => RegionKind::AcpiNvs,
        EntryType::BAD_MEMORY => RegionKind::BadMemory,
        _ => RegionKind::Other,
    }
}
