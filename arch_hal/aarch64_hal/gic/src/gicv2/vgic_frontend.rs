use crate::GicError;
use crate::VIntId;
use crate::VSpiRouting;
use crate::VcpuId;
use crate::VcpuMask;
use crate::VgicIrqScope;
use crate::VgicUpdate;
use crate::VgicVmModel;
use crate::gicv2::registers::GICD_CTLR;
use crate::gicv2::registers::GICD_SGIR;
use crate::gicv2::registers::GICD_TYPER;
use crate::gicv2::registers::GicV2Distributor;
use crate::gicv2::registers::TargetListFilter;
use core::cmp;
use core::mem::offset_of;
use typestate::Readable;

const MAX_VIRTUAL_INTID: u32 = 1019;

// utility to get the size of a return value of a function
const fn size_of_return_value<F, T, U>(_f: &F) -> usize
where
    F: FnOnce(T) -> U,
{
    core::mem::size_of::<U>()
}

macro_rules! size_of_field {
    ($type:ty, $field:ident) => {
        size_of_return_value(&|s: $type| s.$field)
    };
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gicv2AccessSize {
    U8,
    U32,
}

impl Gicv2AccessSize {
    fn bytes(self) -> u32 {
        match self {
            Gicv2AccessSize::U8 => 1,
            Gicv2AccessSize::U32 => 4,
        }
    }
}

/// Snapshot of GICv2 Distributor ID registers used for vGIC emulation.
///
/// These registers are architecturally read-only and should not change while the GIC is running.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Gicv2DistIdRegs {
    typer: u32,
    iidr: u32,
    pidr: [u32; 8],
    cidr: [u32; 4],
}

impl Gicv2DistIdRegs {
    /// All-zero fallback (used when no hardware snapshot is available).
    pub const ZERO: Self = Self {
        typer: 0,
        iidr: 0,
        pidr: [0; 8],
        cidr: [0; 4],
    };

    /// Read Distributor ID registers from hardware.
    pub fn from_hw_gicd(gicd: &GicV2Distributor) -> Self {
        let typer = gicd.typer.read().bits();
        let iidr = gicd.iidr.read();

        let pidr = core::array::from_fn(|i| gicd.pidr[i].read());
        let cidr = core::array::from_fn(|i| gicd.cidr[i].read());

        Self {
            typer,
            iidr,
            pidr,
            cidr,
        }
    }
}

fn decode_sgir_targets(
    vcpu_count: u16,
    requester: VcpuId,
    filter: TargetListFilter,
    cpu_target_list: u16,
) -> Result<VcpuMask, GicError> {
    if vcpu_count == 0 || vcpu_count > 8 {
        return Err(GicError::InvalidVcpuId);
    }
    if requester.0 as u16 >= vcpu_count || requester.0 >= 8 {
        return Err(GicError::InvalidVcpuId);
    }
    let valid_mask = (1u16 << vcpu_count) - 1;
    let self_bit = 1u16 << requester.0;
    let masked_list = cpu_target_list & valid_mask;
    let mask = match filter {
        TargetListFilter::CpuTargetListFieldSpecified => masked_list,
        TargetListFilter::InterruptAllCpuExceptRequestedCpu => valid_mask & !self_bit,
        TargetListFilter::InterruptSelfOnly => self_bit,
    };
    Ok(VcpuMask::from_bits(mask))
}

pub(crate) struct Gicv2Frontend<'a, M: VgicVmModel> {
    vgic_model: &'a M,
    dist_id: Gicv2DistIdRegs,
}

impl<'a, M: VgicVmModel> Gicv2Frontend<'a, M> {
    pub(crate) fn new(vgic_model: &'a M, dist_id: Gicv2DistIdRegs) -> Self {
        Self {
            vgic_model,
            dist_id,
        }
    }

    #[inline]
    fn ensure_vcpu(&self, vcpu: VcpuId) -> Result<(), GicError> {
        // GICv2 architectural limit: up to 8 CPU interfaces.
        let count = self.vgic_model.vcpu_count();
        if count > 8 {
            return Err(GicError::UnsupportedFeature);
        }
        if vcpu.0 as u16 >= count {
            return Err(GicError::InvalidVcpuId);
        }
        Ok(())
    }

    #[inline]
    fn scope_for_intid(vcpu: VcpuId, intid: u32) -> Option<VgicIrqScope> {
        if intid > MAX_VIRTUAL_INTID {
            return None;
        }
        if intid < 32 {
            Some(VgicIrqScope::Local(vcpu)) // SGI/PPI banked
        } else {
            Some(VgicIrqScope::Global) // SPI global
        }
    }

    #[inline]
    fn vcpu_mask_to_u8(mask: &VcpuMask) -> u8 {
        let mut out = 0u8;
        for id in mask.iter() {
            let bit = id.0 as usize;
            if bit < 8 {
                out |= 1u8 << bit;
            }
        }
        out
    }

    #[inline]
    fn implemented_gicv2_target_mask(&self) -> Result<u8, GicError> {
        let count = self.vgic_model.vcpu_count();
        if count == 0 {
            return Err(GicError::InvalidVcpuId);
        }
        if count > 8 {
            return Err(GicError::UnsupportedFeature);
        }
        Ok(((1u16 << count) - 1) as u8)
    }

    #[inline]
    fn sanitize_gicv2_targets(&self, raw: u8) -> Result<VcpuMask, GicError> {
        Ok(VcpuMask::from_bits(
            (raw & self.implemented_gicv2_target_mask()?) as u16,
        ))
    }

    /// Handle a trapped write to the guest vGIC Distributor register.
    ///
    /// 'offset' is byte offset within Distributor, 'value' is zero-extended to u32.
    pub(crate) fn handle_distributor_write(
        &self,
        vcpu: VcpuId,
        offset: u32,
        size: Gicv2AccessSize,
        value: u32,
    ) -> Result<VgicUpdate, GicError> {
        self.ensure_vcpu(vcpu)?;

        const GICD_CTLR_OFFSET: u32 = offset_of!(GicV2Distributor, ctlr) as u32;
        const GICD_TYPER_OFFSET: u32 = offset_of!(GicV2Distributor, typer) as u32;
        const GICD_IIDR_OFFSET: u32 = offset_of!(GicV2Distributor, iidr) as u32;
        const GICD_IGROUPR_OFFSET: u32 = offset_of!(GicV2Distributor, igroupr) as u32;
        const GICD_ISENABLER_OFFSET: u32 = offset_of!(GicV2Distributor, isenabler) as u32;
        const GICD_ICENABLER_OFFSET: u32 = offset_of!(GicV2Distributor, icenabler) as u32;
        const GICD_ISPENDR_OFFSET: u32 = offset_of!(GicV2Distributor, ispendr) as u32;
        const GICD_ICPENDR_OFFSET: u32 = offset_of!(GicV2Distributor, icpendr) as u32;
        const GICD_ISACTIVER_OFFSET: u32 = offset_of!(GicV2Distributor, isactiver) as u32;
        const GICD_ICACTIVER_OFFSET: u32 = offset_of!(GicV2Distributor, icactiver) as u32;
        const GICD_IPRIORITYR_OFFSET: u32 = offset_of!(GicV2Distributor, ipriorityr) as u32;
        const GICD_ITARGETSR0_7_OFFSET: u32 = offset_of!(GicV2Distributor, itargetsr0_7) as u32;
        const GICD_ITARGETSR_OFFSET: u32 = offset_of!(GicV2Distributor, itargetsr) as u32;
        const GICD_ICFGR_OFFSET: u32 = offset_of!(GicV2Distributor, icfgr) as u32;
        const GICD_NSACR_OFFSET: u32 = offset_of!(GicV2Distributor, nsacr) as u32;
        const GICD_SGIR_OFFSET: u32 = offset_of!(GicV2Distributor, sgir) as u32;
        const GICD_CPENDSGIR_OFFSET: u32 = offset_of!(GicV2Distributor, cpendsgir) as u32;
        const GICD_SPENDSGIR_OFFSET: u32 = offset_of!(GicV2Distributor, spendsgir) as u32;
        const GICD_PIDR_OFFSET: u32 = offset_of!(GicV2Distributor, pidr) as u32;
        const GICD_CIDR_OFFSET: u32 = offset_of!(GicV2Distributor, cidr) as u32;

        const GICD_IGROUPR_END: u32 =
            GICD_IGROUPR_OFFSET + size_of_field!(GicV2Distributor, igroupr) as u32;
        const GICD_ISENABLER_END: u32 =
            GICD_ISENABLER_OFFSET + size_of_field!(GicV2Distributor, isenabler) as u32;
        const GICD_ICENABLER_END: u32 =
            GICD_ICENABLER_OFFSET + size_of_field!(GicV2Distributor, icenabler) as u32;
        const GICD_ISPENDR_END: u32 =
            GICD_ISPENDR_OFFSET + size_of_field!(GicV2Distributor, ispendr) as u32;
        const GICD_ICPENDR_END: u32 =
            GICD_ICPENDR_OFFSET + size_of_field!(GicV2Distributor, icpendr) as u32;
        const GICD_ISACTIVER_END: u32 =
            GICD_ISACTIVER_OFFSET + size_of_field!(GicV2Distributor, isactiver) as u32;
        const GICD_ICACTIVER_END: u32 =
            GICD_ICACTIVER_OFFSET + size_of_field!(GicV2Distributor, icactiver) as u32;
        const GICD_IPRIORITYR_END: u32 =
            GICD_IPRIORITYR_OFFSET + size_of_field!(GicV2Distributor, ipriorityr) as u32;
        const GICD_ITARGETSR0_7_END: u32 =
            GICD_ITARGETSR0_7_OFFSET + size_of_field!(GicV2Distributor, itargetsr0_7) as u32;
        const GICD_ITARGETSR_END: u32 =
            GICD_ITARGETSR_OFFSET + size_of_field!(GicV2Distributor, itargetsr) as u32;
        const GICD_ICFGR_END: u32 =
            GICD_ICFGR_OFFSET + size_of_field!(GicV2Distributor, icfgr) as u32;
        const GICD_NSACR_END: u32 =
            GICD_NSACR_OFFSET + size_of_field!(GicV2Distributor, nsacr) as u32;
        const GICD_SGIR_END: u32 = GICD_SGIR_OFFSET + size_of_field!(GicV2Distributor, sgir) as u32;
        const GICD_CPENDSGIR_END: u32 =
            GICD_CPENDSGIR_OFFSET + size_of_field!(GicV2Distributor, cpendsgir) as u32;
        const GICD_SPENDSGIR_END: u32 =
            GICD_SPENDSGIR_OFFSET + size_of_field!(GicV2Distributor, spendsgir) as u32;

        if !matches!(
            offset,
            GICD_IPRIORITYR_OFFSET..GICD_IPRIORITYR_END
                | GICD_ITARGETSR0_7_OFFSET..GICD_ITARGETSR_END
        ) && size != Gicv2AccessSize::U32
        {
            return Err(GicError::InvalidAccessSize);
        }

        if size == Gicv2AccessSize::U32 && !offset.is_multiple_of(4) {
            return Err(GicError::InvalidOffset);
        }

        let word_mask = |base_intid: u32, ints_per_word: u32| -> u32 {
            if base_intid > MAX_VIRTUAL_INTID {
                return 0;
            }
            let valid = cmp::min(ints_per_word, MAX_VIRTUAL_INTID - base_intid + 1);
            if valid >= 32 {
                0xffff_ffff
            } else {
                (1u32 << valid) - 1
            }
        };

        let priority_mask = |base_intid: u32| -> u32 {
            if base_intid > MAX_VIRTUAL_INTID {
                return 0;
            }
            let valid = cmp::min(4, MAX_VIRTUAL_INTID - base_intid + 1);
            if valid == 4 {
                0xffff_ffff
            } else {
                ((1u64 << (valid * 8)) - 1) as u32
            }
        };

        let icfgr_mask = |base_intid: u32| -> u32 {
            if base_intid > MAX_VIRTUAL_INTID {
                return 0;
            }
            let valid = cmp::min(16, MAX_VIRTUAL_INTID - base_intid + 1);
            if valid == 16 {
                0xffff_ffff
            } else {
                ((1u64 << (valid * 2)) - 1) as u32
            }
        };

        let update = match offset {
            GICD_CTLR_OFFSET => {
                let ctlr = GICD_CTLR::from_bits(value);
                let update = self.vgic_model.set_dist_enable(
                    ctlr.get(GICD_CTLR::enable_grp0) != 0,
                    ctlr.get(GICD_CTLR::enable_grp1) != 0,
                )?;
                update
            }
            GICD_TYPER_OFFSET | GICD_IIDR_OFFSET => {
                // Read-only
                return Err(GicError::ReadOnlyRegister);
            }
            GICD_IGROUPR_OFFSET..GICD_IGROUPR_END => self.vgic_model.write_group_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_IGROUPR_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_IGROUPR_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_IGROUPR_OFFSET) / 4) * 32, 32),
            )?,
            GICD_ISENABLER_OFFSET..GICD_ISENABLER_END => self.vgic_model.write_set_enable_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ISENABLER_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ISENABLER_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_ISENABLER_OFFSET) / 4) * 32, 32),
            )?,
            GICD_ICENABLER_OFFSET..GICD_ICENABLER_END => self.vgic_model.write_clear_enable_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ICENABLER_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ICENABLER_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_ICENABLER_OFFSET) / 4) * 32, 32),
            )?,
            GICD_ISPENDR_OFFSET..GICD_ISPENDR_END => self.vgic_model.write_set_pending_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ISPENDR_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ISPENDR_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_ISPENDR_OFFSET) / 4) * 32, 32),
            )?,
            GICD_ICPENDR_OFFSET..GICD_ICPENDR_END => self.vgic_model.write_clear_pending_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ICPENDR_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ICPENDR_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_ICPENDR_OFFSET) / 4) * 32, 32),
            )?,
            GICD_ISACTIVER_OFFSET..GICD_ISACTIVER_END => self.vgic_model.write_set_active_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ISACTIVER_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ISACTIVER_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_ISACTIVER_OFFSET) / 4) * 32, 32),
            )?,
            GICD_ICACTIVER_OFFSET..GICD_ICACTIVER_END => self.vgic_model.write_clear_active_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ICACTIVER_OFFSET) / 4) * 32)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ICACTIVER_OFFSET) / 4) * 32),
                value & word_mask(((offset - GICD_ICACTIVER_OFFSET) / 4) * 32, 32),
            )?,
            GICD_IPRIORITYR_OFFSET..GICD_IPRIORITYR_END => match size {
                Gicv2AccessSize::U32 => {
                    let base_intid = ((offset - GICD_IPRIORITYR_OFFSET) / 4) * 4;
                    let mask = priority_mask(base_intid);
                    if mask == 0 {
                        VgicUpdate::None
                    } else {
                        self.vgic_model.write_priority_word(
                            Self::scope_for_intid(vcpu, base_intid)
                                .ok_or(GicError::UnsupportedIntId)?,
                            VIntId(base_intid),
                            value & mask & 0xF8F8_F8F8,
                        )?
                    }
                }
                Gicv2AccessSize::U8 => {
                    let byte_offset = offset - GICD_IPRIORITYR_OFFSET;
                    let base_intid = (byte_offset / 4) * 4;
                    let intid = base_intid + (byte_offset % 4);
                    if intid > MAX_VIRTUAL_INTID {
                        VgicUpdate::None
                    } else {
                        let scope =
                            Self::scope_for_intid(vcpu, intid).ok_or(GicError::UnsupportedIntId)?;
                        let lane_shift = (intid - base_intid) * 8;
                        let mut merged = self
                            .vgic_model
                            .read_priority_word(scope, VIntId(base_intid))?;
                        merged &= !(0xffu32 << lane_shift);
                        merged |= (((value as u8) & 0xF8) as u32) << lane_shift;
                        self.vgic_model
                            .write_priority_word(scope, VIntId(base_intid), merged)?
                    }
                }
            },
            GICD_ITARGETSR0_7_OFFSET..GICD_ITARGETSR0_7_END => VgicUpdate::None,
            GICD_ITARGETSR_OFFSET..GICD_ITARGETSR_END => {
                let byte_offset = offset - GICD_ITARGETSR_OFFSET;
                let base_intid = 32 + (byte_offset / 4) * 4;
                let _implemented_target_mask = self.implemented_gicv2_target_mask()?;
                match size {
                    Gicv2AccessSize::U32 => {
                        let mut update = VgicUpdate::None;
                        for i in 0..4u32 {
                            let intid = base_intid + i;
                            if intid < 32 || intid > MAX_VIRTUAL_INTID {
                                continue;
                            }
                            let targets =
                                self.sanitize_gicv2_targets(((value >> (i * 8)) & 0xff) as u8)?;
                            let u = self
                                .vgic_model
                                .set_spi_route(VIntId(intid), VSpiRouting::Targets(targets))?;
                            update.combine(&u);
                        }
                        update
                    }
                    Gicv2AccessSize::U8 => {
                        let intid = base_intid + (byte_offset % 4);
                        if intid < 32 || intid > MAX_VIRTUAL_INTID {
                            VgicUpdate::None
                        } else {
                            let targets = self.sanitize_gicv2_targets(value as u8)?;
                            self.vgic_model
                                .set_spi_route(VIntId(intid), VSpiRouting::Targets(targets))?
                        }
                    }
                }
            }
            GICD_ICFGR_OFFSET..GICD_ICFGR_END => self.vgic_model.write_trigger_word(
                Self::scope_for_intid(vcpu, ((offset - GICD_ICFGR_OFFSET) / 4) * 16)
                    .ok_or(GicError::UnsupportedIntId)?,
                VIntId(((offset - GICD_ICFGR_OFFSET) / 4) * 16),
                value & icfgr_mask(((offset - GICD_ICFGR_OFFSET) / 4) * 16),
            )?,
            GICD_NSACR_OFFSET..GICD_NSACR_END => VgicUpdate::None,
            GICD_SGIR_OFFSET..GICD_SGIR_END => {
                let sgir = GICD_SGIR::from_bits(value);
                let sgi = sgir.get(GICD_SGIR::sgi_int_id) as u8;
                let list = sgir.get(GICD_SGIR::cpu_target_list) as u16;

                let filter = sgir
                    .get_enum(GICD_SGIR::target_list_filter)
                    .ok_or(GicError::InvalidOffset)?;
                let targets =
                    decode_sgir_targets(self.vgic_model.vcpu_count(), vcpu, filter, list)?;
                self.vgic_model.inject_sgi(vcpu, targets, sgi)?
            }
            GICD_CPENDSGIR_OFFSET..GICD_CPENDSGIR_END => {
                self.vgic_model.write_clear_sgi_pending_sources_word(
                    vcpu,
                    ((offset - GICD_CPENDSGIR_OFFSET) / 4) as u8,
                    value,
                )?
            }
            GICD_SPENDSGIR_OFFSET..GICD_SPENDSGIR_END => {
                self.vgic_model.write_set_sgi_pending_sources_word(
                    vcpu,
                    ((offset - GICD_SPENDSGIR_OFFSET) / 4) as u8,
                    value,
                )?
            }
            GICD_PIDR_OFFSET | GICD_CIDR_OFFSET => {
                // Read-only
                return Err(GicError::ReadOnlyRegister);
            }
            _ => return Err(GicError::InvalidOffset),
        };
        Ok(update)
    }

    /// Handle a trapped read from the guest vGIC Distributor register.
    ///
    /// 'offset' is byte offset within Distributor.
    pub(crate) fn handle_distributor_read(
        &self,
        vcpu: VcpuId,
        offset: u32,
        size: Gicv2AccessSize,
    ) -> Result<u32, GicError> {
        self.ensure_vcpu(vcpu)?;

        const GICD_CTLR_OFFSET: u32 = offset_of!(GicV2Distributor, ctlr) as u32;
        const GICD_TYPER_OFFSET: u32 = offset_of!(GicV2Distributor, typer) as u32;
        const GICD_IIDR_OFFSET: u32 = offset_of!(GicV2Distributor, iidr) as u32;
        const GICD_IGROUPR_OFFSET: u32 = offset_of!(GicV2Distributor, igroupr) as u32;
        const GICD_ISENABLER_OFFSET: u32 = offset_of!(GicV2Distributor, isenabler) as u32;
        const GICD_ICENABLER_OFFSET: u32 = offset_of!(GicV2Distributor, icenabler) as u32;
        const GICD_ISPENDR_OFFSET: u32 = offset_of!(GicV2Distributor, ispendr) as u32;
        const GICD_ICPENDR_OFFSET: u32 = offset_of!(GicV2Distributor, icpendr) as u32;
        const GICD_ISACTIVER_OFFSET: u32 = offset_of!(GicV2Distributor, isactiver) as u32;
        const GICD_ICACTIVER_OFFSET: u32 = offset_of!(GicV2Distributor, icactiver) as u32;
        const GICD_IPRIORITYR_OFFSET: u32 = offset_of!(GicV2Distributor, ipriorityr) as u32;
        const GICD_ITARGETSR0_7_OFFSET: u32 = offset_of!(GicV2Distributor, itargetsr0_7) as u32;
        const GICD_ITARGETSR_OFFSET: u32 = offset_of!(GicV2Distributor, itargetsr) as u32;
        const GICD_ICFGR_OFFSET: u32 = offset_of!(GicV2Distributor, icfgr) as u32;
        const GICD_NSACR_OFFSET: u32 = offset_of!(GicV2Distributor, nsacr) as u32;
        const GICD_SGIR_OFFSET: u32 = offset_of!(GicV2Distributor, sgir) as u32;
        const GICD_CPENDSGIR_OFFSET: u32 = offset_of!(GicV2Distributor, cpendsgir) as u32;
        const GICD_SPENDSGIR_OFFSET: u32 = offset_of!(GicV2Distributor, spendsgir) as u32;
        const GICD_PIDR_OFFSET: u32 = offset_of!(GicV2Distributor, pidr) as u32;
        const GICD_CIDR_OFFSET: u32 = offset_of!(GicV2Distributor, cidr) as u32;

        const GICD_CTLR_END: u32 = GICD_CTLR_OFFSET + size_of_field!(GicV2Distributor, ctlr) as u32;
        const GICD_TYPER_END: u32 =
            GICD_TYPER_OFFSET + size_of_field!(GicV2Distributor, typer) as u32;
        const GICD_IIDR_END: u32 = GICD_IIDR_OFFSET + size_of_field!(GicV2Distributor, iidr) as u32;
        const GICD_IGROUPR_END: u32 =
            GICD_IGROUPR_OFFSET + size_of_field!(GicV2Distributor, igroupr) as u32;
        const GICD_ISENABLER_END: u32 =
            GICD_ISENABLER_OFFSET + size_of_field!(GicV2Distributor, isenabler) as u32;
        const GICD_ICENABLER_END: u32 =
            GICD_ICENABLER_OFFSET + size_of_field!(GicV2Distributor, icenabler) as u32;
        const GICD_ISPENDR_END: u32 =
            GICD_ISPENDR_OFFSET + size_of_field!(GicV2Distributor, ispendr) as u32;
        const GICD_ICPENDR_END: u32 =
            GICD_ICPENDR_OFFSET + size_of_field!(GicV2Distributor, icpendr) as u32;
        const GICD_ISACTIVER_END: u32 =
            GICD_ISACTIVER_OFFSET + size_of_field!(GicV2Distributor, isactiver) as u32;
        const GICD_ICACTIVER_END: u32 =
            GICD_ICACTIVER_OFFSET + size_of_field!(GicV2Distributor, icactiver) as u32;
        const GICD_IPRIORITYR_END: u32 =
            GICD_IPRIORITYR_OFFSET + size_of_field!(GicV2Distributor, ipriorityr) as u32;
        const GICD_ITARGETSR0_7_END: u32 =
            GICD_ITARGETSR0_7_OFFSET + size_of_field!(GicV2Distributor, itargetsr0_7) as u32;
        const GICD_ITARGETSR_END: u32 =
            GICD_ITARGETSR_OFFSET + size_of_field!(GicV2Distributor, itargetsr) as u32;
        const GICD_ICFGR_END: u32 =
            GICD_ICFGR_OFFSET + size_of_field!(GicV2Distributor, icfgr) as u32;
        const GICD_NSACR_END: u32 =
            GICD_NSACR_OFFSET + size_of_field!(GicV2Distributor, nsacr) as u32;
        const GICD_SGIR_END: u32 = GICD_SGIR_OFFSET + size_of_field!(GicV2Distributor, sgir) as u32;
        const GICD_CPENDSGIR_END: u32 =
            GICD_CPENDSGIR_OFFSET + size_of_field!(GicV2Distributor, cpendsgir) as u32;
        const GICD_SPENDSGIR_END: u32 =
            GICD_SPENDSGIR_OFFSET + size_of_field!(GicV2Distributor, spendsgir) as u32;
        const GICD_PIDR_END: u32 = GICD_PIDR_OFFSET + size_of_field!(GicV2Distributor, pidr) as u32;
        const GICD_CIDR_END: u32 = GICD_CIDR_OFFSET + size_of_field!(GicV2Distributor, cidr) as u32;

        let subword_ok = matches!(
            offset,
            GICD_CTLR_OFFSET..GICD_CTLR_END
                | GICD_TYPER_OFFSET..GICD_TYPER_END
                | GICD_IIDR_OFFSET..GICD_IIDR_END
                | GICD_IPRIORITYR_OFFSET..GICD_IPRIORITYR_END
                | GICD_ITARGETSR0_7_OFFSET..GICD_ITARGETSR_END
                | GICD_PIDR_OFFSET..GICD_PIDR_END
                | GICD_CIDR_OFFSET..GICD_CIDR_END
        );

        if !subword_ok && size != Gicv2AccessSize::U32 {
            return Err(GicError::InvalidAccessSize);
        }

        if size == Gicv2AccessSize::U32 && !offset.is_multiple_of(4) {
            return Err(GicError::InvalidOffset);
        }

        let word_mask = |base_intid: u32, ints_per_word: u32| -> u32 {
            if base_intid > MAX_VIRTUAL_INTID {
                return 0;
            }
            let valid = cmp::min(ints_per_word, MAX_VIRTUAL_INTID - base_intid + 1);
            if valid >= 32 {
                0xffff_ffff
            } else {
                (1u32 << valid) - 1
            }
        };

        let priority_mask = |base_intid: u32| -> u32 {
            if base_intid > MAX_VIRTUAL_INTID {
                return 0;
            }
            let valid = cmp::min(4, MAX_VIRTUAL_INTID - base_intid + 1);
            if valid == 4 {
                0xffff_ffff
            } else {
                ((1u64 << (valid * 8)) - 1) as u32
            }
        };

        let icfgr_mask = |base_intid: u32| -> u32 {
            if base_intid > MAX_VIRTUAL_INTID {
                return 0;
            }
            let valid = cmp::min(16, MAX_VIRTUAL_INTID - base_intid + 1);
            if valid == 16 {
                0xffff_ffff
            } else {
                ((1u64 << (valid * 2)) - 1) as u32
            }
        };

        let slice_reg = |val: u32, reg_offset: u32| -> Result<u32, GicError> {
            if reg_offset + size.bytes() > 4 {
                return Err(GicError::InvalidAccessSize);
            }
            Ok(match size {
                Gicv2AccessSize::U32 => val,
                Gicv2AccessSize::U8 => (val >> (reg_offset * 8)) & 0xff,
            })
        };

        fn read_byte_window<F>(
            size: Gicv2AccessSize,
            make_word: &mut F,
            word_index: u32,
            lane: u32,
        ) -> Result<u32, GicError>
        where
            F: FnMut(u32) -> Result<u32, GicError>,
        {
            let word = make_word(word_index)?;
            match size {
                Gicv2AccessSize::U32 => Ok(word),
                Gicv2AccessSize::U8 => Ok(((word >> (lane * 8)) & 0xff) as u32),
            }
        }

        let value = match offset {
            GICD_CTLR_OFFSET..GICD_CTLR_END => {
                let (grp0, grp1) = self.vgic_model.dist_enable()?;
                let ctlr = GICD_CTLR::new()
                    .set(GICD_CTLR::enable_grp0, grp0 as u32)
                    .set(GICD_CTLR::enable_grp1, grp1 as u32);
                slice_reg(ctlr.bits(), offset - GICD_CTLR_OFFSET)?
            }
            GICD_TYPER_OFFSET..GICD_TYPER_END => {
                let mut typer = GICD_TYPER::from_bits(self.dist_id.typer);
                let it_lines_number = ((MAX_VIRTUAL_INTID + 1 + 31) / 32).saturating_sub(1) & 0x1f;
                let cpus = cmp::min(self.vgic_model.vcpu_count(), 8).saturating_sub(1) as u32 & 0x7;
                typer = typer
                    .set(GICD_TYPER::it_lines_number, it_lines_number)
                    .set(GICD_TYPER::cpu_number, cpus)
                    .set(GICD_TYPER::security_extn, 0)
                    .set(GICD_TYPER::lspi, 0);
                slice_reg(typer.bits(), offset - GICD_TYPER_OFFSET)?
            }
            GICD_IIDR_OFFSET..GICD_IIDR_END => {
                let iidr = self.dist_id.iidr;
                slice_reg(iidr, offset - GICD_IIDR_OFFSET)?
            }
            GICD_IGROUPR_OFFSET..GICD_IGROUPR_END => {
                let base_intid = ((offset - GICD_IGROUPR_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model.read_group_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_ISENABLER_OFFSET..GICD_ISENABLER_END => {
                let base_intid = ((offset - GICD_ISENABLER_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_enable_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_ICENABLER_OFFSET..GICD_ICENABLER_END => {
                let base_intid = ((offset - GICD_ICENABLER_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_enable_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_ISPENDR_OFFSET..GICD_ISPENDR_END => {
                let base_intid = ((offset - GICD_ISPENDR_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_pending_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_ICPENDR_OFFSET..GICD_ICPENDR_END => {
                let base_intid = ((offset - GICD_ICPENDR_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_pending_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_ISACTIVER_OFFSET..GICD_ISACTIVER_END => {
                let base_intid = ((offset - GICD_ISACTIVER_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_active_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_ICACTIVER_OFFSET..GICD_ICACTIVER_END => {
                let base_intid = ((offset - GICD_ICACTIVER_OFFSET) / 4) * 32;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_active_word(scope, VIntId(base_intid))?
                    & word_mask(base_intid, 32)
            }
            GICD_IPRIORITYR_OFFSET..GICD_IPRIORITYR_END => {
                let byte_offset = offset - GICD_IPRIORITYR_OFFSET;
                let word_index = byte_offset / 4;
                let lane = byte_offset % 4;
                let mut make_word = |word_index: u32| -> Result<u32, GicError> {
                    let base_intid = word_index * 4;
                    if base_intid > MAX_VIRTUAL_INTID {
                        return Ok(0);
                    }
                    let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                        return Ok(0);
                    };
                    let w = self
                        .vgic_model
                        .read_priority_word(scope, VIntId(base_intid))?
                        & priority_mask(base_intid);
                    Ok(w & 0xF8F8_F8F8)
                };
                read_byte_window(size, &mut make_word, word_index, lane)?
            }
            GICD_ITARGETSR0_7_OFFSET..GICD_ITARGETSR0_7_END => {
                let byte_offset = offset - GICD_ITARGETSR0_7_OFFSET;
                let word_index = byte_offset / 4;
                let lane = byte_offset % 4;
                // GICv2 ITARGETSR0_7 is banked per-CPU interface and only encodes 8 interfaces.
                // vCPU ids must therefore be < 8 and match the CPU interface id.
                if vcpu.0 >= 8 {
                    return Err(GicError::InvalidVcpuId);
                }
                let cpu_mask = 1u8 << (vcpu.0 as u8);
                let mut make_word = |idx: u32| -> Result<u32, GicError> {
                    if idx >= 8 {
                        return Ok(0);
                    }
                    Ok(u32::from_le_bytes([cpu_mask; 4]))
                };
                read_byte_window(size, &mut make_word, word_index, lane)?
            }
            GICD_ITARGETSR_OFFSET..GICD_ITARGETSR_END => {
                let byte_offset = offset - GICD_ITARGETSR_OFFSET;
                let word_index = byte_offset / 4;
                let lane = byte_offset % 4;
                let mut make_word = |word_index: u32| -> Result<u32, GicError> {
                    let base_intid = 32 + word_index * 4;
                    if base_intid > MAX_VIRTUAL_INTID {
                        return Ok(0);
                    }
                    let mut bytes = [0u8; 4];
                    for i in 0..4u32 {
                        let intid = base_intid + i;
                        if intid > MAX_VIRTUAL_INTID {
                            continue;
                        }
                        let route = self.vgic_model.get_spi_route(VIntId(intid))?;
                        bytes[i as usize] = match route {
                            VSpiRouting::Targets(mask) => Self::vcpu_mask_to_u8(&mask),
                            _ => 0,
                        };
                    }
                    Ok(u32::from_le_bytes(bytes))
                };
                read_byte_window(size, &mut make_word, word_index, lane)?
            }
            GICD_ICFGR_OFFSET..GICD_ICFGR_END => {
                let base_intid = ((offset - GICD_ICFGR_OFFSET) / 4) * 16;
                let Some(scope) = Self::scope_for_intid(vcpu, base_intid) else {
                    return Ok(0);
                };
                self.vgic_model
                    .read_trigger_word(scope, VIntId(base_intid))?
                    & icfgr_mask(base_intid)
            }
            GICD_NSACR_OFFSET..GICD_NSACR_END => 0,
            GICD_SGIR_OFFSET..GICD_SGIR_END => 0,
            GICD_CPENDSGIR_OFFSET..GICD_CPENDSGIR_END => {
                let word_index = (offset - GICD_CPENDSGIR_OFFSET) / 4;
                self.vgic_model
                    .read_sgi_pending_sources_word(vcpu, word_index as u8)?
            }
            GICD_SPENDSGIR_OFFSET..GICD_SPENDSGIR_END => {
                let word_index = (offset - GICD_SPENDSGIR_OFFSET) / 4;
                self.vgic_model
                    .read_sgi_pending_sources_word(vcpu, word_index as u8)?
            }
            GICD_PIDR_OFFSET..GICD_PIDR_END => {
                let reg = (offset - GICD_PIDR_OFFSET) / 4;
                if reg >= 8 {
                    return Err(GicError::InvalidOffset);
                }
                let reg_offset = (offset - GICD_PIDR_OFFSET) % 4;
                let pidr = self.dist_id.pidr[reg as usize];
                slice_reg(pidr, reg_offset)?
            }
            GICD_CIDR_OFFSET..GICD_CIDR_END => {
                let reg = (offset - GICD_CIDR_OFFSET) / 4;
                if reg >= 4 {
                    return Err(GicError::InvalidOffset);
                }
                let reg_offset = (offset - GICD_CIDR_OFFSET) % 4;
                let cidr = self.dist_id.cidr[reg as usize];
                slice_reg(cidr, reg_offset)?
            }
            _ => return Err(GicError::InvalidOffset),
        };

        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IrqGroup;
    use crate::IrqSense;
    use crate::PIntId;
    use crate::PirqNotifications;
    use crate::TriggerMode;
    use crate::VgicGuestRegs;
    use crate::VgicPirqModel;
    use crate::VgicSgiRegs;
    use crate::VgicVcpuModel;
    use crate::VgicVmInfo;
    use core::cell::Cell;

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgir_cpu_target_masks_nonexistent_vcpus() {
        let mask = decode_sgir_targets(
            2,
            VcpuId(0),
            TargetListFilter::CpuTargetListFieldSpecified,
            0b1111,
        )
        .unwrap();
        assert_eq!(mask, VcpuMask::from_bits(0b0011));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgir_all_but_self_excludes_requester() {
        let mask = decode_sgir_targets(
            3,
            VcpuId(1),
            TargetListFilter::InterruptAllCpuExceptRequestedCpu,
            0xffff,
        )
        .unwrap();
        assert_eq!(mask, VcpuMask::from_bits(0b0101));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgir_self_only_targets_requester() {
        let mask =
            decode_sgir_targets(8, VcpuId(3), TargetListFilter::InterruptSelfOnly, 0xffff).unwrap();
        assert_eq!(mask, VcpuMask::from_bits(0b1000));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgir_invalid_requester_rejected() {
        assert!(matches!(
            decode_sgir_targets(4, VcpuId(16), TargetListFilter::InterruptSelfOnly, 0),
            Err(GicError::InvalidVcpuId)
        ));
    }

    struct DummyVcpu;

    impl VgicVcpuModel for DummyVcpu {
        fn set_resident(&self, _core: cpu::CoreAffinity) -> Result<(), GicError> {
            Ok(())
        }

        fn refill_lrs<H: crate::VgicHw>(&self, _hw: &H) -> Result<bool, GicError> {
            Ok(false)
        }

        fn handle_maintenance_collect<H: crate::VgicHw>(
            &self,
            _hw: &H,
        ) -> Result<(VgicUpdate, PirqNotifications), GicError> {
            Ok((VgicUpdate::None, PirqNotifications::new()))
        }

        fn switch_out_sync<H: crate::VgicHw>(&self, _hw: &H) -> Result<(), GicError> {
            Ok(())
        }
    }

    struct FakeModel {
        dist_enable: Cell<(bool, bool)>,
        last_sgi: Cell<Option<(VcpuId, VcpuMask, u8)>>,
        last_spi_route: Cell<Option<(VIntId, VcpuMask)>>,
        vcpu_count: u16,
        vcpu: DummyVcpu,
    }

    impl FakeModel {
        fn new(vcpu_count: u16) -> Self {
            Self {
                dist_enable: Cell::new((false, false)),
                last_sgi: Cell::new(None),
                last_spi_route: Cell::new(None),
                vcpu_count,
                vcpu: DummyVcpu,
            }
        }
    }

    impl VgicVmInfo for FakeModel {
        type VcpuModel = DummyVcpu;

        fn vcpu_count(&self) -> u16 {
            self.vcpu_count
        }

        fn vcpu(&self, id: VcpuId) -> Result<&Self::VcpuModel, GicError> {
            if (id.0 as u16) < self.vcpu_count {
                Ok(&self.vcpu)
            } else {
                Err(GicError::InvalidVcpuId)
            }
        }
    }

    impl VgicGuestRegs for FakeModel {
        fn set_dist_enable(
            &self,
            enable_grp0: bool,
            enable_grp1: bool,
        ) -> Result<VgicUpdate, GicError> {
            self.dist_enable.set((enable_grp0, enable_grp1));
            Ok(VgicUpdate::None)
        }

        fn dist_enable(&self) -> Result<(bool, bool), GicError> {
            Ok(self.dist_enable.get())
        }

        fn set_group(
            &self,
            _scope: VgicIrqScope,
            _vintid: VIntId,
            _group: IrqGroup,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn set_priority(
            &self,
            _scope: VgicIrqScope,
            _vintid: VIntId,
            _priority: u8,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn set_trigger(
            &self,
            _scope: VgicIrqScope,
            _vintid: VIntId,
            _trigger: TriggerMode,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn set_enable(
            &self,
            _scope: VgicIrqScope,
            _vintid: VIntId,
            _enable: bool,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn set_pending(
            &self,
            _scope: VgicIrqScope,
            _vintid: VIntId,
            _pending: bool,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn read_group_word(&self, _scope: VgicIrqScope, _base: VIntId) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_group_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _value: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn read_enable_word(&self, _scope: VgicIrqScope, _base: VIntId) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_set_enable_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _set_bits: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn write_clear_enable_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _clear_bits: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn read_pending_word(&self, _scope: VgicIrqScope, _base: VIntId) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_set_pending_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _set_bits: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn write_clear_pending_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _clear_bits: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn read_active_word(&self, _scope: VgicIrqScope, _base: VIntId) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_set_active_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _set_bits: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn write_clear_active_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _clear_bits: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn read_priority_word(&self, _scope: VgicIrqScope, _base: VIntId) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_priority_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _value: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn read_trigger_word(&self, _scope: VgicIrqScope, _base: VIntId) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_trigger_word(
            &self,
            _scope: VgicIrqScope,
            _base: VIntId,
            _value: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn set_spi_route(
            &self,
            vintid: VIntId,
            targets: VSpiRouting,
        ) -> Result<VgicUpdate, GicError> {
            if let VSpiRouting::Targets(mask) = targets {
                self.last_spi_route.set(Some((vintid, mask)));
            }
            Ok(VgicUpdate::None)
        }

        fn get_spi_route(&self, _vintid: VIntId) -> Result<VSpiRouting, GicError> {
            Ok(VSpiRouting::Targets(VcpuMask::EMPTY))
        }
    }

    impl VgicSgiRegs for FakeModel {
        fn read_sgi_pending_sources_word(
            &self,
            _target: VcpuId,
            _sgi: u8,
        ) -> Result<u32, GicError> {
            Ok(0)
        }

        fn write_set_sgi_pending_sources_word(
            &self,
            _target: VcpuId,
            _word: u8,
            _sources: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn write_clear_sgi_pending_sources_word(
            &self,
            _target: VcpuId,
            _word: u8,
            _sources: u32,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn inject_sgi(
            &self,
            sender: VcpuId,
            targets: VcpuMask,
            sgi: u8,
        ) -> Result<VgicUpdate, GicError> {
            self.last_sgi.set(Some((sender, targets, sgi)));
            Ok(VgicUpdate::None)
        }
    }

    impl VgicPirqModel for FakeModel {
        fn map_pirq(
            &self,
            _pintid: PIntId,
            _target: VcpuId,
            _vintid: VIntId,
            _sense: IrqSense,
            _group: IrqGroup,
            _priority: u8,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn bind_local_pirq_passthrough(
            &self,
            _pintid: PIntId,
            _target: VcpuId,
            _vintid: VIntId,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn bind_local_pirq_write_through_software_lr(
            &self,
            _pintid: PIntId,
            _target: VcpuId,
            _vintid: VIntId,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn bind_spi_pirq_passthrough(
            &self,
            _pintid: PIntId,
            _vintid: VIntId,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn bind_spi_pirq_write_through_software_lr(
            &self,
            _pintid: PIntId,
            _vintid: VIntId,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn bind_spi_pirq_shadow_software_lr(
            &self,
            _pintid: PIntId,
            _vintid: VIntId,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn unmap_pirq(&self, _pintid: PIntId) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn on_physical_irq(
            &self,
            _source_vcpu: VcpuId,
            _pintid: PIntId,
            _level: bool,
        ) -> Result<VgicUpdate, GicError> {
            Ok(VgicUpdate::None)
        }

        fn physical_irq_binding_kind(
            &self,
            _source_vcpu: VcpuId,
            _pintid: PIntId,
        ) -> Result<Option<crate::PhysicalIrqBindingKind>, GicError> {
            Ok(None)
        }

        fn physical_irq_guest_state(
            &self,
            _source_vcpu: VcpuId,
            _pintid: PIntId,
        ) -> Result<Option<crate::PhysicalIrqGuestState>, GicError> {
            Ok(None)
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn ctlr_write_and_read_roundtrip() {
        let model = FakeModel::new(2);
        let read_back = {
            let frontend = Gicv2Frontend::new(&model, Gicv2DistIdRegs::ZERO);
            let offset = offset_of!(GicV2Distributor, ctlr) as u32;
            let value = GICD_CTLR::new()
                .set(GICD_CTLR::enable_grp0, 1)
                .set(GICD_CTLR::enable_grp1, 1)
                .bits();
            frontend
                .handle_distributor_write(VcpuId(0), offset, Gicv2AccessSize::U32, value)
                .unwrap();
            frontend
                .handle_distributor_read(VcpuId(0), offset, Gicv2AccessSize::U32)
                .unwrap()
        };
        assert_eq!(model.dist_enable.get(), (true, true));
        assert_eq!(read_back & 0x3, 0x3);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgir_write_injects_expected_targets() {
        let model = FakeModel::new(4);
        let frontend = Gicv2Frontend::new(&model, Gicv2DistIdRegs::ZERO);
        let offset = offset_of!(GicV2Distributor, sgir) as u32;
        let sgir = GICD_SGIR::new()
            .set(GICD_SGIR::sgi_int_id, 5)
            .set(GICD_SGIR::cpu_target_list, 0b0010)
            .set(
                GICD_SGIR::target_list_filter,
                TargetListFilter::CpuTargetListFieldSpecified as u32,
            );

        frontend
            .handle_distributor_write(VcpuId(0), offset, Gicv2AccessSize::U32, sgir.bits())
            .unwrap();

        let (sender, targets, sgi) = model.last_sgi.get().expect("SGI should be recorded");
        assert_eq!(sender, VcpuId(0));
        assert_eq!(targets, VcpuMask::from_bits(0b0010));
        assert_eq!(sgi, 5);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn itargetsr_u8_masks_nonexistent_vcpus() {
        let model = FakeModel::new(4);
        let frontend = Gicv2Frontend::new(&model, Gicv2DistIdRegs::ZERO);
        let offset = offset_of!(GicV2Distributor, itargetsr) as u32 + 1;

        frontend
            .handle_distributor_write(VcpuId(0), offset, Gicv2AccessSize::U8, 0xfe)
            .unwrap();

        let (vintid, targets) = model
            .last_spi_route
            .get()
            .expect("SPI route should be recorded");
        assert_eq!(vintid, VIntId(33));
        assert_eq!(targets, VcpuMask::from_bits(0x0e));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn itargetsr_u32_masks_nonexistent_vcpus_in_each_lane() {
        let model = FakeModel::new(4);
        let frontend = Gicv2Frontend::new(&model, Gicv2DistIdRegs::ZERO);
        let offset = offset_of!(GicV2Distributor, itargetsr) as u32;

        frontend
            .handle_distributor_write(VcpuId(0), offset, Gicv2AccessSize::U32, 0xfefefefe)
            .unwrap();

        let (vintid, targets) = model
            .last_spi_route
            .get()
            .expect("SPI route should be recorded");
        assert_eq!(vintid, VIntId(35));
        assert_eq!(targets, VcpuMask::from_bits(0x0e));
    }
}
