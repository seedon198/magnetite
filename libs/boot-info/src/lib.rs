#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Architecture-neutral boot information.
//!
//! The boot code (which talks to the bootloader, e.g. Limine) translates the
//! boot protocol's responses into this type, so nothing outside the boot
//! module depends on the bootloader. The physical memory map is **copied** in
//! (not referenced), so the kernel does not depend on bootloader-reclaimable
//! memory staying live.

/// Maximum number of physical memory regions we record. Real firmware memory
/// maps are well under this.
pub const MAX_MEMORY_REGIONS: usize = 256;

/// The kind of a physical memory region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Free RAM available for the allocator.
    Usable,
    /// Firmware/hardware reserved; never usable.
    Reserved,
    /// Reclaimable once the bootloader is no longer needed.
    BootloaderReclaimable,
    /// Occupied by the kernel image and boot modules.
    KernelAndModules,
    /// The framebuffer.
    Framebuffer,
    /// ACPI tables, reclaimable after they are parsed.
    AcpiReclaimable,
    /// ACPI non-volatile storage; must be preserved.
    AcpiNvs,
    /// Known-bad RAM.
    BadMemory,
    /// Anything else the bootloader reported.
    Other,
}

/// A single physical memory region.
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// Physical base address.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// What the region is.
    pub kind: RegionKind,
}

impl MemoryRegion {
    const EMPTY: Self = Self {
        base: 0,
        length: 0,
        kind: RegionKind::Other,
    };
}

/// A linear framebuffer the bootloader set up for us.
#[derive(Debug, Clone, Copy)]
pub struct Framebuffer {
    /// Virtual address of the framebuffer (mapped by the bootloader).
    pub address: u64,
    /// Visible width in pixels.
    pub width: u64,
    /// Visible height in pixels.
    pub height: u64,
    /// Bytes per scanline.
    pub pitch: u64,
    /// Bits per pixel.
    pub bpp: u16,
    /// Bit position of the red channel within a pixel.
    pub red_shift: u8,
    /// Bit position of the green channel within a pixel.
    pub green_shift: u8,
    /// Bit position of the blue channel within a pixel.
    pub blue_shift: u8,
}

/// The platform firmware that started the bootloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    /// Legacy PC BIOS.
    Bios,
    /// UEFI (32- or 64-bit).
    Uefi,
    /// Not reported by the bootloader.
    Unknown,
}

impl Firmware {
    /// A short display name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Firmware::Bios => "BIOS",
            Firmware::Uefi => "UEFI",
            Firmware::Unknown => "unknown firmware",
        }
    }
}

/// A short UTF-8 string held inline, so text from the bootloader (its name and
/// version) is copied out of bootloader-reclaimable memory like the memory map.
/// Input longer than `N` bytes is truncated at a character boundary.
#[derive(Clone, Copy)]
pub struct ShortStr<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> ShortStr<N> {
    /// The empty string.
    pub const EMPTY: Self = Self {
        buf: [0; N],
        len: 0,
    };

    /// Copy `s`, truncating to at most `N` bytes on a character boundary.
    #[must_use]
    pub fn new(s: &str) -> Self {
        let mut len = s.len().min(N);
        while !s.is_char_boundary(len) {
            len -= 1;
        }
        let mut buf = [0; N];
        buf[..len].copy_from_slice(&s.as_bytes()[..len]);
        Self { buf, len }
    }

    /// The stored text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Always valid: built from a `&str` cut at a character boundary.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// The most boot modules recorded.
pub const MAX_MODULES: usize = 4;

/// A file the bootloader loaded alongside the kernel (a user program, #82).
#[derive(Clone, Copy)]
pub struct Module {
    /// Where its bytes start, as a virtual address in the HHDM.
    pub address: u64,
    /// Its size in bytes.
    pub size: u64,
    /// The last component of its path (e.g. `root`).
    pub name: ShortStr<32>,
}

impl Module {
    const EMPTY: Self = Self {
        address: 0,
        size: 0,
        name: ShortStr::EMPTY,
    };
}

/// Information handed from the boot code to `kmain`.
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// Offset of the Higher-Half Direct Map: `virt = phys + hhdm_offset`.
    pub hhdm_offset: u64,
    /// Physical base address of the loaded kernel image.
    pub kernel_phys_base: u64,
    /// Virtual base address of the loaded kernel image.
    pub kernel_virt_base: u64,
    /// The framebuffer, if the bootloader provided one.
    pub framebuffer: Option<Framebuffer>,
    /// The firmware that started the bootloader.
    pub firmware: Firmware,
    /// The bootloader's name (e.g. `Limine`), empty if not reported.
    pub bootloader_name: ShortStr<32>,
    /// The bootloader's version string, empty if not reported.
    pub bootloader_version: ShortStr<32>,
    regions: [MemoryRegion; MAX_MEMORY_REGIONS],
    region_count: usize,
    modules: [Module; MAX_MODULES],
    module_count: usize,
    /// Regions and modules that did not fit their tables (#124).
    dropped_regions: usize,
    dropped_modules: usize,
    /// The end of the highest region the bootloader reported, dropped ones
    /// included.
    map_end: u64,
}

impl Default for BootInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl BootInfo {
    /// Create an empty `BootInfo`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hhdm_offset: 0,
            kernel_phys_base: 0,
            kernel_virt_base: 0,
            framebuffer: None,
            firmware: Firmware::Unknown,
            bootloader_name: ShortStr::EMPTY,
            bootloader_version: ShortStr::EMPTY,
            regions: [MemoryRegion::EMPTY; MAX_MEMORY_REGIONS],
            region_count: 0,
            modules: [Module::EMPTY; MAX_MODULES],
            module_count: 0,
            dropped_regions: 0,
            dropped_modules: 0,
            map_end: 0,
        }
    }

    /// Append a memory region. Returns `false` if the table is full: the region
    /// is dropped and counted in [`dropped_regions`](Self::dropped_regions),
    /// though it still counts towards [`highest_address`](Self::highest_address).
    pub fn push_region(&mut self, region: MemoryRegion) -> bool {
        self.map_end = self.map_end.max(region.base.saturating_add(region.length));
        if self.region_count >= MAX_MEMORY_REGIONS {
            self.dropped_regions += 1;
            return false;
        }
        self.regions[self.region_count] = region;
        self.region_count += 1;
        true
    }

    /// Append a boot module. Returns `false` if the table is full: the module
    /// is dropped and counted in [`dropped_modules`](Self::dropped_modules).
    pub fn push_module(&mut self, module: Module) -> bool {
        if self.module_count >= MAX_MODULES {
            self.dropped_modules += 1;
            return false;
        }
        self.modules[self.module_count] = module;
        self.module_count += 1;
        true
    }

    /// The recorded boot modules.
    #[must_use]
    pub fn modules(&self) -> &[Module] {
        &self.modules[..self.module_count]
    }

    /// The boot module called `name`, if the bootloader loaded one.
    #[must_use]
    pub fn module(&self, name: &str) -> Option<&Module> {
        self.modules().iter().find(|m| m.name.as_str() == name)
    }

    /// The recorded memory regions.
    #[must_use]
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions[..self.region_count]
    }

    /// Total bytes of actual RAM (all regions except firmware-reserved,
    /// framebuffer, and known-bad memory - i.e. excluding MMIO holes).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.regions()
            .iter()
            .filter(|r| {
                !matches!(
                    r.kind,
                    RegionKind::Reserved | RegionKind::Framebuffer | RegionKind::BadMemory
                )
            })
            .map(|r| r.length)
            .sum()
    }

    /// The highest physical address reported by any region, MMIO and regions
    /// dropped for lack of room included: the extent the HHDM must cover.
    #[must_use]
    pub fn highest_address(&self) -> u64 {
        self.map_end
    }

    /// Memory regions the bootloader reported that did not fit the table.
    #[must_use]
    pub fn dropped_regions(&self) -> usize {
        self.dropped_regions
    }

    /// Boot modules that did not fit the table.
    #[must_use]
    pub fn dropped_modules(&self) -> usize {
        self.dropped_modules
    }

    /// Total bytes of [`RegionKind::Usable`] RAM.
    #[must_use]
    pub fn usable_bytes(&self) -> u64 {
        self.regions()
            .iter()
            .filter(|r| r.kind == RegionKind::Usable)
            .map(|r| r.length)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    fn module(name: &str, address: u64) -> Module {
        Module {
            address,
            size: 0x1000,
            name: ShortStr::new(name),
        }
    }

    #[test]
    fn module_lookup_by_name() {
        let mut info = BootInfo::new();
        assert!(info.push_module(module("root", 0x1000)));
        assert!(info.push_module(module("fault", 0x2000)));
        assert_eq!(info.module("fault").map(|m| m.address), Some(0x2000));
        assert_eq!(info.module("root").map(|m| m.address), Some(0x1000));
        assert!(info.module("init").is_none());
        assert_eq!(info.modules().len(), 2);
        assert!(info.push_module(module("c", 0x3000)));
        assert!(info.push_module(module("d", 0x4000)));
        assert!(!info.push_module(module("e", 0x5000)), "a fifth module fit");
        assert_eq!(info.modules().len(), MAX_MODULES);
    }

    use super::*;

    fn region(base: u64, length: u64, kind: RegionKind) -> MemoryRegion {
        MemoryRegion { base, length, kind }
    }

    #[test]
    fn totals_exclude_mmio_and_sum_usable() {
        let mut bi = BootInfo::new();
        assert!(bi.push_region(region(0, 2 * MIB, RegionKind::Usable)));
        assert!(bi.push_region(region(2 * MIB, MIB, RegionKind::BootloaderReclaimable)));
        assert!(bi.push_region(region(0xfee0_0000, 4 * MIB, RegionKind::Reserved))); // MMIO hole
        assert!(bi.push_region(region(3 * MIB, MIB, RegionKind::Usable)));

        assert_eq!(bi.usable_bytes(), 3 * MIB);
        assert_eq!(bi.total_bytes(), 4 * MIB); // excludes the reserved MMIO region
        assert_eq!(bi.highest_address(), 0xfee0_0000 + 4 * MIB);
        assert_eq!(bi.regions().len(), 4);
    }

    #[test]
    fn push_region_reports_when_full() {
        let mut bi = BootInfo::new();
        for _ in 0..MAX_MEMORY_REGIONS {
            assert!(bi.push_region(region(0, 1, RegionKind::Usable)));
        }
        assert!(!bi.push_region(region(0, 1, RegionKind::Usable)));
        assert_eq!(bi.regions().len(), MAX_MEMORY_REGIONS);
    }

    #[test]
    fn dropped_entries_are_counted_and_still_bound_the_map() {
        let mut bi = BootInfo::new();
        for i in 0..MAX_MEMORY_REGIONS as u64 {
            assert!(bi.push_region(region(i * 0x1000, 0x1000, RegionKind::Usable)));
        }
        // A high reserved region past the table's capacity: dropped, but the
        // map still ends at its end (the HHDM maps it, #124).
        assert!(!bi.push_region(region(0xfee0_0000, 0x1000, RegionKind::Reserved)));
        assert_eq!(bi.dropped_regions(), 1);
        assert_eq!(bi.highest_address(), 0xfee0_1000);
        for _ in 0..=MAX_MODULES {
            bi.push_module(Module::EMPTY);
        }
        assert_eq!(bi.dropped_modules(), 1);
    }

    #[test]
    fn empty_boot_info_has_zero_totals() {
        let bi = BootInfo::new();
        assert_eq!(bi.usable_bytes(), 0);
        assert_eq!(bi.total_bytes(), 0);
        assert_eq!(bi.highest_address(), 0);
    }

    #[test]
    fn short_str_copies_and_truncates_on_char_boundary() {
        assert_eq!(ShortStr::<8>::new("Limine").as_str(), "Limine");
        assert_eq!(ShortStr::<4>::new("10.1.2").as_str(), "10.1");
        // "é" is two bytes; a 2-byte cut would split it, so it backs off to 1.
        assert_eq!(ShortStr::<2>::new("aé").as_str(), "a");
        assert_eq!(ShortStr::<8>::EMPTY.as_str(), "");
    }

    const MIB: u64 = 1024 * 1024;
}
