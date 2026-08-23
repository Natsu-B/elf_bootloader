#![cfg_attr(target_arch = "aarch64", no_std)]
#![cfg_attr(target_arch = "aarch64", no_main)]
#![feature(generic_const_exprs)]
#![allow(incomplete_features)]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

// Exercises the rpi_boot-like asserted-PPI path with HW-LR drop-only EOIs.
include!("support/vgic_e2e_physical_ppi.rs");

const TEST_NAME: &str = "vgic_e2e_physical_ppi_rpi_boot_hwlr_drop_only";
const RUN_TEST: RunTest = common::run_el2_rpi_boot_like_hwlr_drop_only_no_deactivate;
const FAILURE_CODES: [u32; 4] = [
    common::FAIL_RPI_BOOT_PPI_WAIT_TIMEOUT,
    common::FAIL_RPI_BOOT_PPI_SECOND_WAIT_TIMEOUT,
    common::FAIL_RPI_BOOT_PPI_DELIVERED_AFTER_DISABLE,
    common::FAIL_RPI_BOOT_PPI_UNEXPECTED_INTID,
];
