use crate::EnableOp;
use crate::GicDistributor;
use crate::GicError;
use crate::GicIrqMirror;
use crate::GicMirrorOp;
use crate::GicMirrorRoute;
use crate::GicMirrorScope;
use crate::GicPpi;
use crate::GicSgi;
use crate::IrqGroup;
use crate::SgiTarget;
use crate::SpiRoute;
use crate::TriggerMode;
use crate::gicv2::Gicv2;
use crate::gicv2::registers::GICD_CTLR;
use crate::gicv2::registers::GICD_SGIR;
use crate::gicv2::registers::TargetListFilter;
use core::cmp;
use typestate::Readable;
use typestate::Writable;

/// Resolves a mirrored PPI or SPI to its Distributor word and bit.
fn mirror_word_and_bit(
    scope: GicMirrorScope,
    intid: u32,
    max_intid: u32,
) -> Result<(usize, u32), GicError> {
    let word = match scope {
        GicMirrorScope::Local(_) if (16..32).contains(&intid) => 0,
        GicMirrorScope::Global if (32..max_intid).contains(&intid) => (intid / 32) as usize,
        _ => return Err(GicError::UnsupportedIntId),
    };
    Ok((word, 1u32 << (intid % 32)))
}

impl Gicv2 {
    pub(crate) fn init_banked_sgi_ppi_state(&self) -> Result<(), GicError> {
        // disable PPIs
        const PPI_MASK: u32 = 0xffff_0000;
        let security_ext = self.is_security_extension_implemented();

        self.gicd.icenabler[0].write(PPI_MASK);
        // Clear any stale SGI/PPI pending/active state before re-enabling.
        self.gicd.icpendr[0].write(PPI_MASK);
        self.gicd.icactiver[0].write(0xffff_ffff);

        for v in self.gicd.ipriorityr.iter().take(8).flatten() {
            // Give SGIs/PPIs the highest priority (numerically lowest) so they are not masked.
            v.write(0x00);
        }

        // Default SGI/PPI group selection: Group0 when Security Extensions are absent, Group1
        // otherwise (to keep Non-secure SGIs usable when EL3 firmware programs security state).
        self.gicd.igroupr[0].write(if security_ext {
            0xffff_ffff
        } else {
            0x0000_0000
        });
        // Enable SGIs for this CPU interface; PPIs remain disabled and should be configured via
        // `GicPpi::configure_ppi` per-PE after CPU interface initialization.
        self.gicd.isenabler[0].write(!PPI_MASK);

        cpu::dsb_sy();
        cpu::isb();

        Ok(())
    }

    fn set_spi_route_inner(&self, intid: u32, route: SpiRoute) -> Result<(), GicError> {
        if intid < 32 || intid >= self.max_intid() {
            return Err(GicError::UnsupportedIntId);
        }

        // Keep INTID validation before route conversion so its error takes precedence.
        let cpu_targets = match route {
            SpiRoute::Specific(affinity) => self.cpu_targets_mask_from_affinity(affinity)?,
            SpiRoute::AnyParticipating => return Err(GicError::UnsupportedFeature),
        };
        // The byte helper performs the bounds-checked, slot-local ITARGETSR write without
        // modifying other interrupt state.
        self.set_spi_route_byte_inner(intid, cpu_targets)
    }

    fn set_spi_route_byte_inner(&self, intid: u32, cpu_targets: u8) -> Result<(), GicError> {
        if intid < 32 || intid >= self.max_intid() {
            return Err(GicError::UnsupportedIntId);
        }

        let spi = (intid - 32) as usize;
        let reg = spi / 4;
        let byte = spi % 4;
        let target = self
            .gicd
            .itargetsr
            .get(reg)
            .and_then(|entry| entry.get(byte))
            .ok_or(GicError::UnsupportedIntId)?;

        // SAFETY: The Distributor MMIO frame is validated and mapped by `Gicv2::new`, `intid`
        // has been checked as an SPI in-range for this GIC instance, and this byte write targets
        // only the ITARGETSR slot for the selected SPI without modifying other interrupt state.
        target.write(cpu_targets);
        Ok(())
    }

    fn apply_group(
        &self,
        scope: GicMirrorScope,
        intid: u32,
        group: IrqGroup,
    ) -> Result<(), GicError> {
        let security_ext = self.is_security_extension_implemented();
        let (word, bit) = mirror_word_and_bit(scope, intid, self.max_intid())?;
        if security_ext && group == IrqGroup::Group0 {
            return Err(GicError::UnsupportedFeature);
        }

        if security_ext {
            let igroupr = self.gicd.igroupr[word].read();
            self.gicd.igroupr[word].write(igroupr | bit);
        } else {
            self.set_shadow_group(intid, group)?;
            let igroupr = self.gicd.igroupr[word].read() & !bit;
            self.gicd.igroupr[word].write(igroupr);
        }
        Ok(())
    }

    fn apply_priority(
        &self,
        scope: GicMirrorScope,
        intid: u32,
        priority: u8,
    ) -> Result<(), GicError> {
        mirror_word_and_bit(scope, intid, self.max_intid())?;
        self.gicd.ipriorityr[intid as usize / 4][intid as usize % 4].write(priority);
        Ok(())
    }

    fn apply_trigger(
        &self,
        scope: GicMirrorScope,
        intid: u32,
        trigger: TriggerMode,
    ) -> Result<(), GicError> {
        mirror_word_and_bit(scope, intid, self.max_intid())?;
        let reg = intid as usize / 16;
        let shift = (intid as usize % 16) * 2;
        self.gicd.icfgr[reg].clear_bits(0b11 << shift);
        self.gicd.icfgr[reg].set_bits(
            match trigger {
                TriggerMode::Edge => 0b10,
                TriggerMode::Level => 0b00,
            } << shift,
        );
        Ok(())
    }

    fn apply_enable(
        &self,
        scope: GicMirrorScope,
        intid: u32,
        enable: bool,
    ) -> Result<(), GicError> {
        let (word, bit) = mirror_word_and_bit(scope, intid, self.max_intid())?;
        if enable {
            self.gicd.isenabler[word].write(bit);
        } else {
            self.gicd.icenabler[word].write(bit);
        }
        Ok(())
    }

    fn apply_pending(
        &self,
        scope: GicMirrorScope,
        intid: u32,
        pending: bool,
    ) -> Result<(), GicError> {
        let (word, bit) = mirror_word_and_bit(scope, intid, self.max_intid())?;
        if pending {
            self.gicd.ispendr[word].write(bit);
        } else {
            self.gicd.icpendr[word].write(bit);
        }
        Ok(())
    }

    fn apply_active(
        &self,
        scope: GicMirrorScope,
        intid: u32,
        active: bool,
    ) -> Result<(), GicError> {
        let (word, bit) = mirror_word_and_bit(scope, intid, self.max_intid())?;
        if active {
            self.gicd.isactiver[word].write(bit);
        } else {
            self.gicd.icactiver[word].write(bit);
        }
        Ok(())
    }
}

impl GicIrqMirror for Gicv2 {
    fn max_intid(&self) -> u32 {
        Gicv2::max_intid(self)
    }

    fn apply_mirror_op(&self, op: GicMirrorOp) -> Result<(), GicError> {
        match op {
            GicMirrorOp::SetGroup {
                scope,
                intid,
                group,
            } => self.apply_group(scope, intid, group)?,
            GicMirrorOp::SetPriority {
                scope,
                intid,
                priority,
            } => self.apply_priority(scope, intid, priority)?,
            GicMirrorOp::SetTrigger {
                scope,
                intid,
                trigger,
            } => self.apply_trigger(scope, intid, trigger)?,
            GicMirrorOp::SetEnable {
                scope,
                intid,
                enable,
            } => self.apply_enable(scope, intid, enable)?,
            GicMirrorOp::SetPending {
                scope,
                intid,
                pending,
            } => self.apply_pending(scope, intid, pending)?,
            GicMirrorOp::SetActive {
                scope,
                intid,
                active,
            } => self.apply_active(scope, intid, active)?,
            GicMirrorOp::SetRoute { intid, route } => match route {
                GicMirrorRoute::Gicv2TargetMask(mask) => {
                    self.set_spi_route_byte_inner(intid, mask)?;
                }
                GicMirrorRoute::Abstract(route) => self.set_spi_route_inner(intid, route)?,
            },
        }

        cpu::dsb_sy();
        cpu::isb();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VcpuId;

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn mirror_scope_rejects_intids_outside_its_partition() {
        let local = GicMirrorScope::Local(VcpuId(0));
        let cases = [
            (local, 15, Err(GicError::UnsupportedIntId)),
            (local, 16, Ok((0, 1 << 16))),
            (local, 31, Ok((0, 1 << 31))),
            (local, 32, Err(GicError::UnsupportedIntId)),
            (GicMirrorScope::Global, 31, Err(GicError::UnsupportedIntId)),
            (GicMirrorScope::Global, 32, Ok((1, 1))),
            (GicMirrorScope::Global, 63, Ok((1, 1 << 31))),
            (GicMirrorScope::Global, 64, Err(GicError::UnsupportedIntId)),
        ];
        for (scope, intid, expected) in cases {
            assert_eq!(mirror_word_and_bit(scope, intid, 64), expected);
        }
    }
}

impl GicPpi for Gicv2 {
    fn set_ppi_enable(&self, intid: u32, enable: bool) -> Result<(), GicError> {
        if intid < 16 || intid >= 32 {
            return Err(GicError::UnsupportedIntId);
        }

        let bit = 1u32 << (intid % 32);
        if enable {
            self.gicd.isenabler[0].write(bit);
        } else {
            self.gicd.icenabler[0].write(bit);
        }

        cpu::dsb_sy();
        cpu::isb();
        Ok(())
    }

    fn configure_ppi(
        &self,
        intid: u32,
        group: IrqGroup,
        priority: u8,
        trigger: TriggerMode,
        enable: EnableOp,
    ) -> Result<(), GicError> {
        let security_ext = self.is_security_extension_implemented();

        if intid < 16 || intid >= 32 {
            return Err(GicError::UnsupportedIntId);
        }
        if security_ext && group == IrqGroup::Group0 {
            return Err(GicError::UnsupportedFeature);
        }

        let bit = 1u32 << (intid % 32);
        let enable_after = match enable {
            EnableOp::Keep => (self.gicd.isenabler[0].read() >> (intid as usize % 32)) & 0b1 != 0,
            EnableOp::Enable => true,
            EnableOp::Disable => false,
        };

        self.gicd.icenabler[0].write(bit);

        self.gicd.icpendr[0].write(bit);
        self.gicd.icactiver[0].write(bit);

        if security_ext {
            if group == IrqGroup::Group1 {
                let igroupr = self.gicd.igroupr[0].read();
                self.gicd.igroupr[0].write(igroupr | bit);
            }
        } else {
            self.set_shadow_group(intid, group)?;
            let igroupr = self.gicd.igroupr[0].read() & !bit;
            self.gicd.igroupr[0].write(igroupr);
        }

        self.gicd.ipriorityr[intid as usize / 4][intid as usize % 4].write(priority);

        let reg = intid as usize / 16;
        let shift = (intid as usize % 16) * 2;
        self.gicd.icfgr[reg].clear_bits(0b11 << shift);
        self.gicd.icfgr[reg].set_bits(
            match trigger {
                TriggerMode::Edge => 0b10,
                TriggerMode::Level => 0b00,
            } << shift,
        );

        cpu::dsb_sy();
        cpu::isb();

        if enable_after {
            self.gicd.isenabler[0].write(bit);
            cpu::dsb_sy();
            cpu::isb();
        }

        Ok(())
    }
}

impl GicSgi for Gicv2 {
    fn send_sgi(&self, sgi_id: u8, target: crate::SgiTarget<'_>) -> Result<(), crate::GicError> {
        if sgi_id >= 16 {
            return Err(GicError::UnsupportedIntId);
        }

        let value = GICD_SGIR::new().set(GICD_SGIR::sgi_int_id, sgi_id as u32);
        let value = match target {
            SgiTarget::Specific(affinities) => {
                if affinities.is_empty() {
                    return Ok(());
                }
                let mut mask: u8 = 0;
                for affinity in affinities {
                    mask |= self.cpu_targets_mask_from_affinity(*affinity)?;
                }
                if mask == 0 {
                    return Ok(());
                }
                value
                    .set_enum(
                        GICD_SGIR::target_list_filter,
                        TargetListFilter::CpuTargetListFieldSpecified,
                    )
                    .set(GICD_SGIR::cpu_target_list, mask as u32)
            }
            SgiTarget::AllButSelf => value.set_enum(
                GICD_SGIR::target_list_filter,
                TargetListFilter::InterruptAllCpuExceptRequestedCpu,
            ),
            // TargetListFilter = 0b10 selects the current CPU interface only.
            SgiTarget::SelfOnly => value.set_enum(
                GICD_SGIR::target_list_filter,
                TargetListFilter::InterruptSelfOnly,
            ),
        };

        self.gicd.sgir.write(value);
        cpu::dsb_sy();
        cpu::isb();
        Ok(())
    }
}

impl GicDistributor for Gicv2 {
    fn init_distributor(&self) -> Result<(), crate::GicError> {
        // disable distributor
        let security_ext = self.is_security_extension_implemented();
        let mutex = self.mutex.lock();
        if security_ext {
            self.gicd
                .ctlr
                .write(GICD_CTLR::new().set(GICD_CTLR::enable_grp1_non_secure, 0));
        } else {
            self.gicd.ctlr.write(
                GICD_CTLR::new()
                    .set(GICD_CTLR::enable_grp0, 0)
                    .set(GICD_CTLR::enable_grp1, 0),
            );
        }
        drop(mutex);
        cpu::dsb_sy();
        cpu::isb();

        let max_intid = self.max_intid();
        let num_words = (max_intid as usize).div_ceil(32);

        // Program interrupt groups so SGIs/PPIs/SPIs are visible to the Non-secure interface.
        // If IGROUPR is Secure-only, these writes are RAZ/WI and firmware must configure groups.
        let words = core::cmp::min(num_words, self.gicd.igroupr.len());
        for n in 0..words {
            let mask = Self::word_mask(max_intid, n);
            if mask == 0 {
                break;
            }
            if security_ext {
                // On systems with Security Extensions, IGROUPR can be Secure-only (RAZ/WI here).
                self.gicd.igroupr[n].write(mask);
            } else {
                // Default to Group0 when Security Extensions are absent; AckCtl=0 expects Group1
                // interrupts to use AIAR/AEOIR, which this driver does not enable by default.
                self.gicd.igroupr[n].write(0);
            }
        }

        if !security_ext {
            // Track all interrupts as Group0 logically when Security Extensions are absent.
            self.reset_shadow_groups();
        }

        // disable all interrupts
        let words = cmp::min(num_words, self.gicd.icenabler.len());
        for n in 0..words {
            let mask = Self::word_mask(max_intid, n);
            if mask == 0 {
                break;
            }
            self.gicd.icenabler[n].write(mask);
            self.gicd.icpendr[n].write(mask);
            self.gicd.icactiver[n].write(mask);
        }

        let mutex = self.mutex.lock();
        let ipriority_capacity = (self.gicd.ipriorityr.len() * 4) as u32;
        let actual_interrupts = max_intid.min(ipriority_capacity);
        for intid in 0..(actual_interrupts as usize) {
            self.gicd.ipriorityr[intid / 4][intid % 4].write(0xff);
        }
        drop(mutex);

        // enable distributor
        if self.is_security_extension_implemented() {
            self.gicd
                .ctlr
                .write(GICD_CTLR::new().set(GICD_CTLR::enable_grp1_non_secure, 1));
        } else {
            self.gicd.ctlr.write(
                GICD_CTLR::new()
                    .set(GICD_CTLR::enable_grp0, 1)
                    .set(GICD_CTLR::enable_grp1, 1),
            );
        }
        cpu::dsb_sy();
        cpu::isb();

        Ok(())
    }

    fn max_intid(&self) -> u32 {
        Gicv2::max_intid(self)
    }

    fn set_spi_enable(&self, intid: u32, enable: bool) -> Result<(), GicError> {
        if intid < 32 || intid >= self.max_intid() {
            return Err(GicError::UnsupportedIntId);
        }
        if enable {
            self.gicd.isenabler[intid as usize / 32].write(1 << (intid as usize % 32));
        } else {
            self.gicd.icenabler[intid as usize / 32].write(1 << (intid as usize % 32));
        }

        // Ensure the enable change is visible before callers proceed (tests may wait immediately).
        cpu::dsb_sy();
        cpu::isb();

        Ok(())
    }

    fn set_spi_route(&self, intid: u32, route: SpiRoute) -> Result<(), GicError> {
        self.set_spi_route_inner(intid, route)?;

        cpu::dsb_sy();
        cpu::isb();
        Ok(())
    }

    fn configure_spi(
        &self,
        intid: u32,
        group: crate::IrqGroup,
        priority: u8,
        trigger: crate::TriggerMode,
        route: crate::SpiRoute,
        enable: EnableOp,
    ) -> Result<(), crate::GicError> {
        let security_ext = self.is_security_extension_implemented();

        if security_ext && group == IrqGroup::Group0 {
            // this crate does not support secure state
            return Err(GicError::UnsupportedFeature);
        }
        if intid < 32 || intid >= self.max_intid() {
            return Err(GicError::UnsupportedIntId);
        }
        let word = (intid / 32) as usize;
        let bit = 1u32 << (intid % 32);
        let enable_after = match enable {
            EnableOp::Keep => {
                (self.gicd.isenabler[word].read() >> (intid as usize % 32)) & 0b1 != 0
            }
            EnableOp::Enable => true,
            EnableOp::Disable => false,
        };

        // disable interrupts for setting
        self.gicd.icenabler[word].write(bit);

        // clear pending/active
        self.gicd.icpendr[word].write(bit);
        self.gicd.icactiver[word].write(bit);

        let mutex = self.mutex.lock();
        if security_ext {
            // Non-secure Group 1: set IGROUPR bit (RMW; do not overwrite other interrupts' group
            // bits). On implementations with Security Extensions (e.g. GIC-400) and EL3 firmware,
            // GICD_IGROUPRn is Secure-only; Non-secure accesses are RAZ/WI. Firmware must
            // configure interrupt groups.
            if group == IrqGroup::Group1 {
                let igroupr = self.gicd.igroupr[word].read();
                self.gicd.igroupr[word].write(igroupr | bit);
            }
        } else {
            // Security Extensions absent: remember the requested logical group in software and
            // force hardware IGROUPR to Group0 to keep AckCtl=0 usable without AIAR.
            self.set_shadow_group(intid, group)?;
            let igroupr = self.gicd.igroupr[word].read() & !bit;
            self.gicd.igroupr[word].write(igroupr);
        }

        self.gicd.ipriorityr[intid as usize / 4][intid as usize % 4].write(priority);

        self.set_spi_route_inner(intid, route)?;

        self.gicd.icfgr[intid as usize / 16].clear_bits(0b11 << ((intid as usize % 16) * 2));
        self.gicd.icfgr[intid as usize / 16].set_bits(
            if trigger == TriggerMode::Level {
                0b00
            } else {
                0b10
            } << ((intid as usize % 16) * 2),
        );
        drop(mutex);

        // Ensure route/priority/group programming is observed before (re-)enabling the SPI.
        cpu::dsb_sy();
        cpu::isb();

        if enable_after {
            self.gicd.isenabler[word].write(bit);

            // Make the enable visible before callers proceed (tests may poll immediately).
            cpu::dsb_sy();
            cpu::isb();
        }

        Ok(())
    }

    fn set_pending(&self, intid: u32, pending: bool) -> Result<(), crate::GicError> {
        if intid < 32 || intid >= self.max_intid() {
            return Err(GicError::UnsupportedIntId);
        }
        let word = (intid / 32) as usize;
        let bit = 1u32 << (intid % 32);

        if pending {
            self.gicd.ispendr[word].write(bit);
        } else {
            self.gicd.icpendr[word].write(bit);
        }

        cpu::dsb_sy();
        cpu::isb();
        Ok(())
    }
}
