//! Platform boundary for source-specific physical IRQ completion.

use arch_hal::common::PirqHookError;
use arch_hal::soc::bcm2712;

/// Direction of a completed passthrough MMIO access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PassthroughMmioAccessKind {
    /// A guest read has completed.
    Read,
    /// A guest write has completed.
    Write,
}

/// A completed guest access to a passthrough MMIO region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PassthroughMmioAccess {
    /// Identity-mapped address accessed by the guest.
    pub(crate) address: usize,
    /// Direction of the access.
    pub(crate) kind: PassthroughMmioAccessKind,
}

/// Source-specific work that must run around generic passthrough and host IRQ handling.
///
/// These are intentionally separate lifecycle points: some devices complete at a guest MMIO
/// access, while level sources may need another sample only after host EOI/deactivation.
pub(crate) trait PirqSourceService: Sync {
    /// Completes any source-specific work after a guest passthrough access.
    fn after_passthrough_access(&self, access: PassthroughMmioAccess) -> Result<(), PirqHookError>;

    /// Re-samples platform sources after the current host IRQ has been completed.
    fn after_host_irq_completion(&self, completed_int_id: u32) -> Result<(), PirqHookError>;
}

struct Bcm2712PirqSourceService;

impl PirqSourceService for Bcm2712PirqSourceService {
    fn after_passthrough_access(&self, access: PassthroughMmioAccess) -> Result<(), PirqHookError> {
        bcm2712::pirq_hook::after_passthrough_mmio_access(
            access.address,
            access.kind == PassthroughMmioAccessKind::Write,
        )
    }

    fn after_host_irq_completion(&self, completed_int_id: u32) -> Result<(), PirqHookError> {
        bcm2712::pirq_hook::resample_level_sources(Some(completed_int_id))
    }
}

static BCM2712_PIRQ_SOURCE_SERVICE: Bcm2712PirqSourceService = Bcm2712PirqSourceService;

/// Returns the board service used by the exception path.
pub(crate) fn pirq_source_service() -> &'static dyn PirqSourceService {
    &BCM2712_PIRQ_SOURCE_SERVICE
}
