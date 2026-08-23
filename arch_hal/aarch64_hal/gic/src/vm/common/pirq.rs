use crate::GicError;
use crate::IrqGroup;
use crate::IrqSense;
use crate::IrqState as IrqStateKind;
use crate::PIntId;
use crate::PhysicalIrqBindingKind;
use crate::PhysicalIrqGuestState;
use crate::TriggerMode;
use crate::VIntId;
use crate::VcpuId;
use crate::VcpuMask;
use crate::VgicGuestRegs;
use crate::VgicIrqScope;
use crate::VgicTargets;
use crate::VgicUpdate;
use crate::VgicVcpuModel;
use crate::VgicVcpuQueue;
use crate::VgicWork;
use crate::VirtualInterrupt;
use crate::vm::GicVmModelGeneric;
use crate::vm::common::LOCAL_INTID_COUNT;

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum DistMirrorMode {
    ShadowOnly,
    WriteThrough,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum CpuDeliveryMode {
    SoftwareLr,
    HardwareLr,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum PirqDelivery {
    Local {
        target: VcpuId,
        dist_mode: DistMirrorMode,
        cpu_mode: CpuDeliveryMode,
    },
    Spi {
        dist_mode: DistMirrorMode,
        cpu_mode: CpuDeliveryMode,
    },
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) struct PirqEntry {
    pub(crate) vintid: VIntId,
    pub(crate) delivery: PirqDelivery,
}

impl PirqEntry {
    pub(crate) fn scope(&self) -> VgicIrqScope {
        match self.delivery {
            PirqDelivery::Local { target, .. } => VgicIrqScope::Local(target),
            PirqDelivery::Spi { .. } => VgicIrqScope::Global,
        }
    }
}

impl PirqDelivery {
    pub(crate) const fn dist_mode(self) -> DistMirrorMode {
        match self {
            PirqDelivery::Local { dist_mode, .. } | PirqDelivery::Spi { dist_mode, .. } => {
                dist_mode
            }
        }
    }

    pub(crate) const fn cpu_mode(self) -> CpuDeliveryMode {
        match self {
            PirqDelivery::Local { cpu_mode, .. } | PirqDelivery::Spi { cpu_mode, .. } => cpu_mode,
        }
    }
}

impl CpuDeliveryMode {
    pub(crate) const fn binding_kind(self) -> PhysicalIrqBindingKind {
        match self {
            CpuDeliveryMode::SoftwareLr => PhysicalIrqBindingKind::SoftwareLr,
            CpuDeliveryMode::HardwareLr => PhysicalIrqBindingKind::HardwareLr,
        }
    }
}

pub(crate) struct PirqTable<const VCPUS: usize>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
{
    global_entries: [Option<PirqEntry>; crate::max_intids_for_vcpus(VCPUS)],
    global_v2p: [Option<PIntId>; crate::max_intids_for_vcpus(VCPUS)],
    local_entries: [[Option<PirqEntry>; LOCAL_INTID_COUNT]; VCPUS],
    local_v2p: [[Option<PIntId>; LOCAL_INTID_COUNT]; VCPUS],
}

fn checked_index(raw: impl Into<u64>, len: usize, error: GicError) -> Result<usize, GicError> {
    let index = raw.into() as usize;
    (index < len).then_some(index).ok_or(error)
}

impl<const VCPUS: usize> PirqTable<VCPUS>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
{
    pub(crate) fn new() -> Self {
        Self {
            global_entries: [None; { crate::max_intids_for_vcpus(VCPUS) }],
            global_v2p: [None; { crate::max_intids_for_vcpus(VCPUS) }],
            local_entries: [[None; LOCAL_INTID_COUNT]; VCPUS],
            local_v2p: [[None; LOCAL_INTID_COUNT]; VCPUS],
        }
    }

    fn index(&self, pintid: PIntId) -> Result<usize, GicError> {
        checked_index(
            pintid.0,
            self.global_entries.len(),
            GicError::UnsupportedIntId,
        )
    }

    fn vindex(&self, vintid: VIntId) -> Result<usize, GicError> {
        checked_index(vintid.0, self.global_v2p.len(), GicError::UnsupportedIntId)
    }

    fn local_pindex(&self, pintid: PIntId) -> Result<usize, GicError> {
        checked_index(pintid.0, LOCAL_INTID_COUNT, GicError::UnsupportedIntId)
    }

    fn local_vindex(&self, vintid: VIntId) -> Result<usize, GicError> {
        checked_index(vintid.0, LOCAL_INTID_COUNT, GicError::UnsupportedIntId)
    }

    fn tindex(&self, target: VcpuId) -> Result<usize, GicError> {
        checked_index(target.0, VCPUS, GicError::InvalidVcpuId)
    }

    pub(crate) fn get(&self, pintid: PIntId) -> Result<Option<PirqEntry>, GicError> {
        let idx = self.index(pintid)?;
        Ok(self.global_entries[idx])
    }

    pub(crate) fn get_local(
        &self,
        target: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<PirqEntry>, GicError> {
        let t_idx = self.tindex(target)?;
        let p_idx = self.local_pindex(pintid)?;
        Ok(self.local_entries[t_idx][p_idx])
    }

    pub(crate) fn take(&mut self, pintid: PIntId) -> Result<Option<PirqEntry>, GicError> {
        let idx = self.index(pintid)?;
        let entry = self.global_entries[idx].take();
        if let Some(entry) = entry {
            let v_idx = self.vindex(entry.vintid)?;
            if matches!(self.global_v2p[v_idx], Some(existing) if existing == pintid) {
                self.global_v2p[v_idx] = None;
            }
        }
        Ok(entry)
    }

    pub(crate) fn take_local(
        &mut self,
        target: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<PirqEntry>, GicError> {
        let t_idx = self.tindex(target)?;
        let p_idx = self.local_pindex(pintid)?;
        let entry = self.local_entries[t_idx][p_idx].take();
        if let Some(entry) = entry {
            let v_idx = self.local_vindex(entry.vintid)?;
            if matches!(self.local_v2p[t_idx][v_idx], Some(existing) if existing == pintid) {
                self.local_v2p[t_idx][v_idx] = None;
            }
        }
        Ok(entry)
    }

    pub(crate) fn ensure_entry(
        &mut self,
        pintid: PIntId,
        entry: PirqEntry,
    ) -> Result<bool, GicError> {
        let idx = self.index(pintid)?;
        let v_idx = self.vindex(entry.vintid)?;

        if let Some(existing_pintid) = self.global_v2p[v_idx] {
            if existing_pintid != pintid {
                return Err(GicError::InvalidState);
            }
        }

        match self.global_entries[idx] {
            Some(existing) if existing == entry => {
                self.global_v2p[v_idx] = Some(pintid);
                Ok(false)
            }
            Some(_) => Err(GicError::InvalidState),
            None => {
                self.global_entries[idx] = Some(entry);
                self.global_v2p[v_idx] = Some(pintid);
                Ok(true)
            }
        }
    }

    pub(crate) fn ensure_local_entry(
        &mut self,
        target: VcpuId,
        pintid: PIntId,
        entry: PirqEntry,
    ) -> Result<bool, GicError> {
        let t_idx = self.tindex(target)?;
        let p_idx = self.local_pindex(pintid)?;
        let v_idx = self.local_vindex(entry.vintid)?;

        if let Some(existing_pintid) = self.local_v2p[t_idx][v_idx] {
            if existing_pintid != pintid {
                return Err(GicError::InvalidState);
            }
        }

        match self.local_entries[t_idx][p_idx] {
            Some(existing) if existing == entry => {
                self.local_v2p[t_idx][v_idx] = Some(pintid);
                Ok(false)
            }
            Some(_) => Err(GicError::InvalidState),
            None => {
                self.local_entries[t_idx][p_idx] = Some(entry);
                self.local_v2p[t_idx][v_idx] = Some(pintid);
                Ok(true)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn lookup_by_vintid(&self, vintid: VIntId) -> Result<Option<PIntId>, GicError> {
        let idx = self.vindex(vintid)?;
        Ok(self.global_v2p[idx])
    }

    #[cfg(test)]
    pub(crate) fn lookup_local_by_vintid(
        &self,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<Option<PIntId>, GicError> {
        let t_idx = self.tindex(target)?;
        let v_idx = self.local_vindex(vintid)?;
        Ok(self.local_v2p[t_idx][v_idx])
    }

    pub(crate) fn v2p(&self, vintid: VIntId) -> Option<PIntId> {
        let idx = vintid.0 as usize;
        self.global_v2p.get(idx).copied().flatten()
    }

    pub(crate) fn v2p_local(&self, target: VcpuId, vintid: VIntId) -> Option<PIntId> {
        let t_idx = target.0 as usize;
        let v_idx = vintid.0 as usize;
        self.local_v2p
            .get(t_idx)
            .and_then(|table| table.get(v_idx))
            .copied()
            .flatten()
    }
}

impl<const VCPUS: usize, V: VgicVcpuModel + VgicVcpuQueue> GicVmModelGeneric<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); crate::vm::pending_cap_for_vcpus(VCPUS)]:,
{
    pub(crate) fn map_pirq_inner_configured(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
        sense: IrqSense,
        group: IrqGroup,
        priority: u8,
    ) -> Result<VgicUpdate, GicError> {
        self.map_pirq_inner_with_enable(pintid, target, vintid, sense, group, priority, true)
    }

    pub(crate) fn bind_local_pirq_inner_passthrough(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_local_pirq_inner_write_through(
            pintid,
            target,
            vintid,
            CpuDeliveryMode::HardwareLr,
        )
    }

    pub(crate) fn bind_local_pirq_inner_write_through_software_lr(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_local_pirq_inner_write_through(
            pintid,
            target,
            vintid,
            CpuDeliveryMode::SoftwareLr,
        )
    }

    fn bind_local_pirq_inner_write_through(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
        cpu_mode: CpuDeliveryMode,
    ) -> Result<VgicUpdate, GicError> {
        if vintid.0 >= LOCAL_INTID_COUNT as u32 {
            return Err(GicError::UnsupportedIntId);
        }

        self.common.vcpu_index(target)?;

        let max = crate::max_intids_for_vcpus(VCPUS) as u32;
        if vintid.0 >= max || pintid.0 >= max {
            return Err(GicError::UnsupportedIntId);
        }

        let entry = PirqEntry {
            vintid,
            delivery: PirqDelivery::Local {
                target,
                dist_mode: DistMirrorMode::WriteThrough,
                cpu_mode,
            },
        };

        let mut routing = self.common.routing_lock.lock_irqsave();
        let existing = routing.pirqs.get_local(target, pintid)?;
        if let Some(prev) = existing {
            if prev != entry {
                return Err(GicError::InvalidState);
            }
        }
        let _ = routing.pirqs.ensure_local_entry(target, pintid, entry)?;
        Ok(VgicUpdate::None)
    }

    pub(crate) fn bind_spi_pirq_inner_passthrough(
        &self,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_spi_pirq_inner(
            pintid,
            vintid,
            DistMirrorMode::WriteThrough,
            CpuDeliveryMode::HardwareLr,
        )
    }

    pub(crate) fn bind_spi_pirq_inner_write_through_software_lr(
        &self,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_spi_pirq_inner(
            pintid,
            vintid,
            DistMirrorMode::WriteThrough,
            CpuDeliveryMode::SoftwareLr,
        )
    }

    pub(crate) fn bind_spi_pirq_inner_shadow_software_lr(
        &self,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_spi_pirq_inner(
            pintid,
            vintid,
            DistMirrorMode::ShadowOnly,
            CpuDeliveryMode::SoftwareLr,
        )
    }

    fn bind_spi_pirq_inner(
        &self,
        pintid: PIntId,
        vintid: VIntId,
        dist_mode: DistMirrorMode,
        cpu_mode: CpuDeliveryMode,
    ) -> Result<VgicUpdate, GicError> {
        if vintid.0 < LOCAL_INTID_COUNT as u32 {
            return Err(GicError::UnsupportedIntId);
        }

        let max = crate::max_intids_for_vcpus(VCPUS) as u32;
        if vintid.0 >= max || pintid.0 >= max {
            return Err(GicError::UnsupportedIntId);
        }

        let entry = PirqEntry {
            vintid,
            delivery: PirqDelivery::Spi {
                dist_mode,
                cpu_mode,
            },
        };

        let mut routing = self.common.routing_lock.lock_irqsave();
        let existing = routing.pirqs.get(pintid)?;
        if let Some(prev) = existing {
            if prev != entry {
                return Err(GicError::InvalidState);
            }
        }
        let _ = routing.pirqs.ensure_entry(pintid, entry)?;
        Ok(VgicUpdate::None)
    }

    fn map_pirq_inner_with_enable(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
        sense: IrqSense,
        group: IrqGroup,
        priority: u8,
        enable: bool,
    ) -> Result<VgicUpdate, GicError> {
        let scope = if vintid.0 < LOCAL_INTID_COUNT as u32 {
            self.common.vcpu_index(target)?; // validate target
            VgicIrqScope::Local(target)
        } else {
            VgicIrqScope::Global
        };

        let entry = match scope {
            VgicIrqScope::Local(local_target) => PirqEntry {
                vintid,
                delivery: PirqDelivery::Local {
                    target: local_target,
                    dist_mode: DistMirrorMode::ShadowOnly,
                    cpu_mode: CpuDeliveryMode::SoftwareLr,
                },
            },
            VgicIrqScope::Global => PirqEntry {
                vintid,
                delivery: PirqDelivery::Spi {
                    dist_mode: DistMirrorMode::ShadowOnly,
                    cpu_mode: CpuDeliveryMode::SoftwareLr,
                },
            },
        };

        let max = crate::max_intids_for_vcpus(VCPUS) as u32;
        if vintid.0 >= max || pintid.0 >= max {
            return Err(GicError::UnsupportedIntId);
        }

        let (inserted, inserted_local_target) = {
            let mut routing = self.common.routing_lock.lock_irqsave();
            match scope {
                VgicIrqScope::Local(local_target) => {
                    let existing = routing.pirqs.get_local(local_target, pintid)?;
                    if let Some(prev) = existing {
                        if prev != entry {
                            return Err(GicError::InvalidState);
                        }
                    }
                    (
                        routing
                            .pirqs
                            .ensure_local_entry(local_target, pintid, entry)?,
                        Some(local_target),
                    )
                }
                VgicIrqScope::Global => {
                    let existing = routing.pirqs.get(pintid)?;
                    if let Some(prev) = existing {
                        if prev != entry {
                            return Err(GicError::InvalidState);
                        }
                    }
                    (routing.pirqs.ensure_entry(pintid, entry)?, None)
                }
            }
        };

        let trigger = match sense {
            IrqSense::Edge => TriggerMode::Edge,
            IrqSense::Level => TriggerMode::Level,
        };

        let mut update = VgicUpdate::None;
        let apply_result: Result<(), GicError> = (|| {
            update.combine(&self.set_group(scope, vintid, group)?);
            update.combine(&self.set_priority(scope, vintid, priority)?);
            update.combine(&self.set_trigger(scope, vintid, trigger)?);
            if enable {
                update.combine(&self.set_enable(scope, vintid, true)?);
            }
            Ok(())
        })();
        if let Err(err) = apply_result {
            if inserted {
                if enable {
                    let _ = self.set_enable(scope, vintid, false);
                }
                let mut routing = self.common.routing_lock.lock_irqsave();
                if let Some(local_target) = inserted_local_target {
                    let _ = routing.pirqs.take_local(local_target, pintid);
                } else {
                    let _ = routing.pirqs.take(pintid);
                }
            }
            return Err(err);
        }

        Ok(update)
    }

    #[cfg(test)]
    pub(crate) fn unmap_pirq_inner(&self, pintid: PIntId) -> Result<VgicUpdate, GicError> {
        let mut local_entries: [Option<PirqEntry>; VCPUS] = [None; VCPUS];
        let global_entry = {
            let mut routing = self.common.routing_lock.lock_irqsave();
            let global_entry = match routing.pirqs.take(pintid) {
                Ok(entry) => entry,
                Err(GicError::UnsupportedIntId) => None,
                Err(err) => return Err(err),
            };

            if (pintid.0 as usize) < LOCAL_INTID_COUNT {
                for cpu in 0..self.common.vcpu_count() {
                    let target = VcpuId(cpu as u16);
                    local_entries[cpu] = match routing.pirqs.take_local(target, pintid) {
                        Ok(entry) => entry,
                        Err(GicError::UnsupportedIntId) => None,
                        Err(err) => return Err(err),
                    };
                }
            }

            global_entry
        };

        let mut update = VgicUpdate::None;
        let mut removed = false;

        if let Some(entry) = global_entry {
            removed = true;
            update.combine(&self.set_enable(entry.scope(), entry.vintid, false)?);
        }

        for maybe_entry in local_entries.into_iter().take(self.common.vcpu_count()) {
            if let Some(entry) = maybe_entry {
                removed = true;
                update.combine(&self.set_enable(entry.scope(), entry.vintid, false)?);
            }
        }

        if !removed {
            return Ok(VgicUpdate::None);
        }

        Ok(update)
    }

    pub(crate) fn on_physical_irq_inner(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
        level: bool,
    ) -> Result<VgicUpdate, GicError> {
        let Some((entry, global_targets)) = self.physical_irq_binding_inner(source_vcpu, pintid)?
        else {
            return Err(GicError::UnsupportedIntId);
        };

        let scope = entry.scope();

        let mut update = VgicUpdate::None;
        let mut changed = false;
        let mut enqueue_irq: Option<VirtualInterrupt> = None;
        let mut requeue_if_not_present = false;

        // NOTE: `level` semantics for pIRQ ingress:
        // - For level-sensitive pIRQs, `level=true` means the line is asserted, `level=false`
        //   means deasserted.
        // - For edge-sensitive pIRQs, callers must pass `level=true` exactly once per detected
        //   edge (pulse). Passing repeated `level=true` while the virtual IRQ is still pending
        //   can inject duplicates; `level=false` is ignored for edge-sensitive pIRQs.
        let sense = {
            let regs = self.common.regs_lock.lock_irqsave();
            let trigger = regs.irq_state.trigger_mode(scope, entry.vintid)?;
            GicVmModelGeneric::<VCPUS, V>::trigger_to_sense(trigger)
        };
        let set_pending = match sense {
            IrqSense::Edge => level,
            IrqSense::Level => level,
        };

        {
            let mut regs = self.common.regs_lock.lock_irqsave();
            if set_pending {
                changed = regs.irq_state.set_pending(scope, entry.vintid, true)?;
                let attrs = regs.irq_state.irq_attrs(scope, entry.vintid)?;
                let dist_enabled = match attrs.group {
                    IrqGroup::Group0 => regs.dist_enable.0,
                    IrqGroup::Group1 => regs.dist_enable.1,
                };
                if changed && attrs.pending && attrs.enable && dist_enabled {
                    requeue_if_not_present = false;
                    let shadow_state = if attrs.pending && attrs.active {
                        IrqStateKind::PendingActive
                    } else if attrs.pending {
                        IrqStateKind::Pending
                    } else if attrs.active {
                        IrqStateKind::Active
                    } else {
                        IrqStateKind::Inactive
                    };
                    enqueue_irq = Some(match entry.delivery.cpu_mode() {
                        CpuDeliveryMode::HardwareLr => VirtualInterrupt::Hardware {
                            vintid: entry.vintid.0,
                            pintid: pintid.0,
                            priority: attrs.priority,
                            group: attrs.group,
                            state: shadow_state,
                            source: None,
                        },
                        CpuDeliveryMode::SoftwareLr => VirtualInterrupt::Software {
                            vintid: entry.vintid.0,
                            pintid: Some(pintid.0),
                            eoi_maintenance: true,
                            priority: attrs.priority,
                            group: attrs.group,
                            state: shadow_state,
                            source: None,
                        },
                    });
                } else if attrs.pending && attrs.enable && dist_enabled {
                    requeue_if_not_present = matches!(sense, IrqSense::Level);
                    if requeue_if_not_present {
                        let shadow_state = if attrs.pending && attrs.active {
                            IrqStateKind::PendingActive
                        } else if attrs.pending {
                            IrqStateKind::Pending
                        } else if attrs.active {
                            IrqStateKind::Active
                        } else {
                            IrqStateKind::Inactive
                        };
                        enqueue_irq = Some(match entry.delivery.cpu_mode() {
                            CpuDeliveryMode::HardwareLr => VirtualInterrupt::Hardware {
                                vintid: entry.vintid.0,
                                pintid: pintid.0,
                                priority: attrs.priority,
                                group: attrs.group,
                                state: shadow_state,
                                source: None,
                            },
                            CpuDeliveryMode::SoftwareLr => VirtualInterrupt::Software {
                                vintid: entry.vintid.0,
                                pintid: Some(pintid.0),
                                eoi_maintenance: true,
                                priority: attrs.priority,
                                group: attrs.group,
                                state: shadow_state,
                                source: None,
                            },
                        });
                    }
                }
            } else if matches!(sense, IrqSense::Level) {
                changed = regs.irq_state.set_pending(scope, entry.vintid, false)?;
            }
        }

        if let Some(irq) = enqueue_irq {
            match scope {
                VgicIrqScope::Local(target) => {
                    if changed
                        || (requeue_if_not_present
                            && !self.common.vcpu(target)?.contains_irq(entry.vintid, None))
                    {
                        update.combine(&self.common.enqueue_to_target(target, irq)?);
                    }
                }
                VgicIrqScope::Global => {
                    for target in global_targets.iter() {
                        if changed
                            || (requeue_if_not_present
                                && !self.common.vcpu(target)?.contains_irq(entry.vintid, None))
                        {
                            update.combine(&self.common.enqueue_to_target(target, irq)?);
                        }
                    }
                }
            }
        } else if !set_pending && matches!(sense, IrqSense::Level) {
            match scope {
                VgicIrqScope::Local(target) => {
                    self.common.vcpu(target)?.cancel_irq(entry.vintid, None)?;
                    update.combine(&VgicUpdate::Some {
                        targets: VgicTargets::One(target),
                        work: VgicWork::REFILL,
                    });
                }
                VgicIrqScope::Global => {
                    for target in global_targets.iter() {
                        self.common.vcpu(target)?.cancel_irq(entry.vintid, None)?;
                        update.combine(&VgicUpdate::Some {
                            targets: VgicTargets::One(target),
                            work: VgicWork::REFILL,
                        });
                    }
                }
            }
        }

        update.combine(&GicVmModelGeneric::<VCPUS, V>::update_for_scope(
            scope, changed,
        ));
        Ok(update)
    }

    fn physical_irq_binding_inner(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<(PirqEntry, VcpuMask)>, GicError> {
        self.common.vcpu_index(source_vcpu)?;

        let routing = self.common.routing_lock.lock_irqsave();
        let local_entry = if (pintid.0 as usize) < LOCAL_INTID_COUNT {
            routing.pirqs.get_local(source_vcpu, pintid)?
        } else {
            None
        };

        let entry = if let Some(local_entry) = local_entry {
            local_entry
        } else {
            let Some(global_entry) = routing.pirqs.get(pintid)? else {
                return Ok(None);
            };
            global_entry
        };

        let targets = if matches!(entry.delivery, PirqDelivery::Spi { .. }) {
            routing
                .routing
                .targets_for_spi(entry.vintid, self.common.vcpu_count())?
        } else {
            VcpuMask::EMPTY
        };

        Ok(Some((entry, targets)))
    }

    pub(crate) fn physical_irq_binding_kind_inner(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<PhysicalIrqBindingKind>, GicError> {
        let Some((entry, _)) = self.physical_irq_binding_inner(source_vcpu, pintid)? else {
            return Ok(None);
        };
        Ok(Some(entry.delivery.cpu_mode().binding_kind()))
    }

    pub(crate) fn physical_irq_guest_state_inner(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<PhysicalIrqGuestState>, GicError> {
        let Some((entry, _)) = self.physical_irq_binding_inner(source_vcpu, pintid)? else {
            return Ok(None);
        };

        let scope = entry.scope();
        let regs = self.common.regs_lock.lock_irqsave();
        let attrs = regs.irq_state.irq_attrs(scope, entry.vintid)?;
        let distributor_enable = match attrs.group {
            IrqGroup::Group0 => regs.dist_enable.0,
            IrqGroup::Group1 => regs.dist_enable.1,
        };

        Ok(Some(PhysicalIrqGuestState {
            binding_kind: entry.delivery.cpu_mode().binding_kind(),
            guest_enable: attrs.enable,
            distributor_enable,
        }))
    }
}
