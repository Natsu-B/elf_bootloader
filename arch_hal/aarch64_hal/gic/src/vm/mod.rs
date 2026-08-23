use self::common::LOCAL_INTID_COUNT;
use self::common::SGI_COUNT;
use self::common::VmCommon;
use self::common::pirq::CpuDeliveryMode;
use self::common::pirq::DistMirrorMode;
use self::common::pirq::PirqDelivery;
use self::v2_ext::V2SgiState;
use crate::GicError;
use crate::GicMirrorOp;
use crate::GicMirrorRoute;
use crate::GicMirrorScope;
use crate::IrqGroup;
use crate::IrqSense;
use crate::PIntId;
use crate::PhysicalIrqBindingKind;
use crate::PhysicalIrqGuestState;
use crate::TriggerMode;
use crate::VIntId;
use crate::VSpiRouting;
use crate::VcpuId;
use crate::VcpuMask;
use crate::VgicGuestRegs;
use crate::VgicIrqScope;
use crate::VgicPirqModel;
use crate::VgicTargets;
use crate::VgicUpdate;
use crate::VgicVcpuModel;
use crate::VgicVcpuQueue;
use crate::VgicVmInfo;
use crate::VgicWork;
use crate::vm::vcpu::GicVCpuGeneric;
pub use ::common::PirqHookError;
pub use ::common::PirqHookOp;
pub use ::common::PirqLifecycleHook;
use aarch64_mutex::RawSpinLockIrqSave;

pub(crate) mod common;
pub mod manager;
mod v2_ext;
pub(crate) mod vcpu;

pub const fn pending_cap_for_vcpus(vcpus: usize) -> usize {
    crate::max_intids_for_vcpus(vcpus)
        .saturating_sub(SGI_COUNT)
        .saturating_add(SGI_COUNT.saturating_mul(vcpus))
}

/// Virtual Distributor model: per-vCPU private state for INTIDs 0-31 and shared SPI state for 32+.
pub(crate) struct GicVmModelGeneric<const VCPUS: usize, V: VgicVcpuModel>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    common: VmCommon<VCPUS, V>,
    v2: RawSpinLockIrqSave<V2SgiState<VCPUS>>,
}

pub(crate) type GicVmModelForVcpus<const VCPUS: usize> = GicVmModelGeneric<
    VCPUS,
    GicVCpuGeneric<
        VCPUS,
        { crate::max_intids_for_vcpus(VCPUS) },
        { crate::VgicVmConfig::<VCPUS>::MAX_LRS },
        { pending_cap_for_vcpus(VCPUS) },
    >,
>;

impl<const VCPUS: usize>
    GicVmModelGeneric<
        VCPUS,
        GicVCpuGeneric<
            VCPUS,
            { crate::max_intids_for_vcpus(VCPUS) },
            { crate::VgicVmConfig::<VCPUS>::MAX_LRS },
            { pending_cap_for_vcpus(VCPUS) },
        >,
    >
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    /// Construct a VM with vCPU ids fixed to `VcpuId(0..vcpu_count-1)`; callers must not assume
    /// alternative vCPU id mappings when using this backend.
    pub(crate) fn new(vcpu_count: u16) -> Result<Self, GicError> {
        Self::new_with(vcpu_count, |id| GicVCpuGeneric::with_id(id))
    }
}

impl<const VCPUS: usize, V: VgicVcpuModel> GicVmModelGeneric<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    const fn global_intids() -> usize {
        crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT
    }

    /// Construct a VM with vCPU ids fixed to `VcpuId(0..vcpu_count-1)`; custom mappings are not
    /// supported because CPUID fields and banked Distributor state rely on contiguous ids.
    pub(crate) fn new_with(
        vcpu_count: u16,
        make: impl FnMut(VcpuId) -> V,
    ) -> Result<Self, GicError> {
        let vcpu_count_usize = vcpu_count as usize;
        if vcpu_count_usize == 0 {
            return Err(GicError::InvalidVcpuId);
        }
        if vcpu_count_usize > VCPUS {
            return Err(GicError::OutOfResources);
        }

        Ok(Self {
            common: VmCommon::new(vcpu_count_usize, make)?,
            v2: RawSpinLockIrqSave::new(V2SgiState::new()),
        })
    }

    fn update_for_scope(scope: VgicIrqScope, changed: bool) -> VgicUpdate {
        if !changed {
            return VgicUpdate::None;
        }
        let targets = match scope {
            VgicIrqScope::Local(vcpu) => VgicTargets::One(vcpu),
            VgicIrqScope::Global => VgicTargets::All,
        };
        VgicUpdate::Some {
            targets,
            work: VgicWork::REFILL,
        }
    }

    pub(crate) fn set_pirq_manager_ctx(&self, ctx: *mut ()) {
        let mut routing = self.common.routing_lock.lock_irqsave();
        routing.pirq_manager_ctx = ctx;
    }

    pub(crate) fn set_pirq_hook(&self, hook: Option<&'static dyn PirqLifecycleHook>) {
        let mut routing = self.common.routing_lock.lock_irqsave();
        routing.pirq_hook = hook;
    }

    fn pirq_hook_snapshot(&self) -> (*mut (), Option<&'static dyn PirqLifecycleHook>) {
        let routing = self.common.routing_lock.lock_irqsave();
        (routing.pirq_manager_ctx, routing.pirq_hook)
    }

    fn map_pirq_hook_error(err: PirqHookError) -> GicError {
        match err {
            PirqHookError::InvalidState => GicError::InvalidState,
            PirqHookError::Unsupported => GicError::UnsupportedFeature,
            PirqHookError::InvalidInput => GicError::UnsupportedIntId,
        }
    }

    fn route_targets_to_bits(mask: VcpuMask) -> Result<u32, PirqHookError> {
        let mut bits = 0u32;
        for target in mask.iter() {
            if target.0 >= 32 {
                return Err(PirqHookError::Unsupported);
            }
            bits |= 1u32 << target.0;
        }
        Ok(bits)
    }

    fn irq_group_to_hook(group: IrqGroup) -> u8 {
        match group {
            IrqGroup::Group0 => 0,
            IrqGroup::Group1 => 1,
        }
    }

    fn trigger_to_sense(trigger: TriggerMode) -> IrqSense {
        match trigger {
            TriggerMode::Edge => IrqSense::Edge,
            TriggerMode::Level => IrqSense::Level,
        }
    }

    fn route_targets_to_gicv2_mask(mask: VcpuMask) -> Result<u8, GicError> {
        let mut bits = 0u8;
        for target in mask.iter() {
            if target.0 >= 8 {
                return Err(GicError::UnsupportedFeature);
            }
            bits |= 1u8 << target.0;
        }
        Ok(bits)
    }

    fn mirror_scope(scope: VgicIrqScope) -> GicMirrorScope {
        match scope {
            VgicIrqScope::Local(target) => GicMirrorScope::Local(target),
            VgicIrqScope::Global => GicMirrorScope::Global,
        }
    }

    fn pirq_binding_for_irq(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
    ) -> Result<Option<(PIntId, PirqDelivery)>, GicError> {
        self.common.pirq_binding(scope, vintid)
    }

    fn emit_mirror_op(&self, op: GicMirrorOp) -> Result<(), GicError>
    where
        [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
        [(); pending_cap_for_vcpus(VCPUS)]:,
    {
        let (ctx, _) = self.pirq_hook_snapshot();
        if ctx.is_null() {
            return Ok(());
        }

        // SAFETY: `ctx` is installed by the manager and points to a stable `VgicManager`
        // for this VM model instance. This call happens after dropping VM locks.
        unsafe { manager::apply_mirror_op_from_ctx::<VCPUS>(ctx, op) }
    }

    fn call_pirq_enable_hook(
        &self,
        pintid: PIntId,
        vintid: VIntId,
        enable: bool,
    ) -> Result<(), GicError> {
        let (_, hook) = self.pirq_hook_snapshot();
        let Some(hook) = hook else {
            return Ok(());
        };

        let route = {
            let routing = self.common.routing_lock.lock_irqsave();
            match routing.routing.get_route(vintid)? {
                VSpiRouting::Targets(mask) => mask,
                VSpiRouting::Specific(_) | VSpiRouting::AnyParticipating => {
                    return Err(GicError::UnsupportedFeature);
                }
            }
        };

        let (group, priority, trigger) = {
            let regs = self.common.regs_lock.lock_irqsave();
            let attrs = regs.irq_state.irq_attrs(VgicIrqScope::Global, vintid)?;
            let trigger = regs.irq_state.trigger_mode(VgicIrqScope::Global, vintid)?;
            (attrs.group, attrs.priority, trigger)
        };

        hook.on_pirq_event(
            pintid.0,
            PirqHookOp::Configure {
                group: Self::irq_group_to_hook(group),
                priority,
                trigger,
                targets: Self::route_targets_to_bits(route).map_err(Self::map_pirq_hook_error)?,
                enable,
            },
        )
        .map_err(Self::map_pirq_hook_error)
    }

    pub(crate) fn dispatch_pirq_notifications(
        &self,
        source_vcpu: VcpuId,
        notifs: &crate::PirqNotifications,
    ) -> Result<VgicUpdate, GicError>
    where
        V: VgicVcpuQueue,
        [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
        [(); pending_cap_for_vcpus(VCPUS)]:,
    {
        let mut update = VgicUpdate::None;

        for pintid in notifs.eoi.iter() {
            self.call_pirq_signal_hook(pintid, PirqHookOp::Eoi)?;
        }
        for pintid in notifs.deactivate.iter() {
            self.call_pirq_signal_hook(pintid, PirqHookOp::Deactivate)?;
        }
        for pintid in notifs.resample.iter() {
            if let Some(resample_update) =
                self.resample_local_pirq_without_hook(source_vcpu, pintid)?
            {
                update.combine(&resample_update);
            } else {
                self.call_pirq_signal_hook(pintid, PirqHookOp::Resample)?;
            }
        }
        Ok(update)
    }

    fn resample_local_pirq_without_hook(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<VgicUpdate>, GicError>
    where
        V: VgicVcpuQueue,
        [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
        [(); pending_cap_for_vcpus(VCPUS)]:,
    {
        if (pintid.0 as usize) >= LOCAL_INTID_COUNT {
            return Ok(None);
        }

        let (_, hook) = self.pirq_hook_snapshot();
        if hook.is_some() {
            return Ok(None);
        }

        let maybe_local = {
            let routing = self.common.routing_lock.lock_irqsave();
            routing.pirqs.get_local(source_vcpu, pintid)?
        };
        let Some(entry) = maybe_local else {
            return Ok(None);
        };

        let PirqDelivery::Local {
            dist_mode,
            cpu_mode,
            ..
        } = entry.delivery
        else {
            return Ok(None);
        };

        if dist_mode != DistMirrorMode::WriteThrough || cpu_mode != CpuDeliveryMode::HardwareLr {
            return Ok(None);
        }

        self.on_physical_irq_inner(source_vcpu, pintid, true)
            .map(Some)
    }

    fn call_pirq_signal_hook(&self, pintid: PIntId, op: PirqHookOp) -> Result<(), GicError> {
        let (_, hook) = self.pirq_hook_snapshot();
        if let Some(hook) = hook {
            hook.on_pirq_event(pintid.0, op)
                .map_err(Self::map_pirq_hook_error)?;
        }
        Ok(())
    }
}

impl<const VCPUS: usize, V: VgicVcpuModel> VgicVmInfo for GicVmModelGeneric<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    type VcpuModel = V;

    fn vcpu_count(&self) -> u16 {
        self.common.vcpu_count() as u16
    }

    fn vcpu(&self, id: VcpuId) -> Result<&Self::VcpuModel, GicError> {
        self.common.vcpu(id)
    }
}

impl<const VCPUS: usize, V: VgicVcpuModel + VgicVcpuQueue> VgicGuestRegs
    for GicVmModelGeneric<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); pending_cap_for_vcpus(VCPUS)]:,
{
    fn set_dist_enable(
        &self,
        enable_grp0: bool,
        enable_grp1: bool,
    ) -> Result<VgicUpdate, GicError> {
        let (prev, changed) = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            let prev = regs.dist_enable;
            let next = (enable_grp0, enable_grp1);
            let changed = prev != next;
            regs.dist_enable = next;
            (prev, changed)
        };

        let mut update = VgicUpdate::None;
        if (!prev.0 && enable_grp0) || (!prev.1 && enable_grp1) {
            for vcpu_idx in 0..self.common.vcpu_count() {
                let vcpu = VcpuId(vcpu_idx as u16);
                for intid in 0..LOCAL_INTID_COUNT {
                    if intid < SGI_COUNT {
                        continue;
                    }
                    let vintid = VIntId(intid as u32);
                    let attrs = self.common.irq_attrs(VgicIrqScope::Local(vcpu), vintid)?;
                    let group_enabled = match attrs.group {
                        IrqGroup::Group0 => enable_grp0,
                        IrqGroup::Group1 => enable_grp1,
                    };
                    if attrs.pending && attrs.enable && group_enabled {
                        update.combine(&self.common.maybe_enqueue_irq(
                            VgicIrqScope::Local(vcpu),
                            vintid,
                            None,
                        )?);
                    }
                }
                for sgi in 0..SGI_COUNT {
                    let attrs = self
                        .common
                        .irq_attrs(VgicIrqScope::Local(vcpu), VIntId(sgi as u32))?;
                    let group_enabled = match attrs.group {
                        IrqGroup::Group0 => enable_grp0,
                        IrqGroup::Group1 => enable_grp1,
                    };
                    if attrs.pending && attrs.enable && group_enabled {
                        update.combine(&self.enqueue_sgi_for_target(vcpu, sgi)?);
                    }
                }
            }

            for spi_idx in 0..Self::global_intids() {
                let vintid = VIntId((LOCAL_INTID_COUNT + spi_idx) as u32);
                let attrs = self.common.irq_attrs(VgicIrqScope::Global, vintid)?;
                let group_enabled = match attrs.group {
                    IrqGroup::Group0 => enable_grp0,
                    IrqGroup::Group1 => enable_grp1,
                };
                if attrs.pending && attrs.enable && group_enabled {
                    update.combine(&self.common.maybe_enqueue_irq(
                        VgicIrqScope::Global,
                        vintid,
                        None,
                    )?);
                }
            }
        }

        update.combine(&Self::update_for_scope(VgicIrqScope::Global, changed));
        Ok(update)
    }

    fn dist_enable(&self) -> Result<(bool, bool), GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        Ok(regs.dist_enable)
    }

    fn set_group(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
        group: IrqGroup,
    ) -> Result<VgicUpdate, GicError> {
        let changed = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state.set_group(scope, vintid, group)?
        };
        Ok(Self::update_for_scope(scope, changed))
    }

    fn set_priority(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
        priority: u8,
    ) -> Result<VgicUpdate, GicError> {
        let changed = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state.set_priority(scope, vintid, priority)?
        };
        Ok(Self::update_for_scope(scope, changed))
    }

    fn set_trigger(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
        trigger: TriggerMode,
    ) -> Result<VgicUpdate, GicError> {
        let changed = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state.set_trigger(scope, vintid, trigger)?
        };
        Ok(Self::update_for_scope(scope, changed))
    }

    fn set_enable(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
        enable: bool,
    ) -> Result<VgicUpdate, GicError> {
        let changed = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state.set_enable(scope, vintid, enable)?
        };
        let mut update = VgicUpdate::None;
        let intid = vintid.0 as usize;

        if changed {
            if let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? {
                if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                    self.emit_mirror_op(GicMirrorOp::SetEnable {
                        scope: Self::mirror_scope(scope),
                        intid: pintid.0,
                        enable,
                    })?;
                }
                if matches!(scope, VgicIrqScope::Global) {
                    self.call_pirq_enable_hook(pintid, vintid, enable)?;
                }
            }
        }

        if enable {
            match scope {
                VgicIrqScope::Local(vcpu) if intid < SGI_COUNT => {
                    update.combine(&self.enqueue_sgi_for_target(vcpu, intid)?);
                }
                _ => {
                    let attrs = self.common.irq_attrs(scope, vintid)?;
                    if attrs.pending {
                        update.combine(&self.common.maybe_enqueue_irq(scope, vintid, None)?);
                    }
                }
            }
        } else {
            update.combine(&self.cancel_for_scope(scope, vintid, None)?);
        }

        update.combine(&Self::update_for_scope(scope, changed));
        Ok(update)
    }

    fn set_pending(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
        pending: bool,
    ) -> Result<VgicUpdate, GicError> {
        let changed = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state.set_pending(scope, vintid, pending)?
        };
        let mut update = VgicUpdate::None;
        let intid = vintid.0 as usize;

        if pending {
            match scope {
                VgicIrqScope::Local(vcpu) if intid < SGI_COUNT => {
                    update.combine(&self.enqueue_sgi_for_target(vcpu, intid)?);
                }
                _ => update.combine(&self.common.maybe_enqueue_irq(scope, vintid, None)?),
            }
        } else {
            update.combine(&self.cancel_for_scope(scope, vintid, None)?);
        }

        update.combine(&Self::update_for_scope(scope, changed));
        Ok(update)
    }

    fn read_group_word(&self, scope: VgicIrqScope, base: VIntId) -> Result<u32, GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        regs.irq_state.read_group_word(scope, base)
    }

    fn write_group_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        value: u32,
    ) -> Result<VgicUpdate, GicError> {
        let (after, changed_mask) = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            let before = regs.irq_state.read_group_word(scope, base)?;
            let _changed = regs.irq_state.write_group_word(scope, base, value)?;
            let after = regs.irq_state.read_group_word(scope, base)?;
            (after, before ^ after)
        };

        for bit in 0..32 {
            let lane_mask = 1u32 << bit;
            if (changed_mask & lane_mask) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? else {
                continue;
            };
            if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                self.emit_mirror_op(GicMirrorOp::SetGroup {
                    scope: Self::mirror_scope(scope),
                    intid: pintid.0,
                    group: if (after & lane_mask) != 0 {
                        IrqGroup::Group1
                    } else {
                        IrqGroup::Group0
                    },
                })?;
            }
        }

        Ok(Self::update_for_scope(scope, changed_mask != 0))
    }

    fn read_enable_word(&self, scope: VgicIrqScope, base: VIntId) -> Result<u32, GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        regs.irq_state.read_enable_word(scope, base)
    }

    fn write_set_enable_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        set_bits: u32,
    ) -> Result<VgicUpdate, GicError> {
        let mask = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state
                .write_set_enable_word(scope, base, set_bits)?
        };
        let mut update = VgicUpdate::None;

        for bit in 0..32 {
            if (mask & (1u32 << bit)) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            if let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? {
                if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                    self.emit_mirror_op(GicMirrorOp::SetEnable {
                        scope: Self::mirror_scope(scope),
                        intid: pintid.0,
                        enable: true,
                    })?;
                }
                if matches!(scope, VgicIrqScope::Global) {
                    self.call_pirq_enable_hook(pintid, vintid, true)?;
                }
            }
            match scope {
                VgicIrqScope::Local(vcpu) if (base.0 + bit) < SGI_COUNT as u32 => {
                    update.combine(&self.enqueue_sgi_for_target(vcpu, (base.0 + bit) as usize)?);
                }
                _ => {
                    let attrs = self.common.irq_attrs(scope, vintid)?;
                    if attrs.pending {
                        update.combine(&self.common.maybe_enqueue_irq(scope, vintid, None)?);
                    }
                }
            }
        }

        update.combine(&Self::update_for_scope(scope, mask != 0));
        Ok(update)
    }

    fn write_clear_enable_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        clear_bits: u32,
    ) -> Result<VgicUpdate, GicError> {
        let mask = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state
                .write_clear_enable_word(scope, base, clear_bits)?
        };
        let mut update = VgicUpdate::None;

        for bit in 0..32 {
            if (mask & (1u32 << bit)) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            if let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? {
                if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                    self.emit_mirror_op(GicMirrorOp::SetEnable {
                        scope: Self::mirror_scope(scope),
                        intid: pintid.0,
                        enable: false,
                    })?;
                }
                if matches!(scope, VgicIrqScope::Global) {
                    self.call_pirq_enable_hook(pintid, vintid, false)?;
                }
            }
            update.combine(&self.cancel_for_scope(scope, vintid, None)?);
        }

        update.combine(&Self::update_for_scope(scope, mask != 0));
        Ok(update)
    }

    fn read_pending_word(&self, scope: VgicIrqScope, base: VIntId) -> Result<u32, GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        regs.irq_state.read_pending_word(scope, base)
    }

    fn write_set_pending_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        set_bits: u32,
    ) -> Result<VgicUpdate, GicError> {
        let mask = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state
                .write_set_pending_word(scope, base, set_bits)?
        };
        let mut update = VgicUpdate::None;

        for bit in 0..32 {
            if (mask & (1u32 << bit)) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            if let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? {
                if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                    self.emit_mirror_op(GicMirrorOp::SetPending {
                        scope: Self::mirror_scope(scope),
                        intid: pintid.0,
                        pending: true,
                    })?;
                }
            }
            match scope {
                VgicIrqScope::Local(vcpu) if (base.0 + bit) < SGI_COUNT as u32 => {
                    update.combine(&self.enqueue_sgi_for_target(vcpu, (base.0 + bit) as usize)?);
                }
                _ => update.combine(&self.common.maybe_enqueue_irq(scope, vintid, None)?),
            }
        }

        update.combine(&Self::update_for_scope(scope, mask != 0));
        Ok(update)
    }

    fn write_clear_pending_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        clear_bits: u32,
    ) -> Result<VgicUpdate, GicError> {
        let mask = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state
                .write_clear_pending_word(scope, base, clear_bits)?
        };
        let mut update = VgicUpdate::None;

        for bit in 0..32 {
            if (mask & (1u32 << bit)) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            if let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? {
                if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                    self.emit_mirror_op(GicMirrorOp::SetPending {
                        scope: Self::mirror_scope(scope),
                        intid: pintid.0,
                        pending: false,
                    })?;
                }
            }
            update.combine(&self.cancel_for_scope(scope, vintid, None)?);
        }

        update.combine(&Self::update_for_scope(scope, mask != 0));
        Ok(update)
    }

    fn read_active_word(&self, scope: VgicIrqScope, base: VIntId) -> Result<u32, GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        regs.irq_state.read_active_word(scope, base)
    }

    fn write_set_active_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        set_bits: u32,
    ) -> Result<VgicUpdate, GicError> {
        let mask = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state
                .write_set_active_word(scope, base, set_bits)?
        };

        for bit in 0..32 {
            if (mask & (1u32 << bit)) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? else {
                continue;
            };
            if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                self.emit_mirror_op(GicMirrorOp::SetActive {
                    scope: Self::mirror_scope(scope),
                    intid: pintid.0,
                    active: true,
                })?;
            }
        }

        Ok(Self::update_for_scope(scope, mask != 0))
    }

    fn write_clear_active_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        clear_bits: u32,
    ) -> Result<VgicUpdate, GicError> {
        let mask = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            regs.irq_state
                .write_clear_active_word(scope, base, clear_bits)?
        };

        for bit in 0..32 {
            if (mask & (1u32 << bit)) == 0 {
                continue;
            }
            let vintid = VIntId(base.0 + bit);
            let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? else {
                continue;
            };
            if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                self.emit_mirror_op(GicMirrorOp::SetActive {
                    scope: Self::mirror_scope(scope),
                    intid: pintid.0,
                    active: false,
                })?;
            }
        }

        Ok(Self::update_for_scope(scope, mask != 0))
    }

    fn read_priority_word(&self, scope: VgicIrqScope, base: VIntId) -> Result<u32, GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        regs.irq_state
            .read_priority_word_raw(scope, base.0 as usize)
    }

    fn write_priority_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        value: u32,
    ) -> Result<VgicUpdate, GicError> {
        let (before, after) = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            let before = regs
                .irq_state
                .read_priority_word_raw(scope, base.0 as usize)?;
            let _changed = regs
                .irq_state
                .write_priority_word_raw(scope, base.0 as usize, value)?;
            let after = regs
                .irq_state
                .read_priority_word_raw(scope, base.0 as usize)?;
            (before, after)
        };

        for lane in 0..4 {
            let shift = lane * 8;
            let before_lane = (before >> shift) & 0xff;
            let after_lane = (after >> shift) & 0xff;
            if before_lane == after_lane {
                continue;
            }

            let vintid = VIntId(base.0 + lane as u32);
            let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? else {
                continue;
            };
            if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                self.emit_mirror_op(GicMirrorOp::SetPriority {
                    scope: Self::mirror_scope(scope),
                    intid: pintid.0,
                    priority: after_lane as u8,
                })?;
            }
        }

        Ok(Self::update_for_scope(scope, before != after))
    }

    fn read_trigger_word(&self, scope: VgicIrqScope, base: VIntId) -> Result<u32, GicError> {
        let regs = self.common.regs_lock.lock_irqsave();
        regs.irq_state.read_trigger_word(scope, base)
    }

    fn write_trigger_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
        value: u32,
    ) -> Result<VgicUpdate, GicError> {
        let (before, after) = {
            let mut regs = self.common.regs_lock.lock_irqsave();
            let before = regs.irq_state.read_trigger_word(scope, base)?;
            let _changed = regs.irq_state.write_trigger_word(scope, base, value)?;
            let after = regs.irq_state.read_trigger_word(scope, base)?;
            (before, after)
        };

        for lane in 0..16 {
            let shift = lane * 2;
            let before_lane = (before >> shift) & 0b11;
            let after_lane = (after >> shift) & 0b11;
            if before_lane == after_lane {
                continue;
            }

            let vintid = VIntId(base.0 + lane as u32);
            let Some((pintid, delivery)) = self.pirq_binding_for_irq(scope, vintid)? else {
                continue;
            };
            if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                self.emit_mirror_op(GicMirrorOp::SetTrigger {
                    scope: Self::mirror_scope(scope),
                    intid: pintid.0,
                    trigger: if after_lane == 0b10 {
                        TriggerMode::Edge
                    } else {
                        TriggerMode::Level
                    },
                })?;
            }
        }

        Ok(Self::update_for_scope(scope, before != after))
    }

    fn set_spi_route(&self, vintid: VIntId, targets: VSpiRouting) -> Result<VgicUpdate, GicError> {
        match targets {
            VSpiRouting::Targets(mask) => {
                let vcpu_count = self.common.vcpu_count();
                for id in mask.iter() {
                    if (id.0 as usize) >= vcpu_count {
                        return Err(GicError::InvalidVcpuId);
                    }
                }

                let changed = {
                    let mut routing = self.common.routing_lock.lock_irqsave();
                    routing
                        .routing
                        .set_route(vintid, VSpiRouting::Targets(mask))?
                };

                if changed {
                    if let Some((pintid, delivery)) =
                        self.pirq_binding_for_irq(VgicIrqScope::Global, vintid)?
                    {
                        if matches!(delivery.dist_mode(), DistMirrorMode::WriteThrough) {
                            self.emit_mirror_op(GicMirrorOp::SetRoute {
                                intid: pintid.0,
                                route: GicMirrorRoute::Gicv2TargetMask(
                                    Self::route_targets_to_gicv2_mask(mask)?,
                                ),
                            })?;
                        }
                    }
                }

                Ok(Self::update_for_scope(VgicIrqScope::Global, changed))
            }
            VSpiRouting::Specific(_) | VSpiRouting::AnyParticipating => {
                Err(GicError::UnsupportedFeature)
            }
        }
    }

    fn get_spi_route(&self, vintid: VIntId) -> Result<VSpiRouting, GicError> {
        let routing = self.common.routing_lock.lock_irqsave();
        routing.routing.get_route(vintid)
    }
}

impl<const VCPUS: usize, V: VgicVcpuModel + VgicVcpuQueue> VgicPirqModel
    for GicVmModelGeneric<VCPUS, V>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); pending_cap_for_vcpus(VCPUS)]:,
{
    fn map_pirq(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
        sense: IrqSense,
        group: IrqGroup,
        priority: u8,
    ) -> Result<VgicUpdate, GicError> {
        self.map_pirq_inner_configured(pintid, target, vintid, sense, group, priority)
    }

    fn bind_local_pirq_passthrough(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_local_pirq_inner_passthrough(pintid, target, vintid)
    }

    fn bind_local_pirq_write_through_software_lr(
        &self,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_local_pirq_inner_write_through_software_lr(pintid, target, vintid)
    }

    fn bind_spi_pirq_passthrough(
        &self,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_spi_pirq_inner_passthrough(pintid, vintid)
    }

    fn bind_spi_pirq_write_through_software_lr(
        &self,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_spi_pirq_inner_write_through_software_lr(pintid, vintid)
    }

    fn bind_spi_pirq_shadow_software_lr(
        &self,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<VgicUpdate, GicError> {
        self.bind_spi_pirq_inner_shadow_software_lr(pintid, vintid)
    }

    #[cfg(test)]
    fn unmap_pirq(&self, pintid: PIntId) -> Result<VgicUpdate, GicError> {
        self.unmap_pirq_inner(pintid)
    }

    fn on_physical_irq(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
        level: bool,
    ) -> Result<VgicUpdate, GicError> {
        self.on_physical_irq_inner(source_vcpu, pintid, level)
    }

    fn physical_irq_binding_kind(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<PhysicalIrqBindingKind>, GicError> {
        self.physical_irq_binding_kind_inner(source_vcpu, pintid)
    }

    fn physical_irq_guest_state(
        &self,
        source_vcpu: VcpuId,
        pintid: PIntId,
    ) -> Result<Option<PhysicalIrqGuestState>, GicError> {
        self.physical_irq_guest_state_inner(source_vcpu, pintid)
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use self::common::pirq::CpuDeliveryMode;
    use super::*;
    use crate::EoiMode;
    use crate::GicIrqMirror;
    use crate::GicMirrorOp;
    use crate::GicMirrorRoute;
    use crate::GicMirrorScope;
    use crate::IrqState;
    use crate::MaintenanceReasons;
    use crate::PhysicalIrqBindingKind;
    use crate::PhysicalIrqGuestState;
    use crate::VcpuMask;
    use crate::VgicHw;
    use crate::VgicSgiRegs;
    use crate::VirtualInterrupt;
    use core::cell::RefCell;
    use core::sync::atomic::AtomicBool;
    use core::sync::atomic::Ordering;

    const TEST_VCPUS: usize = 4;

    struct EnqueueLog {
        count: usize,
        last: Option<VirtualInterrupt>,
    }

    impl EnqueueLog {
        fn new() -> Self {
            Self {
                count: 0,
                last: None,
            }
        }
    }

    struct CancelLog {
        count: usize,
        last: Option<(VIntId, Option<VcpuId>)>,
    }

    impl CancelLog {
        fn new() -> Self {
            Self {
                count: 0,
                last: None,
            }
        }
    }

    struct RecordingVcpu {
        enqueued: RefCell<EnqueueLog>,
        cancelled: RefCell<CancelLog>,
        present: RefCell<[Option<(VIntId, Option<VcpuId>)>; 8]>,
    }

    impl RecordingVcpu {
        fn new(_id: VcpuId) -> Self {
            Self {
                enqueued: RefCell::new(EnqueueLog::new()),
                cancelled: RefCell::new(CancelLog::new()),
                present: RefCell::new([None; 8]),
            }
        }
    }

    impl VgicVcpuModel for RecordingVcpu {
        fn set_resident(&self, _core: cpu::CoreAffinity) -> Result<(), GicError> {
            Ok(())
        }

        fn refill_lrs<H: crate::VgicHw>(&self, _hw: &H) -> Result<bool, GicError> {
            Ok(false)
        }

        fn handle_maintenance_collect<H: crate::VgicHw>(
            &self,
            _hw: &H,
        ) -> Result<(VgicUpdate, crate::PirqNotifications), GicError> {
            Ok((VgicUpdate::None, crate::PirqNotifications::new()))
        }

        fn switch_out_sync<H: crate::VgicHw>(&self, _hw: &H) -> Result<(), GicError> {
            Ok(())
        }

        fn contains_irq(&self, vintid: VIntId, source: Option<VcpuId>) -> bool {
            self.present
                .borrow()
                .iter()
                .flatten()
                .any(|entry| *entry == (vintid, source))
        }
    }

    impl VgicVcpuQueue for RecordingVcpu {
        fn enqueue_irq(&self, irq: VirtualInterrupt) -> Result<VgicWork, GicError> {
            let mut log = self.enqueued.borrow_mut();
            log.count += 1;
            log.last = Some(irq);

            let key = (VIntId(irq.vintid()), irq.source());
            let mut present = self.present.borrow_mut();
            if !present.iter().flatten().any(|entry| *entry == key)
                && let Some(slot) = present.iter_mut().find(|entry| entry.is_none())
            {
                *slot = Some(key);
            }
            Ok(VgicWork::REFILL)
        }

        fn cancel_irq(&self, vintid: VIntId, source: Option<VcpuId>) -> Result<(), GicError> {
            let mut log = self.cancelled.borrow_mut();
            log.count += 1;
            log.last = Some((vintid, source));

            let mut present = self.present.borrow_mut();
            if let Some(slot) = present
                .iter_mut()
                .find(|entry| **entry == Some((vintid, source)))
            {
                *slot = None;
            }
            Ok(())
        }
    }

    type RecordingVm = GicVmModelGeneric<TEST_VCPUS, RecordingVcpu>;

    fn recording_vm(vcpu_count: u16) -> RecordingVm {
        RecordingVm::new_with(vcpu_count, |id| RecordingVcpu::new(id)).unwrap()
    }

    fn assert_wf<T>() {}

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn gic_vm_model_for_vcpus_instantiates_common_sizes() {
        assert_wf::<GicVmModelForVcpus<1>>();
        assert_wf::<GicVmModelForVcpus<2>>();
        assert_wf::<GicVmModelForVcpus<4>>();
        assert_wf::<GicVmModelForVcpus<8>>();
        assert_wf::<GicVmModelForVcpus<16>>();
        assert_wf::<GicVmModelGeneric<4, RecordingVcpu>>();
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn vm_model_rejects_invalid_vcpu_counts() {
        assert!(matches!(
            RecordingVm::new_with(0, |id| RecordingVcpu::new(id)),
            Err(GicError::InvalidVcpuId)
        ));
        assert!(matches!(
            RecordingVm::new_with((TEST_VCPUS as u16) + 1, |id| RecordingVcpu::new(id)),
            Err(GicError::OutOfResources)
        ));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn pending_and_enabled_irq_is_enqueued() {
        let vm = recording_vm(1);
        vm.set_dist_enable(true, true).unwrap();
        vm.set_enable(VgicIrqScope::Local(VcpuId(0)), VIntId(5), true)
            .unwrap();
        let update = vm
            .set_pending(VgicIrqScope::Local(VcpuId(0)), VIntId(5), true)
            .unwrap();
        let vcpu = vm.vcpu(VcpuId(0)).unwrap();
        let enqueued = vcpu.enqueued.borrow();
        assert_eq!(enqueued.count, 1);
        match enqueued.last.as_ref().expect("expected virq") {
            VirtualInterrupt::Software { vintid, state, .. } => {
                assert_eq!(*vintid, 5);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected software pending virq"),
        }
        assert!(matches!(
            update,
            VgicUpdate::Some {
                targets: VgicTargets::One(VcpuId(0)),
                work
            } if work.refill
        ));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn pending_queued_after_enable() {
        let vm = recording_vm(1);
        vm.set_dist_enable(true, true).unwrap();
        vm.set_pending(VgicIrqScope::Local(VcpuId(0)), VIntId(7), true)
            .unwrap();
        {
            let vcpu = vm.vcpu(VcpuId(0)).unwrap();
            assert_eq!(vcpu.enqueued.borrow().count, 0);
        }
        vm.set_enable(VgicIrqScope::Local(VcpuId(0)), VIntId(7), true)
            .unwrap();
        let vcpu = vm.vcpu(VcpuId(0)).unwrap();
        assert_eq!(vcpu.enqueued.borrow().count, 1);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgi_sources_enqueue_with_sender() {
        let vm = recording_vm(2);
        vm.set_dist_enable(true, true).unwrap();
        vm.set_enable(VgicIrqScope::Local(VcpuId(1)), VIntId(0), true)
            .unwrap();
        vm.write_set_sgi_pending_sources_word(VcpuId(1), 0, 1)
            .unwrap();
        let vcpu = vm.vcpu(VcpuId(1)).unwrap();
        let enqueued = vcpu.enqueued.borrow();
        assert_eq!(enqueued.count, 1);
        match enqueued.last.as_ref().expect("expected SGI") {
            VirtualInterrupt::Software { vintid, source, .. } => {
                assert_eq!(*vintid, 0);
                assert_eq!(*source, Some(VcpuId(0)));
            }
            _ => panic!("expected software SGI"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn full_virtualization_still_uses_software_lr_delivery() {
        let vm = recording_vm(1);
        vm.set_dist_enable(true, true).unwrap();
        vm.map_pirq(
            PIntId(48),
            VcpuId(0),
            VIntId(40),
            IrqSense::Level,
            IrqGroup::Group1,
            0x20,
        )
        .unwrap();

        vm.on_physical_irq(VcpuId(0), PIntId(48), true).unwrap();
        let vcpu = vm.vcpu(VcpuId(0)).unwrap();
        match vcpu.enqueued.borrow().last.as_ref().expect("expected virq") {
            VirtualInterrupt::Software { vintid, state, .. } => {
                assert_eq!(*vintid, 40);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected software virq"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn passthrough_uses_hardware_lr_delivery() {
        let vm = recording_vm(1);
        let vintid = VIntId(40);
        let bit = 1u32 << (vintid.0 - 32);
        vm.set_dist_enable(true, true).unwrap();
        vm.bind_spi_pirq_passthrough(PIntId(48), vintid).unwrap();
        vm.write_set_enable_word(VgicIrqScope::Global, VIntId(32), bit)
            .unwrap();

        vm.on_physical_irq(VcpuId(0), PIntId(48), true).unwrap();
        let vcpu = vm.vcpu(VcpuId(0)).unwrap();
        match vcpu.enqueued.borrow().last.as_ref().expect("expected virq") {
            VirtualInterrupt::Hardware { pintid, state, .. } => {
                assert_eq!(*pintid, 48);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected hardware virq"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn write_through_local_ppi_software_lr_uses_software_delivery() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let pintid = PIntId(27);
        let vintid = VIntId(27);

        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_write_through_software_lr(pintid, target, vintid)
            .unwrap();
        vm.set_enable(VgicIrqScope::Local(target), vintid, true)
            .unwrap();

        {
            let routing = vm.common.routing_lock.lock_irqsave();
            let entry = routing
                .pirqs
                .get_local(target, pintid)
                .unwrap()
                .expect("expected pIRQ map");
            assert!(matches!(
                entry.delivery,
                PirqDelivery::Local {
                    target: entry_target,
                    dist_mode: DistMirrorMode::WriteThrough,
                    cpu_mode: CpuDeliveryMode::SoftwareLr,
                } if entry_target == target
            ));
        }

        assert_eq!(
            vm.physical_irq_binding_kind(target, pintid).unwrap(),
            Some(PhysicalIrqBindingKind::SoftwareLr)
        );

        reset_mirror_state();
        vm.write_group_word(VgicIrqScope::Local(target), VIntId(0), 1u32 << vintid.0)
            .unwrap();
        assert_eq!(recorded_mirror_count(), 1);
        assert_eq!(
            recorded_mirror_op(0),
            GicMirrorOp::SetGroup {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                group: IrqGroup::Group1,
            }
        );

        vm.on_physical_irq(target, pintid, true).unwrap();
        let vcpu = vm.vcpu(target).unwrap();
        match vcpu.enqueued.borrow().last.as_ref().expect("expected virq") {
            VirtualInterrupt::Software { vintid, state, .. } => {
                assert_eq!(*vintid, 27);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected software virq"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn passthrough_spi_binding_remains_write_through_hardware_lr() {
        let vm = recording_vm(1);
        let pintid = PIntId(48);
        let vintid = VIntId(40);

        vm.bind_spi_pirq_passthrough(pintid, vintid).unwrap();

        {
            let routing = vm.common.routing_lock.lock_irqsave();
            let entry = routing
                .pirqs
                .get(pintid)
                .unwrap()
                .expect("expected pIRQ map");
            assert!(matches!(
                entry.delivery,
                PirqDelivery::Spi {
                    dist_mode: DistMirrorMode::WriteThrough,
                    cpu_mode: CpuDeliveryMode::HardwareLr,
                }
            ));
        }

        assert_eq!(
            vm.physical_irq_binding_kind(VcpuId(0), pintid).unwrap(),
            Some(PhysicalIrqBindingKind::HardwareLr)
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn software_lr_spi_binding_remains_write_through() {
        let vm = recording_vm(1);
        let pintid = PIntId(48);
        let vintid = VIntId(40);

        vm.bind_spi_pirq_write_through_software_lr(pintid, vintid)
            .unwrap();

        {
            let routing = vm.common.routing_lock.lock_irqsave();
            let entry = routing
                .pirqs
                .get(pintid)
                .unwrap()
                .expect("expected pIRQ map");
            assert!(matches!(
                entry.delivery,
                PirqDelivery::Spi {
                    dist_mode: DistMirrorMode::WriteThrough,
                    cpu_mode: CpuDeliveryMode::SoftwareLr,
                }
            ));
        }

        assert_eq!(
            vm.physical_irq_binding_kind(VcpuId(0), pintid).unwrap(),
            Some(PhysicalIrqBindingKind::SoftwareLr)
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn software_lr_spi_binding_can_remain_shadow_only() {
        let vm = recording_vm(1);
        let pintid = PIntId(48);
        let vintid = VIntId(40);

        vm.bind_spi_pirq_shadow_software_lr(pintid, vintid).unwrap();

        {
            let routing = vm.common.routing_lock.lock_irqsave();
            let entry = routing
                .pirqs
                .get(pintid)
                .unwrap()
                .expect("expected pIRQ map");
            assert!(matches!(
                entry.delivery,
                PirqDelivery::Spi {
                    dist_mode: DistMirrorMode::ShadowOnly,
                    cpu_mode: CpuDeliveryMode::SoftwareLr,
                }
            ));
        }

        assert_eq!(
            vm.physical_irq_binding_kind(VcpuId(0), pintid).unwrap(),
            Some(PhysicalIrqBindingKind::SoftwareLr)
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn physical_irq_binding_kind_distinguishes_bound_local_ppi_from_unbound_ppi() {
        let vm = recording_vm(1);
        let target = VcpuId(0);
        let bound_pintid = PIntId(27);

        vm.bind_local_pirq_write_through_software_lr(bound_pintid, target, VIntId(27))
            .unwrap();

        assert_eq!(
            vm.physical_irq_binding_kind(target, bound_pintid).unwrap(),
            Some(PhysicalIrqBindingKind::SoftwareLr)
        );
        assert_eq!(
            vm.physical_irq_binding_kind(target, PIntId(28)).unwrap(),
            None
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn physical_irq_guest_state_reports_unbound_as_none() {
        let vm = recording_vm(1);
        let target = VcpuId(0);

        vm.bind_local_pirq_write_through_software_lr(PIntId(27), target, VIntId(27))
            .unwrap();

        assert_eq!(
            vm.physical_irq_guest_state(target, PIntId(28)).unwrap(),
            None
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn physical_irq_guest_state_for_local_ppi_is_false_until_guest_enable() {
        let vm = recording_vm(1);
        let target = VcpuId(0);
        let pintid = PIntId(27);
        let vintid = VIntId(27);
        let bit = 1u32 << vintid.0;

        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_write_through_software_lr(pintid, target, vintid)
            .unwrap();

        let state_before = vm
            .physical_irq_guest_state(target, pintid)
            .unwrap()
            .expect("expected bound pIRQ state");
        assert_eq!(
            state_before,
            PhysicalIrqGuestState {
                binding_kind: PhysicalIrqBindingKind::SoftwareLr,
                guest_enable: false,
                distributor_enable: true,
            }
        );
        assert!(!state_before.accepts_asserted_ingress());

        vm.write_set_enable_word(VgicIrqScope::Local(target), VIntId(0), bit)
            .unwrap();

        let state_after = vm
            .physical_irq_guest_state(target, pintid)
            .unwrap()
            .expect("expected bound pIRQ state");
        assert_eq!(
            state_after,
            PhysicalIrqGuestState {
                binding_kind: PhysicalIrqBindingKind::SoftwareLr,
                guest_enable: true,
                distributor_enable: true,
            }
        );
        assert!(state_after.accepts_asserted_ingress());
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn physical_irq_guest_state_for_spi_is_false_until_guest_enable() {
        let vm = recording_vm(1);
        let source_vcpu = VcpuId(0);
        let pintid = PIntId(48);
        let vintid = VIntId(40);
        let bit = 1u32 << (vintid.0 - 32);

        vm.set_dist_enable(true, true).unwrap();
        vm.bind_spi_pirq_passthrough(pintid, vintid).unwrap();

        let state_before = vm
            .physical_irq_guest_state(source_vcpu, pintid)
            .unwrap()
            .expect("expected bound pIRQ state");
        assert_eq!(
            state_before,
            PhysicalIrqGuestState {
                binding_kind: PhysicalIrqBindingKind::HardwareLr,
                guest_enable: false,
                distributor_enable: true,
            }
        );
        assert!(!state_before.accepts_asserted_ingress());

        vm.write_set_enable_word(VgicIrqScope::Global, VIntId(32), bit)
            .unwrap();

        let state_after = vm
            .physical_irq_guest_state(source_vcpu, pintid)
            .unwrap()
            .expect("expected bound pIRQ state");
        assert_eq!(
            state_after,
            PhysicalIrqGuestState {
                binding_kind: PhysicalIrqBindingKind::HardwareLr,
                guest_enable: true,
                distributor_enable: true,
            }
        );
        assert!(state_after.accepts_asserted_ingress());
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn physical_irq_guest_state_is_blocked_when_distributor_disabled() {
        let vm = recording_vm(1);
        let target = VcpuId(0);
        let pintid = PIntId(27);
        let vintid = VIntId(27);
        let bit = 1u32 << vintid.0;

        vm.bind_local_pirq_write_through_software_lr(pintid, target, vintid)
            .unwrap();
        vm.write_set_enable_word(VgicIrqScope::Local(target), VIntId(0), bit)
            .unwrap();

        let state = vm
            .physical_irq_guest_state(target, pintid)
            .unwrap()
            .expect("expected bound pIRQ state");
        assert_eq!(
            state,
            PhysicalIrqGuestState {
                binding_kind: PhysicalIrqBindingKind::SoftwareLr,
                guest_enable: true,
                distributor_enable: false,
            }
        );
        assert!(!state.accepts_asserted_ingress());
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn guest_pending_passthrough_ppi_is_not_overwritten_by_physical_reentry() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);
        let bit = 1u32 << vintid.0;

        reset_mirror_state();
        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();
        vm.write_set_enable_word(VgicIrqScope::Local(target), VIntId(0), bit)
            .unwrap();

        vm.write_set_pending_word(VgicIrqScope::Local(target), VIntId(0), bit)
            .unwrap();
        {
            let log = vm.vcpu(target).unwrap().enqueued.borrow();
            assert_eq!(log.count, 1);
            match log.last.as_ref().expect("expected pending virq") {
                VirtualInterrupt::Hardware { pintid, state, .. } => {
                    assert_eq!(*pintid, 27);
                    assert_eq!(*state, IrqState::Pending);
                }
                _ => panic!("expected hardware virq"),
            }
        }

        vm.on_physical_irq(target, PIntId(27), true).unwrap();
        let log = vm.vcpu(target).unwrap().enqueued.borrow();
        assert_eq!(log.count, 1);
        match log.last.as_ref().expect("expected pending virq") {
            VirtualInterrupt::Hardware { pintid, state, .. } => {
                assert_eq!(*pintid, 27);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected hardware virq"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn stale_pending_passthrough_ppi_requeues_after_previous_delivery_drains() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);
        let bit = 1u32 << vintid.0;

        reset_mirror_state();
        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();
        vm.write_set_enable_word(VgicIrqScope::Local(target), VIntId(0), bit)
            .unwrap();

        vm.on_physical_irq(target, PIntId(27), true).unwrap();
        assert_eq!(vm.vcpu(target).unwrap().enqueued.borrow().count, 1);
        assert_ne!(
            vm.read_pending_word(VgicIrqScope::Local(target), VIntId(0))
                .unwrap()
                & bit,
            0
        );

        vm.vcpu(target).unwrap().cancel_irq(vintid, None).unwrap();
        assert_eq!(vm.vcpu(target).unwrap().cancelled.borrow().count, 1);
        assert_ne!(
            vm.read_pending_word(VgicIrqScope::Local(target), VIntId(0))
                .unwrap()
                & bit,
            0
        );

        vm.on_physical_irq(target, PIntId(27), true).unwrap();
        let log = vm.vcpu(target).unwrap().enqueued.borrow();
        assert_eq!(log.count, 2);
        match log.last.as_ref().expect("expected requeued virq") {
            VirtualInterrupt::Hardware { pintid, state, .. } => {
                assert_eq!(*pintid, 27);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected hardware virq"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn resample_without_hook_requeues_local_passthrough_ppi_via_vm_ingress() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);
        let bit = 1u32 << vintid.0;

        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();
        vm.write_set_enable_word(VgicIrqScope::Local(target), VIntId(0), bit)
            .unwrap();

        vm.on_physical_irq(target, PIntId(27), true).unwrap();
        vm.vcpu(target).unwrap().cancel_irq(vintid, None).unwrap();

        reset_mirror_state();
        let mut notifications = crate::PirqNotifications::new();
        notifications.resample.push(PIntId(27)).unwrap();
        let update = vm
            .dispatch_pirq_notifications(target, &notifications)
            .unwrap();

        assert_eq!(recorded_mirror_count(), 0);
        assert!(matches!(
            update,
            VgicUpdate::Some { work, .. } if work.refill
        ));

        let log = vm.vcpu(target).unwrap().enqueued.borrow();
        assert_eq!(log.count, 2);
        match log.last.as_ref().expect("expected requeued virq") {
            VirtualInterrupt::Hardware { pintid, state, .. } => {
                assert_eq!(*pintid, 27);
                assert_eq!(*state, IrqState::Pending);
            }
            _ => panic!("expected hardware virq"),
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn passthrough_local_ppi_targets_single_vcpu() {
        let vm = recording_vm(2);
        let target_vcpu = VcpuId(1);
        let other_vcpu = VcpuId(0);
        let pintid = PIntId(27);
        let vintid = VIntId(27);

        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_passthrough(pintid, target_vcpu, vintid)
            .unwrap();
        vm.set_enable(VgicIrqScope::Local(target_vcpu), vintid, true)
            .unwrap();

        {
            let routing = vm.common.routing_lock.lock_irqsave();
            let entry = routing
                .pirqs
                .get_local(target_vcpu, pintid)
                .unwrap()
                .expect("expected pIRQ map");
            assert_eq!(
                routing
                    .pirqs
                    .lookup_local_by_vintid(target_vcpu, vintid)
                    .unwrap(),
                Some(pintid)
            );
            assert!(matches!(
                entry.delivery,
                PirqDelivery::Local {
                    target,
                    dist_mode: DistMirrorMode::WriteThrough,
                    cpu_mode: CpuDeliveryMode::HardwareLr,
                } if target == target_vcpu
            ));
        }

        vm.on_physical_irq(target_vcpu, pintid, true).unwrap();
        assert_eq!(vm.vcpu(target_vcpu).unwrap().enqueued.borrow().count, 1);
        assert_eq!(vm.vcpu(other_vcpu).unwrap().enqueued.borrow().count, 0);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn bind_local_pirq_same_vintid_across_vcpus_routes_by_source_vcpu() {
        let vm = recording_vm(2);
        let pintid = PIntId(27);
        let vintid = VIntId(27);
        let vcpu0 = VcpuId(0);
        let vcpu1 = VcpuId(1);

        vm.set_dist_enable(true, true).unwrap();
        vm.bind_local_pirq_passthrough(pintid, vcpu0, vintid)
            .unwrap();
        vm.bind_local_pirq_passthrough(pintid, vcpu1, vintid)
            .unwrap();
        vm.set_enable(VgicIrqScope::Local(vcpu0), vintid, true)
            .unwrap();
        vm.set_enable(VgicIrqScope::Local(vcpu1), vintid, true)
            .unwrap();

        vm.on_physical_irq(vcpu0, pintid, true).unwrap();
        vm.on_physical_irq(vcpu1, pintid, true).unwrap();

        assert_eq!(vm.vcpu(vcpu0).unwrap().enqueued.borrow().count, 1);
        assert_eq!(vm.vcpu(vcpu1).unwrap().enqueued.borrow().count, 1);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn bind_spi_pirq_passthrough_does_not_touch_spi_route() {
        let vm = recording_vm(2);
        let vintid = VIntId(40);
        vm.set_spi_route(vintid, VSpiRouting::Targets(VcpuMask::from_bits(0b10)))
            .unwrap();
        vm.bind_spi_pirq_passthrough(PIntId(48), vintid).unwrap();
        assert_eq!(
            vm.get_spi_route(vintid).unwrap(),
            VSpiRouting::Targets(VcpuMask::from_bits(0b10))
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn map_pirq_applies_group_and_priority_without_storing() {
        let vm = recording_vm(1);
        vm.map_pirq(
            PIntId(50),
            VcpuId(0),
            VIntId(40),
            IrqSense::Level,
            IrqGroup::Group1,
            0x5a,
        )
        .unwrap();

        let group_word = vm
            .read_group_word(VgicIrqScope::Global, VIntId(32))
            .unwrap();
        assert_eq!(group_word & (1u32 << (40 - 32)), 1u32 << (40 - 32));

        let prio_word = vm
            .read_priority_word(VgicIrqScope::Global, VIntId(40))
            .unwrap();
        assert_eq!(prio_word & 0xff, 0x5a);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn bind_local_pirq_passthrough_rejects_invalid_target_vcpu() {
        let vm = recording_vm(1);
        let res = vm.bind_local_pirq_passthrough(PIntId(48), VcpuId(2), VIntId(27));
        assert!(matches!(res, Err(GicError::InvalidVcpuId)));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn map_pirq_rejects_invalid_ids() {
        let vm = recording_vm(1);
        let max_intids = crate::max_intids_for_vcpus(TEST_VCPUS) as u32;
        let bad_pintid = PIntId(max_intids);
        let res = vm.map_pirq(
            bad_pintid,
            VcpuId(0),
            VIntId(40),
            IrqSense::Level,
            IrqGroup::Group1,
            0x20,
        );
        assert!(matches!(res, Err(GicError::UnsupportedIntId)));

        let bad_vintid = VIntId(max_intids);
        let res = vm.map_pirq(
            PIntId(48),
            VcpuId(0),
            bad_vintid,
            IrqSense::Level,
            IrqGroup::Group1,
            0x20,
        );
        assert!(matches!(res, Err(GicError::UnsupportedIntId)));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn unmapped_local_ppi_enable_updates_shadow_only() {
        let vm = recording_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);

        vm.set_enable(VgicIrqScope::Local(target), vintid, true)
            .unwrap();
        assert_ne!(
            vm.read_enable_word(VgicIrqScope::Local(target), VIntId(0))
                .unwrap()
                & (1u32 << vintid.0),
            0
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn unmapped_spi_enable_updates_shadow_only() {
        let vm = recording_vm(1);
        let vintid = VIntId(40);
        let bit = 1u32 << (vintid.0 - 32);

        vm.write_set_enable_word(VgicIrqScope::Global, VIntId(32), bit)
            .unwrap();
        assert_ne!(
            vm.read_enable_word(VgicIrqScope::Global, VIntId(32))
                .unwrap()
                & bit,
            0
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn sgi_sources_isolated() {
        let vm = recording_vm(1);
        let common_size = core::mem::size_of::<VmCommon<TEST_VCPUS, RecordingVcpu>>();
        let v2_size = core::mem::size_of::<V2SgiState<TEST_VCPUS>>();
        let vm_size = core::mem::size_of_val(&vm);
        assert!(vm_size >= common_size + v2_size);
    }

    const MAX_RECORDED_MIRROR_OPS: usize = 32;

    struct MirrorLog {
        count: usize,
        ops: [Option<GicMirrorOp>; MAX_RECORDED_MIRROR_OPS],
    }

    impl MirrorLog {
        const fn new() -> Self {
            Self {
                count: 0,
                ops: [None; MAX_RECORDED_MIRROR_OPS],
            }
        }

        fn clear(&mut self) {
            self.count = 0;
            self.ops = [None; MAX_RECORDED_MIRROR_OPS];
        }
    }

    struct RecordingMirror;
    struct MirrorRecordingDelegate;
    struct SignatureOnlyHw;

    static RECORDING_MIRROR: RecordingMirror = RecordingMirror;
    static MIRROR_DELEGATE: MirrorRecordingDelegate = MirrorRecordingDelegate;
    static MIRROR_MANAGER: manager::VgicManager<TEST_VCPUS> =
        manager::VgicManager::new(&MIRROR_DELEGATE, 11);
    static MIRROR_LOG: RawSpinLockIrqSave<MirrorLog> = RawSpinLockIrqSave::new(MirrorLog::new());
    static MIRROR_REJECT_GROUP0: AtomicBool = AtomicBool::new(false);

    fn reset_mirror_state() {
        MIRROR_REJECT_GROUP0.store(false, Ordering::SeqCst);
        let mut log = MIRROR_LOG.lock_irqsave();
        log.clear();
    }

    fn recorded_mirror_count() -> usize {
        let log = MIRROR_LOG.lock_irqsave();
        log.count
    }

    fn recorded_mirror_op(index: usize) -> GicMirrorOp {
        let log = MIRROR_LOG.lock_irqsave();
        log.ops[index].expect("missing recorded mirror op")
    }

    fn mirrored_vm(vcpu_count: u16) -> RecordingVm {
        let vm = recording_vm(vcpu_count);
        vm.set_pirq_manager_ctx(&MIRROR_MANAGER as *const _ as *mut ());
        vm
    }

    impl GicIrqMirror for RecordingMirror {
        fn max_intid(&self) -> u32 {
            crate::max_intids_for_vcpus(TEST_VCPUS) as u32
        }

        fn apply_mirror_op(&self, op: GicMirrorOp) -> Result<(), GicError> {
            if MIRROR_REJECT_GROUP0.load(Ordering::SeqCst)
                && matches!(
                    op,
                    GicMirrorOp::SetGroup {
                        group: IrqGroup::Group0,
                        ..
                    }
                )
            {
                return Err(GicError::UnsupportedFeature);
            }

            let mut log = MIRROR_LOG.lock_irqsave();
            if log.count >= log.ops.len() {
                return Err(GicError::OutOfResources);
            }
            let index = log.count;
            log.ops[index] = Some(op);
            log.count = index + 1;
            Ok(())
        }
    }

    impl manager::VgicDelegate for MirrorRecordingDelegate {
        fn irq_mirror(&self) -> Result<&'static dyn GicIrqMirror, GicError> {
            Ok(&RECORDING_MIRROR)
        }

        fn get_resident_affinity(
            &self,
            _vm_id: usize,
            _vcpu_id: u16,
        ) -> Result<Option<cpu::CoreAffinity>, GicError> {
            Ok(Some(cpu::get_current_core_id()))
        }

        fn get_home_affinity(
            &self,
            _vm_id: usize,
            _vcpu_id: u16,
        ) -> Result<cpu::CoreAffinity, GicError> {
            Ok(cpu::get_current_core_id())
        }

        fn get_current_vcpu(&self, _vm_id: usize) -> Result<VcpuId, GicError> {
            Ok(VcpuId(0))
        }

        fn kick_pcpu(&self, _target: cpu::CoreAffinity) -> Result<(), GicError> {
            Ok(())
        }
    }

    impl VgicHw for SignatureOnlyHw {
        type SavedState = ();

        fn hw_init(&self) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn set_enabled(&self, _enabled: bool) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn set_underflow_irq(&self, _enable: bool) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn current_eoi_mode(&self) -> Result<EoiMode, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn num_lrs(&self) -> Result<usize, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn empty_lr_bitmap(&self) -> Result<u64, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn eoi_lr_bitmap(&self) -> Result<u64, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn clear_eoi_lr_bitmap(&self, _bitmap: u64) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn take_eoi_count(&self) -> Result<u32, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn read_lr(&self, _index: usize) -> Result<VirtualInterrupt, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn write_lr(&self, _index: usize, _irq: VirtualInterrupt) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn read_apr(&self, _index: usize) -> Result<u32, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn write_apr(&self, _index: usize, _value: u32) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn maintenance_reasons(&self) -> Result<MaintenanceReasons, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn save_state(&self) -> Result<Self::SavedState, GicError> {
            panic!("signature-only hardware should not be executed")
        }

        fn restore_state(&self, _state: &Self::SavedState) -> Result<(), GicError> {
            panic!("signature-only hardware should not be executed")
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn pirq_reverse_lookup_updates_on_bind_and_unmap() {
        let vm = recording_vm(1);
        let pintid = PIntId(48);
        let vintid = VIntId(40);
        vm.bind_spi_pirq_passthrough(pintid, vintid).unwrap();
        {
            let routing = vm.common.routing_lock.lock_irqsave();
            assert_eq!(
                routing.pirqs.lookup_by_vintid(vintid).unwrap(),
                Some(pintid)
            );
        }
        vm.unmap_pirq(pintid).unwrap();
        {
            let routing = vm.common.routing_lock.lock_irqsave();
            assert_eq!(routing.pirqs.lookup_by_vintid(vintid).unwrap(), None);
        }
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn pirq_duplicate_vintid_rejected_for_bindings() {
        let vm = recording_vm(1);
        let vintid = VIntId(40);
        vm.bind_spi_pirq_passthrough(PIntId(48), vintid).unwrap();
        let res = vm.bind_spi_pirq_passthrough(PIntId(49), vintid);
        assert!(matches!(res, Err(GicError::InvalidState)));
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn no_first_enable_resolution_remains_for_local_ppi() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);
        let group_bit = 1u32 << vintid.0;

        reset_mirror_state();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();

        vm.write_group_word(VgicIrqScope::Local(target), VIntId(0), group_bit)
            .unwrap();
        vm.write_priority_word(VgicIrqScope::Local(target), VIntId(24), 0x68u32 << 24)
            .unwrap();
        vm.write_trigger_word(
            VgicIrqScope::Local(target),
            VIntId(16),
            0b10u32 << ((vintid.0 - 16) * 2),
        )
        .unwrap();

        assert_eq!(recorded_mirror_count(), 3);
        assert_eq!(
            recorded_mirror_op(0),
            GicMirrorOp::SetGroup {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                group: IrqGroup::Group1,
            }
        );
        assert_eq!(
            recorded_mirror_op(1),
            GicMirrorOp::SetPriority {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                priority: 0x68,
            }
        );
        assert_eq!(
            recorded_mirror_op(2),
            GicMirrorOp::SetTrigger {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                trigger: TriggerMode::Edge,
            }
        );

        vm.write_set_enable_word(VgicIrqScope::Local(target), VIntId(0), group_bit)
            .unwrap();
        assert_eq!(recorded_mirror_count(), 4);
        assert_eq!(
            recorded_mirror_op(3),
            GicMirrorOp::SetEnable {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                enable: true,
            }
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn local_ppi_group_write_through_emits_mirror_op() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);

        reset_mirror_state();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();
        vm.write_group_word(VgicIrqScope::Local(target), VIntId(0), 1u32 << vintid.0)
            .unwrap();

        assert_eq!(recorded_mirror_count(), 1);
        assert_eq!(
            recorded_mirror_op(0),
            GicMirrorOp::SetGroup {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                group: IrqGroup::Group1,
            }
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn local_ppi_priority_write_through_emits_mirror_op() {
        let vm = mirrored_vm(1);
        let target = VcpuId(0);
        let vintid = VIntId(27);

        reset_mirror_state();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();
        vm.write_priority_word(VgicIrqScope::Local(target), VIntId(24), 0x60u32 << 24)
            .unwrap();

        assert_eq!(recorded_mirror_count(), 1);
        assert_eq!(
            recorded_mirror_op(0),
            GicMirrorOp::SetPriority {
                scope: GicMirrorScope::Local(target),
                intid: 27,
                priority: 0x60,
            }
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn local_ppi_write_through_rejects_non_current_target_vcpu() {
        let vm = mirrored_vm(2);
        let target = VcpuId(1);
        let vintid = VIntId(27);

        reset_mirror_state();
        vm.bind_local_pirq_passthrough(PIntId(27), target, vintid)
            .unwrap();

        let res = vm.write_group_word(VgicIrqScope::Local(target), VIntId(0), 1u32 << vintid.0);
        assert!(matches!(res, Err(GicError::InvalidState)));
        assert_eq!(recorded_mirror_count(), 0);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn manager_exposes_asserted_physical_irq_wrapper() {
        fn assert_signature(
            _: fn(
                &manager::VgicManager<TEST_VCPUS>,
                &SignatureOnlyHw,
                PIntId,
            ) -> Result<(), GicError>,
        ) {
        }

        assert_signature(
            manager::VgicManager::<TEST_VCPUS>::handle_physical_irq_asserted::<SignatureOnlyHw>,
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn manager_exposes_spi_software_lr_binding_wrapper() {
        fn assert_signature(
            _: fn(
                &manager::VgicManager<TEST_VCPUS>,
                &SignatureOnlyHw,
                PIntId,
                VIntId,
            ) -> Result<(), GicError>,
        ) {
        }

        assert_signature(
            manager::VgicManager::<TEST_VCPUS>::bind_spi_pirq_write_through_software_lr::<
                SignatureOnlyHw,
            >,
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn manager_exposes_physical_irq_binding_kind_wrapper() {
        fn assert_signature(
            _: fn(
                &manager::VgicManager<TEST_VCPUS>,
                PIntId,
            ) -> Result<Option<PhysicalIrqBindingKind>, GicError>,
        ) {
        }

        assert_signature(manager::VgicManager::<TEST_VCPUS>::physical_irq_binding_kind);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn manager_exposes_physical_irq_guest_state_wrapper() {
        fn assert_signature(
            _: fn(
                &manager::VgicManager<TEST_VCPUS>,
                PIntId,
            ) -> Result<Option<PhysicalIrqGuestState>, GicError>,
        ) {
        }

        assert_signature(manager::VgicManager::<TEST_VCPUS>::physical_irq_guest_state);
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn exact_gicv2_itargetsr_byte_is_mirrored() {
        let vm = mirrored_vm(4);
        let vintid = VIntId(40);

        reset_mirror_state();
        vm.bind_spi_pirq_passthrough(PIntId(48), vintid).unwrap();
        vm.set_spi_route(
            vintid,
            VSpiRouting::Targets(VcpuMask::from_bits(0b0000_1011)),
        )
        .unwrap();

        assert_eq!(recorded_mirror_count(), 1);
        assert_eq!(
            recorded_mirror_op(0),
            GicMirrorOp::SetRoute {
                intid: 48,
                route: GicMirrorRoute::Gicv2TargetMask(0b0000_1011),
            }
        );
    }

    #[cfg_attr(all(test, target_arch = "aarch64"), test_case)]
    fn security_policy_error_surfaces_on_group_write() {
        let vm = mirrored_vm(1);
        let vintid = VIntId(40);
        let bit = 1u32 << (vintid.0 - 32);

        reset_mirror_state();
        vm.bind_spi_pirq_passthrough(PIntId(48), vintid).unwrap();
        vm.write_group_word(VgicIrqScope::Global, VIntId(32), bit)
            .unwrap();

        MIRROR_REJECT_GROUP0.store(true, Ordering::SeqCst);
        let res = vm.write_group_word(VgicIrqScope::Global, VIntId(32), 0);
        assert!(matches!(res, Err(GicError::UnsupportedFeature)));
    }
}
