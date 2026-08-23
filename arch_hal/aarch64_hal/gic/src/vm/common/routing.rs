use crate::GicError;
use crate::VIntId;
use crate::VSpiRouting;
use crate::VcpuMask;
use crate::vm::common::irq_state::LOCAL_INTID_COUNT;

pub(crate) struct SpiRouting<const VCPUS: usize>
where
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    spi_route: [VSpiRouting; crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT],
}

impl<const VCPUS: usize> SpiRouting<VCPUS>
where
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    pub(crate) fn new() -> Self {
        Self {
            spi_route: [VSpiRouting::Targets(VcpuMask::from_bits(1)); {
                crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT
            }],
        }
    }

    fn index_for(&self, vintid: VIntId) -> Result<usize, GicError> {
        let intid = vintid.0 as usize;
        let max_intids = crate::max_intids_for_vcpus(VCPUS);
        if intid < LOCAL_INTID_COUNT || intid >= max_intids {
            return Err(GicError::UnsupportedIntId);
        }
        Ok(intid - LOCAL_INTID_COUNT)
    }

    pub(crate) fn set_route(
        &mut self,
        vintid: VIntId,
        targets: VSpiRouting,
    ) -> Result<bool, GicError> {
        let idx = self.index_for(vintid)?;
        match targets {
            VSpiRouting::Targets(mask) => {
                let entry = VSpiRouting::Targets(mask);
                Ok(core::mem::replace(&mut self.spi_route[idx], entry) != entry)
            }
            VSpiRouting::Specific(_) | VSpiRouting::AnyParticipating => {
                Err(GicError::UnsupportedFeature)
            }
        }
    }

    pub(crate) fn get_route(&self, vintid: VIntId) -> Result<VSpiRouting, GicError> {
        let idx = self.index_for(vintid)?;
        Ok(self.spi_route[idx])
    }

    pub(crate) fn targets_for_spi(
        &self,
        vintid: VIntId,
        vcpu_count: usize,
    ) -> Result<VcpuMask, GicError> {
        let idx = self.index_for(vintid)?;
        match self.spi_route[idx] {
            VSpiRouting::Targets(mask) => {
                let mut out = VcpuMask::EMPTY;
                for id in mask.iter() {
                    if (id.0 as usize) < vcpu_count {
                        let _ = out.set(id);
                    }
                }
                Ok(out)
            }
            VSpiRouting::Specific(_) | VSpiRouting::AnyParticipating => {
                Err(GicError::UnsupportedFeature)
            }
        }
    }
}
