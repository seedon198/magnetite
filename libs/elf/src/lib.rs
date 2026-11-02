#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! A pure ELF64 validator for user programs (#82,
//! [ADR 0019](../../docs/adr/0019-first-user-task.md)).
//!
//! The kernel's first parser of untrusted input. [`parse`] checks the header
//! and every loadable segment before the kernel maps anything: a statically
//! linked x86-64 executable, every segment inside the file and inside the
//! user range, no two segments sharing a page, none both writable and
//! executable, and an entry point inside an executable segment. Every field is
//! read with explicit little-endian decoding and checked arithmetic, so no
//! input can make it panic.

#[cfg(test)]
mod tests;

/// Most loadable segments an image may have.
pub const MAX_SEGMENTS: usize = 8;

/// Most pages an image's segments may cover in total (16 MiB): far more than
/// any boot program needs, and small enough that a hostile image cannot drain
/// the frame allocator before the loader refuses it (#132).
pub const MAX_IMAGE_PAGES: u64 = 4096;

const PAGE: u64 = 4096;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

/// One loadable segment, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Segment {
    /// Where the segment starts in the task's address space.
    pub vaddr: u64,
    /// Its size in memory (the bytes past `file_size` are zero).
    pub mem_size: u64,
    /// Where its bytes start in the file.
    pub offset: u64,
    /// How many bytes come from the file.
    pub file_size: u64,
    /// Mapped writable (never together with `executable`).
    pub writable: bool,
    /// Mapped executable.
    pub executable: bool,
}

/// One page of a segment to load: map a zeroed frame at `page`, then copy
/// `len` bytes from `file_offset` in the file to `frame_offset` in the frame.
/// A page holding only bss has `len == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLoad {
    /// The page-aligned address to map.
    pub page: u64,
    /// Where the bytes start in the file.
    pub file_offset: u64,
    /// Where they go in the frame.
    pub frame_offset: u64,
    /// How many file bytes the page holds.
    pub len: u64,
}

impl Segment {
    /// Every page the segment covers, in order, with the file bytes each one
    /// holds. Pure arithmetic, so segments that start mid-page or mix file
    /// bytes and bss in one page are host-tested (#133). Meaningful for a
    /// segment [`parse`] accepted, whose sums cannot overflow.
    pub fn load_plan(&self) -> impl Iterator<Item = PageLoad> + '_ {
        let (first, end) = pages(self);
        let file_end = self.vaddr + self.file_size;
        (first..end).step_by(PAGE as usize).map(move |page| {
            let (from, to) = (page.max(self.vaddr), (page + PAGE).min(file_end));
            if from < to {
                PageLoad {
                    page,
                    file_offset: self.offset + (from - self.vaddr),
                    frame_offset: from - page,
                    len: to - from,
                }
            } else {
                PageLoad {
                    page,
                    file_offset: 0,
                    frame_offset: 0,
                    len: 0,
                }
            }
        })
    }
}

/// A validated executable: its entry point and loadable segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    /// The first instruction.
    pub entry: u64,
    segments: [Segment; MAX_SEGMENTS],
    count: usize,
}

impl Image {
    /// The loadable segments, in file order.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

/// Why an image was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// The file ends inside a header.
    Truncated,
    /// Not an ELF file.
    BadMagic,
    /// Not a 64-bit ELF file.
    NotElf64,
    /// Not little-endian.
    NotLittleEndian,
    /// An unknown ELF version.
    BadVersion,
    /// Not a static executable (`ET_EXEC`).
    NotExecutable,
    /// Not for x86-64.
    WrongMachine,
    /// Program headers of an unexpected size.
    BadHeaderSize,
    /// More than [`MAX_SEGMENTS`] loadable segments.
    TooManySegments,
    /// No loadable segment.
    NoSegments,
    /// A segment's bytes lie outside the file.
    SegmentOutOfFile,
    /// A segment has more file bytes than memory.
    FileLargerThanMemory,
    /// A segment's address and file offset disagree within a page.
    Misaligned,
    /// A segment lies in the null page, the kernel half or past the limit.
    OutOfUserRange,
    /// Two segments share a page.
    Overlap,
    /// A segment is both writable and executable.
    WriteAndExecute,
    /// The entry point is not inside an executable segment.
    BadEntry,
    /// The segments cover more than [`MAX_IMAGE_PAGES`] pages in total.
    TooLarge,
}

fn bytes<const N: usize>(b: &[u8], at: usize) -> Result<[u8; N], ElfError> {
    let end = at.checked_add(N).ok_or(ElfError::Truncated)?;
    b.get(at..end)
        .and_then(|s| s.try_into().ok())
        .ok_or(ElfError::Truncated)
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, ElfError> {
    bytes(b, at).map(u16::from_le_bytes)
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, ElfError> {
    bytes(b, at).map(u32::from_le_bytes)
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, ElfError> {
    bytes(b, at).map(u64::from_le_bytes)
}

/// The pages a segment touches, as `[first, end)` page-aligned addresses.
fn pages(s: &Segment) -> (u64, u64) {
    // Validated: `vaddr + mem_size <= user_limit`, so neither overflows.
    let first = s.vaddr & !(PAGE - 1);
    let end = (s.vaddr + s.mem_size).div_ceil(PAGE) * PAGE;
    (first, end)
}

/// Validate `bytes` as a user program whose segments must all end at or below
/// `user_limit`. Nothing is mapped or copied: the caller loads the returned
/// segments only if this succeeds.
///
/// # Errors
/// The first rule the image breaks, as an [`ElfError`].
pub fn parse(bytes: &[u8], user_limit: u64) -> Result<Image, ElfError> {
    if bytes.len() < EHDR_SIZE {
        return Err(ElfError::Truncated);
    }
    if bytes[..4] != *b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if bytes[4] != 2 {
        return Err(ElfError::NotElf64);
    }
    if bytes[5] != 1 {
        return Err(ElfError::NotLittleEndian);
    }
    if bytes[6] != 1 || u32_at(bytes, 20)? != 1 {
        return Err(ElfError::BadVersion);
    }
    if u16_at(bytes, 16)? != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if u16_at(bytes, 18)? != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }
    if usize::from(u16_at(bytes, 54)?) != PHDR_SIZE {
        return Err(ElfError::BadHeaderSize);
    }
    let entry = u64_at(bytes, 24)?;
    let phoff = usize::try_from(u64_at(bytes, 32)?).map_err(|_| ElfError::Truncated)?;
    let phnum = usize::from(u16_at(bytes, 56)?);
    let table_end = phnum
        .checked_mul(PHDR_SIZE)
        .and_then(|n| n.checked_add(phoff))
        .ok_or(ElfError::Truncated)?;
    if table_end > bytes.len() {
        return Err(ElfError::Truncated);
    }

    let mut image = Image {
        entry,
        segments: [Segment::default(); MAX_SEGMENTS],
        count: 0,
    };
    for i in 0..phnum {
        let p = phoff + i * PHDR_SIZE;
        if u32_at(bytes, p)? != PT_LOAD {
            continue;
        }
        let flags = u32_at(bytes, p + 4)?;
        let s = Segment {
            offset: u64_at(bytes, p + 8)?,
            vaddr: u64_at(bytes, p + 16)?,
            file_size: u64_at(bytes, p + 32)?,
            mem_size: u64_at(bytes, p + 40)?,
            writable: flags & PF_W != 0,
            executable: flags & PF_X != 0,
        };
        if s.mem_size == 0 {
            continue; // maps nothing
        }
        if image.count == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        check_segment(&s, bytes.len() as u64, user_limit)?;
        image.segments[image.count] = s;
        image.count += 1;
    }
    if image.count == 0 {
        return Err(ElfError::NoSegments);
    }

    let segs = image.segments();
    let total: u64 = segs
        .iter()
        .map(|s| {
            let (first, end) = pages(s);
            (end - first) / PAGE
        })
        .sum();
    if total > MAX_IMAGE_PAGES {
        return Err(ElfError::TooLarge);
    }
    for (i, a) in segs.iter().enumerate() {
        let (a0, a1) = pages(a);
        for b in &segs[i + 1..] {
            let (b0, b1) = pages(b);
            if a0 < b1 && b0 < a1 {
                return Err(ElfError::Overlap);
            }
        }
    }
    let entry_ok = segs
        .iter()
        .any(|s| s.executable && entry >= s.vaddr && entry - s.vaddr < s.mem_size);
    if !entry_ok {
        return Err(ElfError::BadEntry);
    }
    Ok(image)
}

/// The per-segment rules.
fn check_segment(s: &Segment, file_len: u64, user_limit: u64) -> Result<(), ElfError> {
    match s.offset.checked_add(s.file_size) {
        Some(end) if end <= file_len => {}
        _ => return Err(ElfError::SegmentOutOfFile),
    }
    if s.file_size > s.mem_size {
        return Err(ElfError::FileLargerThanMemory);
    }
    if s.vaddr % PAGE != s.offset % PAGE {
        return Err(ElfError::Misaligned);
    }
    match s.vaddr.checked_add(s.mem_size) {
        Some(end) if s.vaddr >= PAGE && end <= user_limit => {}
        _ => return Err(ElfError::OutOfUserRange),
    }
    if s.writable && s.executable {
        return Err(ElfError::WriteAndExecute);
    }
    Ok(())
}
