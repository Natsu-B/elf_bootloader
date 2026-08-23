use cpu::CoreAffinity;
use typestate::Readable;
use typestate::Writable;

use crate::AckKind;
use crate::AckedIrq;
use crate::BinaryPoint;
use crate::EoiMode;
use crate::GicCpuCaps;
use crate::GicCpuInterface;
use crate::GicError;
use crate::IrqGroup;
use crate::gicv2::Gicv2;
use crate::gicv2::registers::GICC_ABPR;
use crate::gicv2::registers::GICC_BPR;
use crate::gicv2::registers::GICC_CTLR;
use crate::gicv2::registers::GICC_DIR;
use crate::gicv2::registers::GICC_EOIR;
use crate::gicv2::registers::GICC_IAR;
use crate::gicv2::registers::GICC_PMR;

impl GicCpuInterface for Gicv2 {
    fn init_cpu_interface(&self) -> Result<GicCpuCaps, GicError> {
        let security_ext = self.is_security_extension_implemented();
        // disable cpu interface
        self.disable_cpu_interface(security_ext);

        // initialize banked Distributor state for SGI/PPI targeting
        self.init_banked_sgi_ppi_state()?;

        // set affinity to cpu_id table
        let affinity = cpu::get_current_core_id();
        let cpu_if = tls::cpu_if().ok_or(GicError::UninitCpuId)?;
        if cpu_if >= 8 {
            return Err(GicError::InvalidCpuId);
        }
        {
            let mut table = self.affinity_table.write();
            let slot = &mut table[cpu_if as usize];
            match slot {
                Some(a) if a.0 != affinity => return Err(GicError::InvalidState),
                None | Some(_) => *slot = Some((affinity, false, false)),
            }
        }
        // set priority mask(non block)
        self.set_priority_mask(0xff)?;
        // check supported priority bits(RAZ/WI)
        let priority = self.gicc.pmr.read().get(GICC_PMR::priority);

        // set binary point
        self.gicc
            .bpr
            .write(GICC_BPR::new().set(GICC_BPR::binary_point, 0));
        let binary_point = self.gicc.bpr.read().get(GICC_BPR::binary_point);

        // enable cpu interface
        if security_ext {
            self.gicc
                .ctlr
                .set_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp1_non_secure, 1));
        } else {
            self.gicc
                .ctlr
                .set_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp0, 1));
            self.gicc
                .ctlr
                .set_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp1, 1));
        }

        // check spilt EOI/deactivate mode support
        let supports_deactivate = if security_ext {
            self.gicc
                .ctlr
                .set_bits(GICC_CTLR::new().set(GICC_CTLR::eoi_mode_ns_non_secure, 1));
            self.gicc.ctlr.read().get(GICC_CTLR::eoi_mode_ns_non_secure) != 0
        } else {
            self.gicc
                .ctlr
                .set_bits(GICC_CTLR::new().set(GICC_CTLR::eoi_mode_ns, 1));
            self.gicc.ctlr.read().get(GICC_CTLR::eoi_mode_ns) != 0
        };

        cpu::dsb_sy();
        cpu::isb();

        Ok(GicCpuCaps {
            priority_bits: priority.count_ones() as u8,
            binary_points_min: binary_point as u8,
            supports_group0: !security_ext,
            supports_group1: true,
            supports_separate_binary_points: !security_ext,
            supports_deactivate,
        })
    }

    fn configure(&self, cfg: &crate::GicCpuConfig) -> Result<(), GicError> {
        let security_ext = self.is_security_extension_implemented();
        // disable cpu interface for configuration
        self.disable_cpu_interface(security_ext);

        // priority mask
        self.set_priority_mask(cfg.priority_mask)?;

        if security_ext {
            // binary point
            match cfg.binary_point {
                BinaryPoint::Common(binary_point) if binary_point < 0x8 => self
                    .gicc
                    .bpr
                    .write(GICC_BPR::new().set(GICC_BPR::binary_point, binary_point as u32)),
                BinaryPoint::Common(_) => return Err(GicError::InvalidState),
                BinaryPoint::Separate {
                    group0: _,
                    group1: _,
                } => return Err(GicError::UnsupportedFeature),
            }

            match cfg.eoi_mode {
                EoiMode::DropOnly => self
                    .gicc
                    .ctlr
                    .set_bits(GICC_CTLR::new().set(GICC_CTLR::eoi_mode_ns_non_secure, 1)),
                EoiMode::DropAndDeactivate => self
                    .gicc
                    .ctlr
                    .clear_bits(GICC_CTLR::new().set(GICC_CTLR::eoi_mode_ns_non_secure, 1)),
            }

            if cfg.enable_group0 {
                return Err(GicError::UnsupportedFeature);
            }

            self.gicc.ctlr.set_bits(
                GICC_CTLR::new().set(GICC_CTLR::enable_grp1_non_secure, cfg.enable_group1 as u32),
            );

            self.gicc.ctlr.set_bits(
                GICC_CTLR::new()
                    .set(GICC_CTLR::fiq_byp_dis_grp1_non_secure, 1)
                    .set(GICC_CTLR::irq_byp_dis_grp1_non_secure, 1),
            );
        } else {
            // binary point
            match cfg.binary_point {
                BinaryPoint::Common(binary_point) if binary_point < 0x8 => {
                    self.gicc
                        .ctlr
                        .set_bits(GICC_CTLR::new().set(GICC_CTLR::cbpr, 1));
                    self.gicc
                        .bpr
                        .write(GICC_BPR::new().set(GICC_BPR::binary_point, binary_point as u32));
                }
                BinaryPoint::Separate { group0, group1 } if group0 < 0x8 && group1 < 0x8 => {
                    self.gicc
                        .ctlr
                        .clear_bits(GICC_CTLR::new().set(GICC_CTLR::cbpr, 1));
                    self.gicc
                        .bpr
                        .write(GICC_BPR::new().set(GICC_BPR::binary_point, group0 as u32));
                    self.gicc
                        .abpr
                        .write(GICC_ABPR::new().set(GICC_ABPR::binary_point, group1 as u32));
                }
                _ => return Err(GicError::InvalidState),
            }

            match cfg.eoi_mode {
                EoiMode::DropOnly => self.gicc.ctlr.set_bits(
                    GICC_CTLR::new()
                        .set(GICC_CTLR::eoi_mode_s, 1)
                        .set(GICC_CTLR::eoi_mode_ns, 1),
                ),
                EoiMode::DropAndDeactivate => self.gicc.ctlr.clear_bits(
                    GICC_CTLR::new()
                        .set(GICC_CTLR::eoi_mode_s, 1)
                        .set(GICC_CTLR::eoi_mode_ns, 1),
                ),
            }

            self.gicc
                .ctlr
                .clear_bits(GICC_CTLR::new().set(GICC_CTLR::ack_ctl, 1));

            let enable_group0_hw = cfg.enable_group0 || cfg.enable_group1;

            if enable_group0_hw {
                self.gicc
                    .ctlr
                    .set_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp0, 1));
            } else {
                self.gicc
                    .ctlr
                    .clear_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp0, 1));
            }

            if cfg.enable_group1 {
                self.gicc
                    .ctlr
                    .set_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp1, 1));
            } else {
                self.gicc
                    .ctlr
                    .clear_bits(GICC_CTLR::new().set(GICC_CTLR::enable_grp1, 1));
            }

            self.gicc.ctlr.set_bits(
                GICC_CTLR::new()
                    .set(GICC_CTLR::fiq_byp_dis_grp0, 1)
                    .set(GICC_CTLR::irq_byp_dis_grp0, 1)
                    .set(GICC_CTLR::fiq_byp_dis_grp1, 1)
                    .set(GICC_CTLR::irq_byp_dis_grp1, 1),
            );
        };

        let cpu_if = self.current_cpu_if()?;
        let mut table = self.affinity_table.write();
        let slot = &mut table[cpu_if as usize];
        match slot {
            Some(a) if a.0 == cpu::get_current_core_id() => {
                *a = (a.0, cfg.enable_group0, cfg.enable_group1)
            }
            _ => return Err(GicError::InvalidState),
        }

        cpu::dsb_sy();
        cpu::isb();

        Ok(())
    }

    fn set_priority_mask(&self, pmr: u8) -> Result<(), crate::GicError> {
        self.gicc
            .pmr
            .write(GICC_PMR::new().set(GICC_PMR::priority, pmr as u32));
        Ok(())
    }

    fn acknowledge(&self) -> Result<Option<crate::AckedIrq>, crate::GicError> {
        let enable_group = self.current_enable_groups()?;
        if self.is_security_extension_implemented() {
            if !enable_group.1 {
                return Err(GicError::InvalidState);
            }
            // expect non-secure world
            self.ack_via_iar(|| Ok(None))
        } else {
            match (enable_group.0, enable_group.1) {
                (true, true) => self.ack_via_iar(|| self.ack_via_aiar(|| Ok(None))),
                (true, false) => self.ack_via_iar(|| Ok(None)),
                (false, true) => self.ack_via_aiar(|| self.ack_via_iar(|| Ok(None))),
                (false, false) => Ok(None),
            }
        }
    }

    fn end_of_interrupt(&self, ack: crate::AckedIrq) -> Result<(), crate::GicError> {
        match ack.ack_kind {
            AckKind::Iar => self.gicc.eoir.write(GICC_EOIR::from_bits(ack.raw)),
            AckKind::Aiar => self.gicc.aeoir.write(ack.raw),
        };
        Ok(())
    }

    fn deactivate(&self, ack: crate::AckedIrq) -> Result<(), crate::GicError> {
        let ctlr = self.gicc.ctlr.read();
        if self.is_security_extension_implemented() {
            if ctlr.get(GICC_CTLR::eoi_mode_ns_non_secure) == 0 {
                return Ok(());
            }
            self.gicc.dir.write(GICC_DIR::from_bits(ack.raw));
            return Ok(());
        }

        let separate = match ack.group {
            IrqGroup::Group0 => ctlr.get(GICC_CTLR::eoi_mode_s) != 0,
            IrqGroup::Group1 => ctlr.get(GICC_CTLR::eoi_mode_ns) != 0,
        };

        if !separate {
            return Ok(());
        }

        self.gicc.dir.write(GICC_DIR::from_bits(ack.raw));
        Ok(())
    }
}

impl Gicv2 {
    pub fn logical_group(&self, intid: u32) -> Result<IrqGroup, GicError> {
        if self.is_security_extension_implemented() {
            self.intid_group(intid)
        } else {
            self.shadow_group(intid)
        }
    }

    fn disable_cpu_interface(&self, security_ext: bool) {
        let enable_bits = if security_ext {
            GICC_CTLR::new().set(GICC_CTLR::enable_grp1_non_secure, 1)
        } else {
            GICC_CTLR::new()
                .set(GICC_CTLR::enable_grp0, 1)
                .set(GICC_CTLR::enable_grp1, 1)
                .set(GICC_CTLR::ack_ctl, 1)
        };
        self.gicc.ctlr.clear_bits(enable_bits);
        cpu::dsb_sy();
        cpu::isb();
    }

    fn ack_via_iar<F>(&self, spurious_action: F) -> Result<Option<AckedIrq>, GicError>
    where
        F: FnOnce() -> Result<Option<AckedIrq>, GicError>,
    {
        let raw = self.gicc.iar.read();
        match raw.get(GICC_IAR::interrupt_id) {
            x if x < 1020 => {
                let sgi = self.sgi_source(raw)?;
                let group = self.intid_group(x)?;
                Ok(Some(AckedIrq {
                    raw: raw.bits(),
                    ack_kind: AckKind::Iar,
                    intid: x,
                    source: sgi,
                    group,
                }))
            }
            x if x == 1022 || x == 1023 => spurious_action(),
            _ => Err(GicError::UnsupportedIntId),
        }
    }

    fn ack_via_aiar<F>(&self, spurious_action: F) -> Result<Option<AckedIrq>, GicError>
    where
        F: FnOnce() -> Result<Option<AckedIrq>, GicError>,
    {
        let raw = self.gicc.aiar.read();
        match raw.get(GICC_IAR::interrupt_id) {
            x if x < 1020 => {
                let sgi = self.sgi_source(raw)?;
                let group = self.intid_group(x)?;
                Ok(Some(AckedIrq {
                    raw: raw.bits(),
                    ack_kind: AckKind::Aiar,
                    intid: x,
                    source: sgi,
                    group,
                }))
            }
            x if x == 1022 || x == 1023 => spurious_action(),
            _ => Err(GicError::UnsupportedIntId),
        }
    }

    fn intid_group(&self, intid: u32) -> Result<IrqGroup, GicError> {
        if intid >= self.max_intid() {
            return Err(GicError::UnsupportedIntId);
        }
        if !self.is_security_extension_implemented() {
            return self.shadow_group(intid);
        }
        let word = (intid / 32) as usize;
        let bit = 1u32 << (intid % 32);
        let is_group1 = (self.gicd.igroupr[word].read() & bit) != 0;
        Ok(if is_group1 {
            IrqGroup::Group1
        } else {
            IrqGroup::Group0
        })
    }

    fn sgi_source(&self, int: GICC_IAR) -> Result<Option<CoreAffinity>, GicError> {
        if int.get(GICC_IAR::interrupt_id) >= 16 {
            return Ok(None);
        }
        let masks = int.get(GICC_IAR::cpu_id);
        self.affinity_from_cpu_id(masks as u8).map(Some)
    }
}
