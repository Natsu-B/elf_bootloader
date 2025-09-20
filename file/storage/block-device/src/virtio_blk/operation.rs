use typestate::Le;
use typestate_macro::RawReg;

#[allow(dead_code)]
pub(crate) struct VirtioBlkReq {
    pub(crate) reg_type: Le<VirtioBlkReqType>,
    pub(crate) reserved: Le<u32>,
    pub(crate) sector: Le<u64>,
    // data: [u8; NUM_OF_BUF_LEN],
    // status: VirtioBlkReqStatus
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, RawReg)]
pub(crate) struct VirtioBlkReqType(u32);

impl VirtioBlkReqType {
    #![allow(unused)]
    pub(crate) const VIRTIO_BLK_T_IN: Self = Self(0);
    pub(crate) const VIRTIO_BLK_T_OUT: Self = Self(1);
    pub(crate) const VIRTIO_BLK_T_FLUSH: Self = Self(4);
    pub(crate) const VIRTIO_BLK_T_GET_ID: Self = Self(8);
    pub(crate) const VIRTIO_BLK_T_GET_LIFETIME: Self = Self(10);
    pub(crate) const VIRTIO_BLK_T_DISCARD: Self = Self(11);
    pub(crate) const VIRTIO_BLK_T_WRITE_ZEROES: Self = Self(13);
    pub(crate) const VIRTIO_BLK_T_SECURE_ERASE: Self = Self(14);
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, RawReg, PartialEq)]
pub(crate) struct VirtioBlkReqStatus(u8);

impl VirtioBlkReqStatus {
    #![allow(unused)]
    pub(crate) const VIRTIO_BLK_S_OK: Self = Self(0);
    pub(crate) const VIRTIO_BLK_S_IOERR: Self = Self(1);
    pub(crate) const VIRTIO_BLK_S_UNSUPP: Self = Self(2);
    pub(crate) const VIRTIO_BLK_S_RESERVED: Self = Self(0xFF);
}
