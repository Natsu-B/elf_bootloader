use super::GicVmModelForVcpus;
use super::common::LOCAL_INTID_COUNT;
use super::pending_cap_for_vcpus;
use crate::GicError;
use crate::GicIrqMirror;
use crate::GicMirrorOp;
use crate::IrqGroup;
use crate::IrqSense;
use crate::PIntId;
use crate::PhysicalIrqBindingKind;
use crate::PhysicalIrqGuestState;
use crate::VIntId;
use crate::VcpuId;
use crate::VgicGuestRegs;
use crate::VgicHw;
use crate::VgicIrqScope;
use crate::VgicPirqModel;
use crate::VgicTargets;
use crate::VgicUpdate;
use crate::VgicVcpuModel;
use crate::VgicVmInfo;
use crate::gicv2::Gicv2;
use crate::gicv2::vgic_frontend::Gicv2AccessSize;
use crate::gicv2::vgic_frontend::Gicv2DistIdRegs;
use crate::gicv2::vgic_frontend::Gicv2Frontend;
use common::PirqLifecycleHook;
use core::cell::SyncUnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::Ordering;
use cpu::CoreAffinity;
use mutex::pod::RawAtomicPod;
use tls::cpu_if;

/// Platform policy callbacks used by [`VgicManager`].
///
/// Callbacks can run concurrently with VM model operations and are never invoked while a global
/// manager lock is held.
pub trait VgicDelegate: Send + Sync {
    fn irq_mirror(&self) -> Result<&'static dyn GicIrqMirror, GicError>;
    fn get_resident_affinity(
        &self,
        vm_id: usize,
        vcpu_id: u16,
    ) -> Result<Option<CoreAffinity>, GicError>;
    fn get_home_affinity(&self, vm_id: usize, vcpu_id: u16) -> Result<CoreAffinity, GicError>;
    fn get_current_vcpu(&self, vm_id: usize) -> Result<VcpuId, GicError>;
    fn kick_pcpu(&self, target: CoreAffinity) -> Result<(), GicError>;
}

pub struct VgicManager<const VCPUS: usize>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); pending_cap_for_vcpus(VCPUS)]:,
{
    delegate: &'static dyn VgicDelegate,
    vm_id: usize,
    model_ptr: RawAtomicPod<Option<NonNull<GicVmModelForVcpus<VCPUS>>>>,
    model_storage: SyncUnsafeCell<MaybeUninit<GicVmModelForVcpus<VCPUS>>>,
}

impl<const VCPUS: usize> VgicManager<VCPUS>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); pending_cap_for_vcpus(VCPUS)]:,
{
    pub const fn new(delegate: &'static dyn VgicDelegate, vm_id: usize) -> Self {
        Self {
            delegate,
            vm_id,
            model_ptr: unsafe {
                // SAFETY: `Option<NonNull<T>>` canonical raw encoding uses 0 for `None`.
                RawAtomicPod::new_raw_unchecked(0usize)
            },
            model_storage: SyncUnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Initialize this manager from a GICv2 backend.
    ///
    /// # Safety
    /// - Must be called exactly once.
    /// - Must be called from single-threaded bring-up (BSP only), before any concurrent vGIC access.
    /// - `self` must have a stable address for the whole program lifetime (typically a `'static`).
    pub unsafe fn init_from_gicv2(
        &'static self,
        gic: &Gicv2,
        vcpu_count: u8,
    ) -> Result<(), GicError>
    where
        [(); VCPUS - 1]:,
        [(); 16 - VCPUS]:,
    {
        if self.model_ptr.load(Ordering::Relaxed).is_some() {
            return Err(GicError::InvalidState);
        }

        let model = gic.create_vm_model::<VCPUS>(vcpu_count)?;
        let ptr = {
            let slot = self.model_storage.get();
            // SAFETY: Caller guarantees one-time single-threaded initialization before publish.
            // `slot` points to the in-place storage owned by this manager and is valid for writes.
            let model_ref = unsafe { (*slot).write(model) };
            model_ref.set_pirq_manager_ctx(self as *const _ as *mut ());
            NonNull::from(model_ref)
        };
        self.model_ptr.store(Some(ptr), Ordering::Release);
        Ok(())
    }

    fn model(&self) -> Result<&GicVmModelForVcpus<VCPUS>, GicError> {
        let ptr = self
            .model_ptr
            .load(Ordering::Acquire)
            .ok_or(GicError::InvalidState)?;
        // SAFETY: Pointer is published with `Release` only after in-place initialization, and
        // points into `model_storage` owned by this manager at a stable address.
        Ok(unsafe { ptr.as_ref() })
    }

    pub fn switch_in<H: VgicHw>(
        &self,
        hw: &H,
        vcpu: VcpuId,
        core: CoreAffinity,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let vcpu_model = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, vcpu)?;
        vcpu_model.set_resident(core)?;
        let _kick = vcpu_model.refill_lrs(hw)?;
        hw.set_enabled(true)?;
        Ok(())
    }

    pub fn switch_out_sync<H: VgicHw>(&self, hw: &H, vcpu: VcpuId) -> Result<(), GicError> {
        let vm = self.model()?;
        let vcpu_model = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, vcpu)?;
        vcpu_model.switch_out_sync(hw)
    }

    pub fn handle_physical_irq<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        level: bool,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let source_vcpu = VcpuId(u16::from(cpu_if().ok_or(GicError::UninitCpuId)?));
        let vcpu_model = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, source_vcpu)?;
        vcpu_model.sync_lr_shadow(hw)?;
        let update = vm.on_physical_irq(source_vcpu, pintid, level)?;
        self.apply_update(hw, update)
    }

    /// Handle asserted physical IRQ ingress observed from the EL2 IRQ exception path.
    ///
    /// Callers should use this after `acknowledge()` and still perform `end_of_interrupt()` once
    /// vGIC handling completes. This wrapper is intentionally narrower than the boolean-based
    /// ingress API because the EL2 IRQ exception path only observes asserted ingress.
    pub fn handle_physical_irq_asserted<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
    ) -> Result<(), GicError> {
        self.handle_physical_irq(hw, pintid, true)
    }

    pub fn physical_irq_binding_kind(
        &self,
        pintid: PIntId,
    ) -> Result<Option<PhysicalIrqBindingKind>, GicError> {
        let vm = self.model()?;
        let source_vcpu = VcpuId(u16::from(cpu_if().ok_or(GicError::UninitCpuId)?));
        vm.physical_irq_binding_kind(source_vcpu, pintid)
    }

    pub fn physical_irq_guest_state(
        &self,
        pintid: PIntId,
    ) -> Result<Option<PhysicalIrqGuestState>, GicError> {
        let vm = self.model()?;
        let source_vcpu = VcpuId(u16::from(cpu_if().ok_or(GicError::UninitCpuId)?));
        vm.physical_irq_guest_state(source_vcpu, pintid)
    }

    pub fn inject_ppi<H: VgicHw>(
        &self,
        hw: &H,
        ppi_intid: u32,
        level: bool,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let current = self.delegate.get_current_vcpu(self.vm_id)?;
        let update = vm.set_pending(VgicIrqScope::Local(current), VIntId(ppi_intid), level)?;
        self.apply_update(hw, update)
    }

    pub fn inject_physical_irq_as_pending<H: VgicHw>(
        &self,
        hw: &H,
        intid: u32,
        level: bool,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let current = self.delegate.get_current_vcpu(self.vm_id)?;
        let scope = if intid < LOCAL_INTID_COUNT as u32 {
            VgicIrqScope::Local(current)
        } else {
            VgicIrqScope::Global
        };
        let update = vm.set_pending(scope, VIntId(intid), level)?;
        self.apply_update(hw, update)
    }

    pub fn map_pirq<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
        sense: IrqSense,
        group: IrqGroup,
        priority: u8,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let update = vm.map_pirq(pintid, target, vintid, sense, group, priority)?;
        self.apply_update(hw, update)
    }

    pub fn bind_local_pirq_passthrough<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let update = vm.bind_local_pirq_passthrough(pintid, target, vintid)?;
        self.apply_update(hw, update)
    }

    pub fn bind_local_pirq_write_through_software_lr<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        target: VcpuId,
        vintid: VIntId,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let update = vm.bind_local_pirq_write_through_software_lr(pintid, target, vintid)?;
        self.apply_update(hw, update)
    }

    pub fn bind_spi_pirq_passthrough<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let update = vm.bind_spi_pirq_passthrough(pintid, vintid)?;
        self.apply_update(hw, update)
    }

    pub fn bind_spi_pirq_write_through_software_lr<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let update = vm.bind_spi_pirq_write_through_software_lr(pintid, vintid)?;
        self.apply_update(hw, update)
    }

    pub fn bind_spi_pirq_shadow_software_lr<H: VgicHw>(
        &self,
        hw: &H,
        pintid: PIntId,
        vintid: VIntId,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        let update = vm.bind_spi_pirq_shadow_software_lr(pintid, vintid)?;
        self.apply_update(hw, update)
    }

    pub fn set_pirq_hook(
        &self,
        hook: Option<&'static dyn PirqLifecycleHook>,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        vm.set_pirq_hook(hook);
        Ok(())
    }

    pub fn handle_distributor_read(
        &self,
        vcpu: VcpuId,
        dist_id: Gicv2DistIdRegs,
        offset: u32,
        access_size: Gicv2AccessSize,
    ) -> Result<u32, GicError> {
        let vm = self.model()?;
        let frontend = Gicv2Frontend::new(vm, dist_id);
        frontend.handle_distributor_read(vcpu, offset, access_size)
    }

    pub fn handle_distributor_write<H: VgicHw>(
        &self,
        hw: &H,
        vcpu: VcpuId,
        dist_id: Gicv2DistIdRegs,
        offset: u32,
        access_size: Gicv2AccessSize,
        value: u32,
    ) -> Result<(), GicError> {
        let vm = self.model()?;
        if offset == 0xF00 {
            let vcpu_model = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, vcpu)?;
            vcpu_model.sync_lr_shadow(hw)?;
        }
        let frontend = Gicv2Frontend::new(vm, dist_id);
        let update = frontend.handle_distributor_write(vcpu, offset, access_size, value)?;
        self.apply_update(hw, update)
    }

    pub fn handle_maintenance<H: VgicHw>(&self, hw: &H, vcpu: VcpuId) -> Result<(), GicError> {
        let vm = self.model()?;
        let (update, notifs) = {
            let vcpu_model = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, vcpu)?;
            vcpu_model.handle_maintenance_collect(hw)?
        };
        let mut combined_update = update;
        combined_update.combine(&vm.dispatch_pirq_notifications(vcpu, &notifs)?);
        self.apply_update(hw, combined_update)
    }

    pub fn refill_vcpu<H: VgicHw>(&self, hw: &H, vcpu: VcpuId) -> Result<bool, GicError> {
        let vm = self.model()?;
        let vcpu_model = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, vcpu)?;
        vcpu_model.refill_lrs(hw)
    }

    pub(crate) fn apply_update<H: VgicHw>(
        &self,
        hw: &H,
        update: VgicUpdate,
    ) -> Result<(), GicError> {
        let VgicUpdate::Some { targets, work } = update else {
            return Ok(());
        };
        if !work.refill {
            return Ok(());
        }

        let vm = self.model()?;
        let current = self.delegate.get_current_vcpu(self.vm_id)?;
        let mut kick_targets: [Option<CoreAffinity>; VCPUS] = [None; VCPUS];
        let mut kick_count = 0usize;

        let mut maybe_queue_kick = |target: VcpuId| -> Result<(), GicError> {
            let vcpu = <GicVmModelForVcpus<VCPUS> as VgicVmInfo>::vcpu(vm, target)?;
            let needs_kick = vcpu.refill_lrs(hw)?;
            if target == current || (!work.kick && !needs_kick) {
                return Ok(());
            }
            let Some(affinity) = self.delegate.get_resident_affinity(self.vm_id, target.0)? else {
                return Ok(());
            };

            if kick_count >= kick_targets.len() {
                return Err(GicError::OutOfResources);
            }
            if kick_targets[..kick_count]
                .iter()
                .any(|existing| *existing == Some(affinity))
            {
                return Ok(());
            }
            kick_targets[kick_count] = Some(affinity);
            kick_count += 1;
            Ok(())
        };

        match targets {
            VgicTargets::One(id) => maybe_queue_kick(id)?,
            VgicTargets::Mask(mask) => {
                for id in mask.iter() {
                    maybe_queue_kick(id)?;
                }
            }
            VgicTargets::All => {
                for id in 0..vm.vcpu_count() {
                    maybe_queue_kick(VcpuId(id))?;
                }
            }
        }

        for affinity in kick_targets.into_iter().take(kick_count).flatten() {
            self.delegate.kick_pcpu(affinity)?;
        }
        Ok(())
    }
}

unsafe fn manager_from_ctx<const VCPUS: usize>(
    ctx: *mut (),
) -> Result<&'static VgicManager<VCPUS>, GicError>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); pending_cap_for_vcpus(VCPUS)]:,
{
    if ctx.is_null() {
        return Err(GicError::InvalidState);
    }
    // SAFETY: `ctx` is installed by `VgicManager::init_from_gicv2` from a stable `'static`
    // manager reference with the same `VCPUS` parameter.
    Ok(unsafe { &*(ctx as *const VgicManager<VCPUS>) })
}

pub(crate) unsafe fn apply_mirror_op_from_ctx<const VCPUS: usize>(
    ctx: *mut (),
    op: GicMirrorOp,
) -> Result<(), GicError>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
    [(); crate::VgicVmConfig::<VCPUS>::MAX_LRS]:,
    [(); pending_cap_for_vcpus(VCPUS)]:,
{
    // SAFETY: `ctx` was installed by `init_from_gicv2` for this manager const instantiation.
    let manager = unsafe { manager_from_ctx::<VCPUS>(ctx)? };

    let local_target = match op {
        GicMirrorOp::SetGroup {
            scope: crate::GicMirrorScope::Local(target),
            ..
        }
        | GicMirrorOp::SetPriority {
            scope: crate::GicMirrorScope::Local(target),
            ..
        }
        | GicMirrorOp::SetTrigger {
            scope: crate::GicMirrorScope::Local(target),
            ..
        }
        | GicMirrorOp::SetEnable {
            scope: crate::GicMirrorScope::Local(target),
            ..
        }
        | GicMirrorOp::SetPending {
            scope: crate::GicMirrorScope::Local(target),
            ..
        }
        | GicMirrorOp::SetActive {
            scope: crate::GicMirrorScope::Local(target),
            ..
        } => Some(target),
        GicMirrorOp::SetGroup {
            scope: crate::GicMirrorScope::Global,
            ..
        }
        | GicMirrorOp::SetPriority {
            scope: crate::GicMirrorScope::Global,
            ..
        }
        | GicMirrorOp::SetTrigger {
            scope: crate::GicMirrorScope::Global,
            ..
        }
        | GicMirrorOp::SetEnable {
            scope: crate::GicMirrorScope::Global,
            ..
        }
        | GicMirrorOp::SetPending {
            scope: crate::GicMirrorScope::Global,
            ..
        }
        | GicMirrorOp::SetActive {
            scope: crate::GicMirrorScope::Global,
            ..
        }
        | GicMirrorOp::SetRoute { .. } => None,
    };

    if let Some(target) = local_target {
        let current = manager.delegate.get_current_vcpu(manager.vm_id)?;
        if target != current {
            return Err(GicError::InvalidState);
        }
    }

    manager.delegate.irq_mirror()?.apply_mirror_op(op)
}
