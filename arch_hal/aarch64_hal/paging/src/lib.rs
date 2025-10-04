#![no_std]
#![recursion_limit = "1024"]

//! TODO
//! Stage 2 Pagingをとりあえず作成する
//! とりあえずメモリサイズ48bit、4KiB pagingで大きなサイズの対応は無し
//! 
//! memo:
//! - VTCR_EL2 virtualization translation control register
//!     - 

use alloc::boxed::Box;

extern crate alloc;

mod descriptor;
mod registers;

pub struct Stage2Paging {
    before: Box<[Stage2PagingSetting]>,
}

pub struct Stage2PagingSetting {
    pub ipa: usize,
    pub pa: usize,
    pub size: usize,
    pub permissions: u8,
}

impl Stage2Paging {
    pub fn activate(&self) {
        todo!()
    }

    /// # Safety
    ///     dataは必ず昇順
    pub fn set_stage2paging(
        data: &[Stage2PagingSetting],
    ) -> Result<Self, PagingErr> {
        let data = Box::from(data);
        todo!()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingErr {

}
