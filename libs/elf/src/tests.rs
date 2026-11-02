//! Host tests: every refusal, and adversarial images.

extern crate std;

use std::vec::Vec;

use super::*;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const LIMIT: u64 = 0x7fff_0000_0000;

/// One `PT_LOAD`: (vaddr, memsz, offset, filesz, flags).
type Load = (u64, u64, u64, u64, u32);

/// Build a little ELF64 executable: the header, one program header per load,
/// and zero bytes up to the end of the furthest segment.
fn elf(loads: &[Load], entry: u64) -> Vec<u8> {
    let phnum = loads.len();
    let headers = 64 + 56 * phnum;
    let end = loads
        .iter()
        .map(|l| (l.2 + l.3) as usize)
        .max()
        .unwrap_or(0)
        .max(headers);
    let mut b = std::vec![0u8; end];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // ELFCLASS64
    b[5] = 1; // little endian
    b[6] = 1; // EV_CURRENT
    b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    b[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&entry.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    b[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    b[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    b[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
    for (i, &(vaddr, memsz, off, filesz, flags)) in loads.iter().enumerate() {
        let p = 64 + 56 * i;
        b[p..p + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        b[p + 4..p + 8].copy_from_slice(&flags.to_le_bytes());
        b[p + 8..p + 16].copy_from_slice(&off.to_le_bytes());
        b[p + 16..p + 24].copy_from_slice(&vaddr.to_le_bytes());
        b[p + 32..p + 40].copy_from_slice(&filesz.to_le_bytes());
        b[p + 40..p + 48].copy_from_slice(&memsz.to_le_bytes());
        b[p + 48..p + 56].copy_from_slice(&4096u64.to_le_bytes());
    }
    b
}

/// Text, rodata, and data plus bss, the way `user/link.ld` lays them out.
fn good() -> Vec<u8> {
    elf(
        &[
            (0x40_0000, 0x1800, 0x1000, 0x1800, PF_R | PF_X),
            (0x40_2000, 0x200, 0x3000, 0x200, PF_R),
            (0x40_3000, 0x3000, 0x4000, 0x100, PF_R | PF_W),
        ],
        0x40_0010,
    )
}

fn parsed(b: &[u8]) -> Result<Image, ElfError> {
    parse(b, LIMIT)
}

#[test]
fn accepts_text_rodata_data_bss() {
    let image = parsed(&good()).unwrap();
    assert_eq!(image.entry, 0x40_0010);
    let s = image.segments();
    assert_eq!(s.len(), 3);
    assert_eq!(
        s[0],
        Segment {
            vaddr: 0x40_0000,
            mem_size: 0x1800,
            offset: 0x1000,
            file_size: 0x1800,
            writable: false,
            executable: true
        }
    );
    assert!(!s[1].writable && !s[1].executable);
    assert!(s[2].writable && !s[2].executable);
    assert_eq!(s[2].mem_size, 0x3000);
}

#[test]
fn truncated() {
    assert_eq!(parsed(&good()[..63]), Err(ElfError::Truncated));
    // Program headers that run past the end of the file.
    let mut b = good();
    b[56..58].copy_from_slice(&200u16.to_le_bytes());
    b.truncate(64 + 56 * 3);
    assert_eq!(parsed(&b), Err(ElfError::Truncated));
}

#[test]
fn refuses_truncated_header_at_every_length() {
    let b = good();
    for len in 0..64 + 56 * 3 {
        assert!(parsed(&b[..len]).is_err(), "accepted a {len}-byte prefix");
    }
}

#[test]
fn bad_magic() {
    let mut b = good();
    b[0] = 0;
    assert_eq!(parsed(&b), Err(ElfError::BadMagic));
}

#[test]
fn not_elf64() {
    let mut b = good();
    b[4] = 1;
    assert_eq!(parsed(&b), Err(ElfError::NotElf64));
}

#[test]
fn not_little_endian() {
    let mut b = good();
    b[5] = 2;
    assert_eq!(parsed(&b), Err(ElfError::NotLittleEndian));
}

#[test]
fn bad_version() {
    let mut b = good();
    b[6] = 0;
    assert_eq!(parsed(&b), Err(ElfError::BadVersion));
    let mut b = good();
    b[20] = 2;
    assert_eq!(parsed(&b), Err(ElfError::BadVersion));
}

#[test]
fn not_executable() {
    let mut b = good();
    b[16] = 3; // ET_DYN
    assert_eq!(parsed(&b), Err(ElfError::NotExecutable));
}

#[test]
fn wrong_machine() {
    let mut b = good();
    b[18] = 40; // ARM
    assert_eq!(parsed(&b), Err(ElfError::WrongMachine));
}

#[test]
fn bad_header_size() {
    let mut b = good();
    b[54] = 32;
    assert_eq!(parsed(&b), Err(ElfError::BadHeaderSize));
}

#[test]
fn too_many_segments() {
    let loads: Vec<Load> = (0..MAX_SEGMENTS as u64 + 1)
        .map(|i| {
            (
                0x40_0000 + i * 0x1000,
                0x10,
                0x1000 + i * 0x1000,
                0x10,
                PF_R | PF_X,
            )
        })
        .collect();
    assert_eq!(
        parsed(&elf(&loads, 0x40_0000)),
        Err(ElfError::TooManySegments)
    );
}

#[test]
fn no_segments() {
    assert_eq!(parsed(&elf(&[], 0x40_0000)), Err(ElfError::NoSegments));
    // Program headers of other types do not count.
    let mut b = good();
    for i in 0..3 {
        b[64 + 56 * i] = 4; // PT_NOTE
    }
    assert_eq!(parsed(&b), Err(ElfError::NoSegments));
}

#[test]
fn segment_out_of_file() {
    let mut b = good();
    let len = b.len();
    b.truncate(len - 1);
    assert_eq!(parsed(&b), Err(ElfError::SegmentOutOfFile));
}

#[test]
fn refuses_overflowing_offset() {
    let mut b = good();
    b[64 + 8..64 + 16].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
    assert_eq!(parsed(&b), Err(ElfError::SegmentOutOfFile));
}

#[test]
fn file_larger_than_memory() {
    let b = elf(&[(0x40_0000, 0x10, 0x1000, 0x20, PF_R | PF_X)], 0x40_0000);
    assert_eq!(parsed(&b), Err(ElfError::FileLargerThanMemory));
}

#[test]
fn misaligned() {
    let b = elf(&[(0x40_0010, 0x10, 0x1000, 0x10, PF_R | PF_X)], 0x40_0010);
    assert_eq!(parsed(&b), Err(ElfError::Misaligned));
}

#[test]
fn out_of_user_range() {
    // The null page.
    let b = elf(&[(0x0, 0x10, 0x1000, 0x10, PF_R | PF_X)], 0x0);
    assert_eq!(parsed(&b), Err(ElfError::OutOfUserRange));
    // Ending past the limit.
    let b = elf(
        &[(LIMIT - 0x1000, 0x2000, 0x1000, 0x10, PF_R | PF_X)],
        LIMIT - 0x1000,
    );
    assert_eq!(parsed(&b), Err(ElfError::OutOfUserRange));
    // The kernel half.
    let k = 0xffff_8000_0000_0000;
    let b = elf(&[(k, 0x10, 0x1000, 0x10, PF_R | PF_X)], k);
    assert_eq!(parsed(&b), Err(ElfError::OutOfUserRange));
}

#[test]
fn refuses_segment_ending_past_limit() {
    // vaddr + memsz overflows u64.
    let b = elf(
        &[(0x40_0000, u64::MAX - 0x1000, 0x1000, 0x10, PF_R | PF_X)],
        0x40_0000,
    );
    assert_eq!(parsed(&b), Err(ElfError::OutOfUserRange));
}

#[test]
fn overlap() {
    // Two segments sharing a page, in either order.
    let b = elf(
        &[
            (0x40_0000, 0x10, 0x1000, 0x10, PF_R | PF_X),
            (0x40_0800, 0x10, 0x1800, 0x10, PF_R),
        ],
        0x40_0000,
    );
    assert_eq!(parsed(&b), Err(ElfError::Overlap));
    let b = elf(
        &[
            (0x40_1000, 0x10, 0x2000, 0x10, PF_R),
            (0x40_0000, 0x1001, 0x1000, 0x10, PF_R | PF_X),
        ],
        0x40_0000,
    );
    assert_eq!(parsed(&b), Err(ElfError::Overlap));
}

#[test]
fn write_and_execute() {
    let b = elf(
        &[(0x40_0000, 0x10, 0x1000, 0x10, PF_R | PF_W | PF_X)],
        0x40_0000,
    );
    assert_eq!(parsed(&b), Err(ElfError::WriteAndExecute));
}

#[test]
fn bad_entry() {
    let mut b = good();
    // In the rodata segment: not executable.
    b[24..32].copy_from_slice(&0x40_2000u64.to_le_bytes());
    assert_eq!(parsed(&b), Err(ElfError::BadEntry));
    // In no segment at all.
    b[24..32].copy_from_slice(&0x50_0000u64.to_le_bytes());
    assert_eq!(parsed(&b), Err(ElfError::BadEntry));
}

#[test]
fn zero_size_loads_are_ignored() {
    let b = elf(
        &[
            (0x40_0000, 0x10, 0x1000, 0x10, PF_R | PF_X),
            (0x40_0000, 0, 0x1000, 0, PF_R | PF_W),
        ],
        0x40_0000,
    );
    assert_eq!(parsed(&b).unwrap().segments().len(), 1);
}

#[test]
fn refuses_images_over_the_page_cap() {
    // One read-only page of text, then a bss that brings the image to the cap,
    // then one page past it.
    let text = (0x40_0000, 0x1000, 0x1000, 0x10, PF_R | PF_X);
    let at_cap = (MAX_IMAGE_PAGES - 1) * 4096;
    let ok = elf(
        &[text, (0x40_1000, at_cap, 0x2000, 0, PF_R | PF_W)],
        0x40_0000,
    );
    assert!(
        parsed(&ok).is_ok(),
        "an image of exactly the cap is accepted"
    );
    let over = elf(
        &[text, (0x40_1000, at_cap + 1, 0x2000, 0, PF_R | PF_W)],
        0x40_0000,
    );
    assert_eq!(parsed(&over), Err(ElfError::TooLarge));
}

/// The load plan of `seg`, as (page, file offset, frame offset, length).
fn plan(seg: Segment) -> Vec<(u64, u64, u64, u64)> {
    seg.load_plan()
        .map(|p| (p.page, p.file_offset, p.frame_offset, p.len))
        .collect()
}

fn seg(vaddr: u64, mem_size: u64, offset: u64, file_size: u64) -> Segment {
    Segment {
        vaddr,
        mem_size,
        offset,
        file_size,
        writable: true,
        executable: false,
    }
}

#[test]
fn load_plan_of_a_segment_starting_mid_page() {
    assert_eq!(
        plan(seg(0x40_0800, 0x100, 0x2800, 0x100)),
        [(0x40_0000, 0x2800, 0x800, 0x100)]
    );
}

#[test]
fn load_plan_of_file_bytes_straddling_a_page() {
    assert_eq!(
        plan(seg(0x40_0ff0, 0x20, 0x1ff0, 0x20)),
        [
            (0x40_0000, 0x1ff0, 0xff0, 0x10),
            (0x40_1000, 0x2000, 0, 0x10)
        ]
    );
}

#[test]
fn load_plan_of_file_bytes_then_bss() {
    // A page of file bytes, a page that holds both, and a bss-only page.
    assert_eq!(
        plan(seg(0x40_3000, 0x3000, 0x4000, 0x1100)),
        [
            (0x40_3000, 0x4000, 0, 0x1000),
            (0x40_4000, 0x5000, 0, 0x100),
            (0x40_5000, 0, 0, 0),
        ]
    );
}
