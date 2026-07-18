pub(crate) mod irq_state;
pub(crate) mod pirq;
pub(crate) mod routing;
pub(crate) mod vcpu_array;

use crate::GicError;
use crate::IrqGroup;
use crate::IrqState as IrqStateKind;
use crate::PIntId;
use crate::VIntId;
use crate::VcpuId;
use crate::VcpuMask;
use crate::VgicIrqScope;
use crate::VgicTargets;
use crate::VgicUpdate;
use crate::VgicVcpuModel;
use crate::VgicVcpuQueue;
use crate::VirtualInterrupt;
use aarch64_mutex::RawSpinLockIrqSave;

use irq_state::IrqAttrs;
use irq_state::IrqState as IrqStateTable;
use pirq::CpuDeliveryMode;
use pirq::PirqDelivery;
use pirq::PirqTable;
use routing::SpiRouting;
use vcpu_array::VcpuArray;

pub(crate) use irq_state::LOCAL_INTID_COUNT;
pub(crate) use irq_state::SGI_COUNT;

pub(crate) struct RegsState<const VCPUS: usize>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    pub(crate) dist_enable: (bool, bool),
    pub(crate) irq_state: IrqStateTable<VCPUS>,
}

pub(crate) struct RoutingState<const VCPUS: usize>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    pub(crate) routing: SpiRouting<VCPUS>,
    pub(crate) pirqs: PirqTable<VCPUS>,
    pub(crate) pirq_manager_ctx: *mut (),
    pub(crate) pirq_hook: Option<&'static dyn common::PirqLifecycleHook>,
}

// SAFETY: `RoutingState` is only accessed through `routing_lock`, so moving it between threads is
// synchronized by the lock. `pirq_manager_ctx` is an opaque manager pointer installed during init
// and only copied/snapshotted before use; dereference happens in explicitly-audited `unsafe` paths.
unsafe impl<const VCPUS: usize> Send for RoutingState<VCPUS>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
}

pub(crate) struct VmCommon<const VCPUS: usize, V: VgicVcpuModel>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    pub(crate) vcpu_count: usize,
    pub(crate) vcpus: VcpuArray<VCPUS, V>,
    // Lock ordering: if a path needs both, always acquire routing_lock first, then regs_lock.
    pub(crate) regs_lock: RawSpinLockIrqSave<RegsState<VCPUS>>,
    pub(crate) routing_lock: RawSpinLockIrqSave<RoutingState<VCPUS>>,
}

impl<const VCPUS: usize, V: VgicVcpuModel> VmCommon<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    pub(crate) fn new(vcpu_count: usize, make: impl FnMut(VcpuId) -> V) -> Result<Self, GicError> {
        Ok(Self {
            vcpu_count,
            vcpus: VcpuArray::new_with(vcpu_count, make)?,
            regs_lock: RawSpinLockIrqSave::new(RegsState {
                dist_enable: (false, false),
                irq_state: IrqStateTable::new(vcpu_count),
            }),
            routing_lock: RawSpinLockIrqSave::new(RoutingState {
                routing: SpiRouting::new(),
                pirqs: PirqTable::new(),
                pirq_manager_ctx: core::ptr::null_mut(),
                pirq_hook: None,
            }),
        })
    }

    pub(crate) fn vcpu_count(&self) -> usize {
        self.vcpu_count
    }

    pub(crate) fn vcpu(&self, id: VcpuId) -> Result<&V, GicError> {
        self.vcpus.get(id.0 as usize).ok_or(GicError::InvalidVcpuId)
    }

    pub(crate) fn vcpu_index(&self, id: VcpuId) -> Result<usize, GicError> {
        if (id.0 as usize) < self.vcpu_count {
            Ok(id.0 as usize)
        } else {
            Err(GicError::InvalidVcpuId)
        }
    }

    #[inline(always)]
    fn dist_enabled_from(dist_enable: (bool, bool), group: IrqGroup) -> bool {
        match group {
            IrqGroup::Group0 => dist_enable.0,
            IrqGroup::Group1 => dist_enable.1,
        }
    }

    pub(crate) fn irq_attrs(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
    ) -> Result<IrqAttrs, GicError> {
        let regs = self.regs_lock.lock_irqsave();
        regs.irq_state.irq_attrs(scope, vintid)
    }

    pub(crate) fn targets_for_global_spi(&self, vintid: VIntId) -> Result<VcpuMask, GicError> {
        let routing = self.routing_lock.lock_irqsave();
        routing.routing.targets_for_spi(vintid, self.vcpu_count)
    }

    pub(crate) fn pirq_binding(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
    ) -> Result<Option<(PIntId, PirqDelivery)>, GicError> {
        let routing = self.routing_lock.lock_irqsave();
        match scope {
            VgicIrqScope::Local(target) => {
                let Some(pintid) = routing.pirqs.v2p_local(target, vintid) else {
                    return Ok(None);
                };
                let Some(entry) = routing.pirqs.get_local(target, pintid)? else {
                    return Ok(None);
                };
                Ok(Some((pintid, entry.delivery)))
            }
            VgicIrqScope::Global => {
                let Some(pintid) = routing.pirqs.v2p(vintid) else {
                    return Ok(None);
                };
                let Some(entry) = routing.pirqs.get(pintid)? else {
                    return Ok(None);
                };
                Ok(Some((pintid, entry.delivery)))
            }
        }
    }
}

impl<const VCPUS: usize, V: VgicVcpuModel + VgicVcpuQueue> VmCommon<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    #[inline(always)]
    fn state_from_attrs(attrs: IrqAttrs) -> IrqStateKind {
        if attrs.pending && attrs.active {
            IrqStateKind::PendingActive
        } else if attrs.pending {
            IrqStateKind::Pending
        } else if attrs.active {
            IrqStateKind::Active
        } else {
            IrqStateKind::Inactive
        }
    }

    pub(crate) fn enqueue_to_target(
        &self,
        target: VcpuId,
        virq: VirtualInterrupt,
    ) -> Result<VgicUpdate, GicError> {
        let work = self.vcpu(target)?.enqueue_irq(virq)?;
        Ok(VgicUpdate::Some {
            targets: VgicTargets::One(target),
            work,
        })
    }

    pub(crate) fn maybe_enqueue_irq(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
        source: Option<VcpuId>,
    ) -> Result<VgicUpdate, GicError> {
        match scope {
            VgicIrqScope::Local(vcpu) => {
                let binding = self.pirq_binding(scope, vintid)?;
                let attrs = {
                    let regs = self.regs_lock.lock_irqsave();
                    let attrs = regs.irq_state.irq_attrs(scope, vintid)?;
                    if !attrs.pending
                        || !attrs.enable
                        || !Self::dist_enabled_from(regs.dist_enable, attrs.group)
                    {
                        return Ok(VgicUpdate::None);
                    }
                    attrs
                };

                let irq = match binding {
                    Some((pintid, delivery))
                        if matches!(delivery.cpu_mode(), CpuDeliveryMode::HardwareLr) =>
                    {
                        VirtualInterrupt::Hardware {
                            vintid: vintid.0,
                            pintid: pintid.0,
                            priority: attrs.priority,
                            group: attrs.group,
                            state: Self::state_from_attrs(attrs),
                            source,
                        }
                    }
                    Some((pintid, _)) => VirtualInterrupt::Software {
                        vintid: vintid.0,
                        pintid: Some(pintid.0),
                        eoi_maintenance: true,
                        priority: attrs.priority,
                        group: attrs.group,
                        state: Self::state_from_attrs(attrs),
                        source,
                    },
                    None => VirtualInterrupt::Software {
                        vintid: vintid.0,
                        pintid: None,
                        eoi_maintenance: false,
                        priority: attrs.priority,
                        group: attrs.group,
                        state: Self::state_from_attrs(attrs),
                        source,
                    },
                };
                self.enqueue_to_target(vcpu, irq)
            }
            VgicIrqScope::Global => {
                let binding = self.pirq_binding(scope, vintid)?;
                let targets = {
                    let routing = self.routing_lock.lock_irqsave();
                    routing.routing.targets_for_spi(vintid, self.vcpu_count)?
                };
                if targets.is_empty() {
                    return Ok(VgicUpdate::None);
                }

                let attrs = {
                    let regs = self.regs_lock.lock_irqsave();
                    let attrs = regs.irq_state.irq_attrs(scope, vintid)?;
                    if !attrs.pending
                        || !attrs.enable
                        || !Self::dist_enabled_from(regs.dist_enable, attrs.group)
                    {
                        return Ok(VgicUpdate::None);
                    }
                    attrs
                };

                let irq = match binding {
                    Some((pintid, delivery))
                        if matches!(delivery.cpu_mode(), CpuDeliveryMode::HardwareLr) =>
                    {
                        VirtualInterrupt::Hardware {
                            vintid: vintid.0,
                            pintid: pintid.0,
                            priority: attrs.priority,
                            group: attrs.group,
                            state: Self::state_from_attrs(attrs),
                            source,
                        }
                    }
                    Some((pintid, _)) => VirtualInterrupt::Software {
                        vintid: vintid.0,
                        pintid: Some(pintid.0),
                        eoi_maintenance: true,
                        priority: attrs.priority,
                        group: attrs.group,
                        state: Self::state_from_attrs(attrs),
                        source,
                    },
                    None => VirtualInterrupt::Software {
                        vintid: vintid.0,
                        pintid: None,
                        eoi_maintenance: false,
                        priority: attrs.priority,
                        group: attrs.group,
                        state: Self::state_from_attrs(attrs),
                        source,
                    },
                };

                let mut update = VgicUpdate::None;
                for target in targets.iter() {
                    update.combine(&self.enqueue_to_target(target, irq)?);
                }
                Ok(update)
            }
        }
    }
}
