use crate::VirtioErr;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtIoDeviceTypes {
    /// 0: reserved (invalid)
    ReservedInvalid = 0,
    /// 1: network device
    NetworkDevice = 1,
    /// 2: block device
    BlockDevice = 2,
    /// 3: console
    Console = 3,
    /// 4: entropy source
    EntropySource = 4,
    /// 5: memory ballooning (traditional)
    MemoryBalloonTraditional = 5,
    /// 6: ioMemory
    IoMemory = 6,
    /// 7: rpmsg
    Rpmsg = 7,
    /// 8: SCSI host
    ScsiHost = 8,
    /// 9: 9P transport
    V9PTransport = 9,
    /// 10: mac80211 wlan
    Mac80211Wlan = 10,
    /// 11: rproc serial
    RprocSerial = 11,
    /// 12: virtio CAIF
    VirtioCaif = 12,
    /// 13: memory balloon
    MemoryBalloon = 13,
    /// 16: GPU device
    GpuDevice = 16,
    /// 17: Timer/Clock device
    TimerClockDevice = 17,
    /// 18: Input device
    InputDevice = 18,
    /// 19: Socket device
    SocketDevice = 19,
    /// 20: Crypto device
    CryptoDevice = 20,
    /// 21: Signal Distribution Module
    SignalDistributionModule = 21,
    /// 22: pstore device
    PstoreDevice = 22,
    /// 23: IOMMU device
    IommuDevice = 23,
    /// 24: Memory device
    MemoryDevice = 24,
    /// 25: Sound device
    SoundDevice = 25,
    /// 26: file system device
    FileSystemDevice = 26,
    /// 27: PMEM device
    PmemDevice = 27,
    /// 28: RPMB device
    RpmbDevice = 28,
    /// 29: mac80211 hwsim wireless simulation device
    Mac80211HwsimWirelessSimulationDevice = 29,
    /// 30: Video encoder device
    VideoEncoderDevice = 30,
    /// 31: Video decoder device
    VideoDecoderDevice = 31,
    /// 32: SCMI device
    ScmiDevice = 32,
    /// 33: NitroSecureModule
    NitroSecureModule = 33,
    /// 34: I2C adapter
    I2cAdapter = 34,
    /// 35: Watchdog
    Watchdog = 35,
    /// 36: CAN device
    CanDevice = 36,
    /// 38: Parameter Server
    ParameterServer = 38,
    /// 39: Audio policy device
    AudioPolicyDevice = 39,
    /// 40: Bluetooth device
    BluetoothDevice = 40,
    /// 41: GPIO device
    GpioDevice = 41,
    /// 42: RDMA device
    RdmaDevice = 42,
    /// 43: Camera device
    CameraDevice = 43,
    /// 44: ISM device
    IsmDevice = 44,
    /// 45: SPI master
    SpiMaster = 45,
}

impl TryFrom<u32> for VirtIoDeviceTypes {
    type Error = VirtioErr;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        let v = match value {
            0 => Self::ReservedInvalid,
            1 => Self::NetworkDevice,
            2 => Self::BlockDevice,
            3 => Self::Console,
            4 => Self::EntropySource,
            5 => Self::MemoryBalloonTraditional,
            6 => Self::IoMemory,
            7 => Self::Rpmsg,
            8 => Self::ScsiHost,
            9 => Self::V9PTransport,
            10 => Self::Mac80211Wlan,
            11 => Self::RprocSerial,
            12 => Self::VirtioCaif,
            13 => Self::MemoryBalloon,
            16 => Self::GpuDevice,
            17 => Self::TimerClockDevice,
            18 => Self::InputDevice,
            19 => Self::SocketDevice,
            20 => Self::CryptoDevice,
            21 => Self::SignalDistributionModule,
            22 => Self::PstoreDevice,
            23 => Self::IommuDevice,
            24 => Self::MemoryDevice,
            25 => Self::SoundDevice,
            26 => Self::FileSystemDevice,
            27 => Self::PmemDevice,
            28 => Self::RpmbDevice,
            29 => Self::Mac80211HwsimWirelessSimulationDevice,
            30 => Self::VideoEncoderDevice,
            31 => Self::VideoDecoderDevice,
            32 => Self::ScmiDevice,
            33 => Self::NitroSecureModule,
            34 => Self::I2cAdapter,
            35 => Self::Watchdog,
            36 => Self::CanDevice,
            38 => Self::ParameterServer,
            39 => Self::AudioPolicyDevice,
            40 => Self::BluetoothDevice,
            41 => Self::GpioDevice,
            42 => Self::RdmaDevice,
            43 => Self::CameraDevice,
            44 => Self::IsmDevice,
            45 => Self::SpiMaster,
            _ => return Err(VirtioErr::UnknownVirtioDevice(value)),
        };
        Ok(v)
    }
}
