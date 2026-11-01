#![no_std]
#![no_main]
#![forbid(unsafe_code)]

//! The root task (#82, ADR 0019 decision 9): the first user program.

fn main() -> ! {
    sys::exit()
}

sys::entry!(main);
