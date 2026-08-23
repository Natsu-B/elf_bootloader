#![cfg_attr(target_arch = "aarch64", no_std)]
#![cfg_attr(target_arch = "aarch64", no_main)]
#![feature(generic_const_exprs)]
#![allow(incomplete_features)]

#[cfg(not(target_arch = "aarch64"))]
compile_error!("This test is intended to run on aarch64 targets only");

// Exercises direct sticky-PPI delivery with the regular vGIC EL2 path.
include!("support/vgic_e2e_physical_ppi.rs");

const TEST_NAME: &str = "vgic_e2e_physical_ppi";
const RUN_TEST: RunTest = common::run_el2;
const FAILURE_CODES: [u32; 4] = [
    common::FAIL_PHYS_PPI_WAIT_TIMEOUT,
    common::FAIL_PHYS_PPI_SECOND_WAIT_TIMEOUT,
    common::FAIL_PHYS_PPI_DELIVERED_AFTER_DISABLE,
    common::FAIL_PHYS_PPI_UNEXPECTED_INTID,
];
