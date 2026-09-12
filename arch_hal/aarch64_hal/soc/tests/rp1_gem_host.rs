//! Run with rustc --edition=2024 --test; no MMIO or cross-target dependencies.
//! The only host stand-in is MacAddr; the production polling code is included.
#![allow(dead_code)]
extern crate self as io_api;
pub mod ethernet {
    pub struct MacAddr(pub [u8; 6]);
}
#[path = "../src/bcm2712/rp1_gem.rs"]
mod rp1_gem;
