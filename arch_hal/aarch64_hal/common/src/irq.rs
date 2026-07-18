/// Trigger configuration for an interrupt (where configurable).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TriggerMode {
    Level,
    Edge,
}

/// Edge/level semantics for injection bookkeeping.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum IrqSense {
    Edge,
    Level,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PirqHookOp {
    Configure {
        group: u8,
        priority: u8,
        trigger: TriggerMode,
        targets: u32,
        enable: bool,
    },
    Eoi,
    Deactivate,
    Resample,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum PirqHookError {
    InvalidState,
    Unsupported,
    InvalidInput,
}

/// Platform callback for physical interrupt lifecycle events emitted by a virtual GIC.
///
/// Implementations must be safe to call concurrently and must not depend on a vGIC lock being
/// held. The vGIC invokes this boundary only after releasing its internal locks.
pub trait PirqLifecycleHook: Send + Sync {
    /// Handles one lifecycle event for `int_id`.
    fn on_pirq_event(&self, int_id: u32, op: PirqHookOp) -> Result<(), PirqHookError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU8;
    use core::sync::atomic::AtomicU32;
    use core::sync::atomic::Ordering;

    struct RecordingHook {
        int_id: AtomicU32,
        event: AtomicU8,
    }

    impl PirqLifecycleHook for RecordingHook {
        fn on_pirq_event(&self, int_id: u32, op: PirqHookOp) -> Result<(), PirqHookError> {
            let event = match op {
                PirqHookOp::Configure { .. } => 1,
                PirqHookOp::Eoi => 2,
                PirqHookOp::Deactivate => 3,
                PirqHookOp::Resample => 4,
            };
            self.int_id.store(int_id, Ordering::Relaxed);
            self.event.store(event, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn lifecycle_hook_dispatches_through_trait_object() {
        let recording = RecordingHook {
            int_id: AtomicU32::new(0),
            event: AtomicU8::new(0),
        };
        let hook: &dyn PirqLifecycleHook = &recording;

        hook.on_pirq_event(185, PirqHookOp::Eoi).unwrap();

        assert_eq!(recording.int_id.load(Ordering::Relaxed), 185);
        assert_eq!(recording.event.load(Ordering::Relaxed), 2);
    }
}
