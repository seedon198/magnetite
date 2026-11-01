#![no_std]
#![no_main]
#![forbid(unsafe_code)]

//! The fault task (#82, ADR 0019 decision 9): reads kernel memory, which it
//! does not own. The kernel must end it and carry on.

/// The base of the kernel half: never mapped for ring 3.
const KERNEL_HALF: u64 = 0xffff_8000_0000_0000;

fn main() -> ! {
    sys::debug::read_u64(KERNEL_HALF);
    sys::exit()
}

sys::entry!(main);
