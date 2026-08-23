#![cfg_attr(target_arch = "aarch64", no_std)]
#![cfg_attr(target_arch = "aarch64", no_main)]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

include!("support/gicv2_pending.rs");

const TEST_NAME: &str = "gicv2_vgic_pending";
const EOI_MODES: &[EoiMode] = &[EoiMode::DropAndDeactivate];
