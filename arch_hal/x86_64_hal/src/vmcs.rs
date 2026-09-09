//! VMCS field encodings and the small control-bit set used by the smoke monitor.
//!
//! Encodings follow Intel SDM Volume 3, Appendix B, "Field Encoding in VMCS".
//! Each declaration is checked against both the Appendix B hexadecimal value
//! and the encoding layout described in SDM section "VMCS Component Encoding".

/// Encoding value for a 16-bit VMCS component.
const WIDTH_16: u32 = 0;
/// Encoding value for a 64-bit VMCS component.
const WIDTH_64: u32 = 1;
/// Encoding value for a 32-bit VMCS component.
const WIDTH_32: u32 = 2;
/// Encoding value for a natural-width VMCS component.
const WIDTH_NATURAL: u32 = 3;

/// Encoding value for a control component.
const CONTROL: u32 = 0;
/// Encoding value for a read-only VM-exit-information component.
const EXIT_INFORMATION: u32 = 1;
/// Encoding value for a guest-state component.
const GUEST_STATE: u32 = 2;
/// Encoding value for a host-state component.
const HOST_STATE: u32 = 3;

/// Builds a full-access VMCS component encoding.
const fn encode_field(width: u32, class: u32, index: u32) -> u32 {
    assert!(width <= 3 && class <= 3 && index <= 0x1ff);
    (width << 13) | (class << 10) | (index << 1)
}

/// Declares fields while checking their published hexadecimal encodings.
macro_rules! vmcs_fields {
    ($($name:ident = $expected:literal => ($width:ident, $class:ident, $index:literal);)+) => {
        $(
            #[doc = concat!("Intel VMCS field encoding for `", stringify!($name), "`.")]
            pub const $name: u32 = encode_field($width, $class, $index);
            const _: () = assert!($name == $expected);
        )+
    };
}

vmcs_fields! {
    VIRTUAL_PROCESSOR_ID = 0x0000 => (WIDTH_16, CONTROL, 0);
    GUEST_ES_SELECTOR = 0x0800 => (WIDTH_16, GUEST_STATE, 0);
    GUEST_CS_SELECTOR = 0x0802 => (WIDTH_16, GUEST_STATE, 1);
    GUEST_SS_SELECTOR = 0x0804 => (WIDTH_16, GUEST_STATE, 2);
    GUEST_DS_SELECTOR = 0x0806 => (WIDTH_16, GUEST_STATE, 3);
    GUEST_FS_SELECTOR = 0x0808 => (WIDTH_16, GUEST_STATE, 4);
    GUEST_GS_SELECTOR = 0x080a => (WIDTH_16, GUEST_STATE, 5);
    GUEST_LDTR_SELECTOR = 0x080c => (WIDTH_16, GUEST_STATE, 6);
    GUEST_TR_SELECTOR = 0x080e => (WIDTH_16, GUEST_STATE, 7);

    HOST_ES_SELECTOR = 0x0c00 => (WIDTH_16, HOST_STATE, 0);
    HOST_CS_SELECTOR = 0x0c02 => (WIDTH_16, HOST_STATE, 1);
    HOST_SS_SELECTOR = 0x0c04 => (WIDTH_16, HOST_STATE, 2);
    HOST_DS_SELECTOR = 0x0c06 => (WIDTH_16, HOST_STATE, 3);
    HOST_FS_SELECTOR = 0x0c08 => (WIDTH_16, HOST_STATE, 4);
    HOST_GS_SELECTOR = 0x0c0a => (WIDTH_16, HOST_STATE, 5);
    HOST_TR_SELECTOR = 0x0c0c => (WIDTH_16, HOST_STATE, 6);

    MSR_BITMAP = 0x2004 => (WIDTH_64, CONTROL, 2);
    VM_EXIT_MSR_STORE_ADDR = 0x2006 => (WIDTH_64, CONTROL, 3);
    VM_EXIT_MSR_LOAD_ADDR = 0x2008 => (WIDTH_64, CONTROL, 4);
    VM_ENTRY_MSR_LOAD_ADDR = 0x200a => (WIDTH_64, CONTROL, 5);
    EPT_POINTER = 0x201a => (WIDTH_64, CONTROL, 13);
    GUEST_PHYSICAL_ADDRESS = 0x2400 => (WIDTH_64, EXIT_INFORMATION, 0);
    VMCS_LINK_POINTER = 0x2800 => (WIDTH_64, GUEST_STATE, 0);
    GUEST_IA32_DEBUGCTL = 0x2802 => (WIDTH_64, GUEST_STATE, 1);
    GUEST_IA32_PAT = 0x2804 => (WIDTH_64, GUEST_STATE, 2);
    GUEST_IA32_EFER = 0x2806 => (WIDTH_64, GUEST_STATE, 3);
    HOST_IA32_PAT = 0x2c00 => (WIDTH_64, HOST_STATE, 0);
    HOST_IA32_EFER = 0x2c02 => (WIDTH_64, HOST_STATE, 1);

    PIN_BASED_VM_EXEC_CONTROL = 0x4000 => (WIDTH_32, CONTROL, 0);
    CPU_BASED_VM_EXEC_CONTROL = 0x4002 => (WIDTH_32, CONTROL, 1);
    EXCEPTION_BITMAP = 0x4004 => (WIDTH_32, CONTROL, 2);
    PAGE_FAULT_ERROR_CODE_MASK = 0x4006 => (WIDTH_32, CONTROL, 3);
    PAGE_FAULT_ERROR_CODE_MATCH = 0x4008 => (WIDTH_32, CONTROL, 4);
    CR3_TARGET_COUNT = 0x400a => (WIDTH_32, CONTROL, 5);
    VM_EXIT_CONTROLS = 0x400c => (WIDTH_32, CONTROL, 6);
    VM_EXIT_MSR_STORE_COUNT = 0x400e => (WIDTH_32, CONTROL, 7);
    VM_EXIT_MSR_LOAD_COUNT = 0x4010 => (WIDTH_32, CONTROL, 8);
    VM_ENTRY_CONTROLS = 0x4012 => (WIDTH_32, CONTROL, 9);
    VM_ENTRY_MSR_LOAD_COUNT = 0x4014 => (WIDTH_32, CONTROL, 10);
    VM_ENTRY_INTR_INFO_FIELD = 0x4016 => (WIDTH_32, CONTROL, 11);
    VM_ENTRY_EXCEPTION_ERROR_CODE = 0x4018 => (WIDTH_32, CONTROL, 12);
    VM_ENTRY_INSTRUCTION_LEN = 0x401a => (WIDTH_32, CONTROL, 13);
    SECONDARY_VM_EXEC_CONTROL = 0x401e => (WIDTH_32, CONTROL, 15);

    VM_INSTRUCTION_ERROR = 0x4400 => (WIDTH_32, EXIT_INFORMATION, 0);
    VM_EXIT_REASON = 0x4402 => (WIDTH_32, EXIT_INFORMATION, 1);
    VM_EXIT_INTR_INFO = 0x4404 => (WIDTH_32, EXIT_INFORMATION, 2);
    VM_EXIT_INTR_ERROR_CODE = 0x4406 => (WIDTH_32, EXIT_INFORMATION, 3);
    IDT_VECTORING_INFO_FIELD = 0x4408 => (WIDTH_32, EXIT_INFORMATION, 4);
    IDT_VECTORING_ERROR_CODE = 0x440a => (WIDTH_32, EXIT_INFORMATION, 5);
    VM_EXIT_INSTRUCTION_LEN = 0x440c => (WIDTH_32, EXIT_INFORMATION, 6);
    VMX_INSTRUCTION_INFO = 0x440e => (WIDTH_32, EXIT_INFORMATION, 7);

    GUEST_ES_LIMIT = 0x4800 => (WIDTH_32, GUEST_STATE, 0);
    GUEST_CS_LIMIT = 0x4802 => (WIDTH_32, GUEST_STATE, 1);
    GUEST_SS_LIMIT = 0x4804 => (WIDTH_32, GUEST_STATE, 2);
    GUEST_DS_LIMIT = 0x4806 => (WIDTH_32, GUEST_STATE, 3);
    GUEST_FS_LIMIT = 0x4808 => (WIDTH_32, GUEST_STATE, 4);
    GUEST_GS_LIMIT = 0x480a => (WIDTH_32, GUEST_STATE, 5);
    GUEST_LDTR_LIMIT = 0x480c => (WIDTH_32, GUEST_STATE, 6);
    GUEST_TR_LIMIT = 0x480e => (WIDTH_32, GUEST_STATE, 7);
    GUEST_GDTR_LIMIT = 0x4810 => (WIDTH_32, GUEST_STATE, 8);
    GUEST_IDTR_LIMIT = 0x4812 => (WIDTH_32, GUEST_STATE, 9);
    GUEST_ES_AR_BYTES = 0x4814 => (WIDTH_32, GUEST_STATE, 10);
    GUEST_CS_AR_BYTES = 0x4816 => (WIDTH_32, GUEST_STATE, 11);
    GUEST_SS_AR_BYTES = 0x4818 => (WIDTH_32, GUEST_STATE, 12);
    GUEST_DS_AR_BYTES = 0x481a => (WIDTH_32, GUEST_STATE, 13);
    GUEST_FS_AR_BYTES = 0x481c => (WIDTH_32, GUEST_STATE, 14);
    GUEST_GS_AR_BYTES = 0x481e => (WIDTH_32, GUEST_STATE, 15);
    GUEST_LDTR_AR_BYTES = 0x4820 => (WIDTH_32, GUEST_STATE, 16);
    GUEST_TR_AR_BYTES = 0x4822 => (WIDTH_32, GUEST_STATE, 17);
    GUEST_INTERRUPTIBILITY_INFO = 0x4824 => (WIDTH_32, GUEST_STATE, 18);
    GUEST_ACTIVITY_STATE = 0x4826 => (WIDTH_32, GUEST_STATE, 19);
    GUEST_SYSENTER_CS = 0x482a => (WIDTH_32, GUEST_STATE, 21);
    HOST_IA32_SYSENTER_CS = 0x4c00 => (WIDTH_32, HOST_STATE, 0);

    CR0_GUEST_HOST_MASK = 0x6000 => (WIDTH_NATURAL, CONTROL, 0);
    CR4_GUEST_HOST_MASK = 0x6002 => (WIDTH_NATURAL, CONTROL, 1);
    CR0_READ_SHADOW = 0x6004 => (WIDTH_NATURAL, CONTROL, 2);
    CR4_READ_SHADOW = 0x6006 => (WIDTH_NATURAL, CONTROL, 3);

    EXIT_QUALIFICATION = 0x6400 => (WIDTH_NATURAL, EXIT_INFORMATION, 0);
    GUEST_LINEAR_ADDRESS = 0x640a => (WIDTH_NATURAL, EXIT_INFORMATION, 5);

    GUEST_CR0 = 0x6800 => (WIDTH_NATURAL, GUEST_STATE, 0);
    GUEST_CR3 = 0x6802 => (WIDTH_NATURAL, GUEST_STATE, 1);
    GUEST_CR4 = 0x6804 => (WIDTH_NATURAL, GUEST_STATE, 2);
    GUEST_ES_BASE = 0x6806 => (WIDTH_NATURAL, GUEST_STATE, 3);
    GUEST_CS_BASE = 0x6808 => (WIDTH_NATURAL, GUEST_STATE, 4);
    GUEST_SS_BASE = 0x680a => (WIDTH_NATURAL, GUEST_STATE, 5);
    GUEST_DS_BASE = 0x680c => (WIDTH_NATURAL, GUEST_STATE, 6);
    GUEST_FS_BASE = 0x680e => (WIDTH_NATURAL, GUEST_STATE, 7);
    GUEST_GS_BASE = 0x6810 => (WIDTH_NATURAL, GUEST_STATE, 8);
    GUEST_LDTR_BASE = 0x6812 => (WIDTH_NATURAL, GUEST_STATE, 9);
    GUEST_TR_BASE = 0x6814 => (WIDTH_NATURAL, GUEST_STATE, 10);
    GUEST_GDTR_BASE = 0x6816 => (WIDTH_NATURAL, GUEST_STATE, 11);
    GUEST_IDTR_BASE = 0x6818 => (WIDTH_NATURAL, GUEST_STATE, 12);
    GUEST_DR7 = 0x681a => (WIDTH_NATURAL, GUEST_STATE, 13);
    GUEST_RSP = 0x681c => (WIDTH_NATURAL, GUEST_STATE, 14);
    GUEST_RIP = 0x681e => (WIDTH_NATURAL, GUEST_STATE, 15);
    GUEST_RFLAGS = 0x6820 => (WIDTH_NATURAL, GUEST_STATE, 16);
    GUEST_PENDING_DBG_EXCEPTIONS = 0x6822 => (WIDTH_NATURAL, GUEST_STATE, 17);
    GUEST_SYSENTER_ESP = 0x6824 => (WIDTH_NATURAL, GUEST_STATE, 18);
    GUEST_SYSENTER_EIP = 0x6826 => (WIDTH_NATURAL, GUEST_STATE, 19);

    HOST_CR0 = 0x6c00 => (WIDTH_NATURAL, HOST_STATE, 0);
    HOST_CR3 = 0x6c02 => (WIDTH_NATURAL, HOST_STATE, 1);
    HOST_CR4 = 0x6c04 => (WIDTH_NATURAL, HOST_STATE, 2);
    HOST_FS_BASE = 0x6c06 => (WIDTH_NATURAL, HOST_STATE, 3);
    HOST_GS_BASE = 0x6c08 => (WIDTH_NATURAL, HOST_STATE, 4);
    HOST_TR_BASE = 0x6c0a => (WIDTH_NATURAL, HOST_STATE, 5);
    HOST_GDTR_BASE = 0x6c0c => (WIDTH_NATURAL, HOST_STATE, 6);
    HOST_IDTR_BASE = 0x6c0e => (WIDTH_NATURAL, HOST_STATE, 7);
    HOST_IA32_SYSENTER_ESP = 0x6c10 => (WIDTH_NATURAL, HOST_STATE, 8);
    HOST_IA32_SYSENTER_EIP = 0x6c12 => (WIDTH_NATURAL, HOST_STATE, 9);
    HOST_RSP = 0x6c14 => (WIDTH_NATURAL, HOST_STATE, 10);
    HOST_RIP = 0x6c16 => (WIDTH_NATURAL, HOST_STATE, 11);
}

/// Primary processor control that activates secondary controls.
pub const PRIMARY_EXEC_ACTIVATE_SECONDARY_CONTROLS: u32 = 1 << 31;
/// Primary processor control that selects the four-kibibyte MSR bitmap.
pub const PRIMARY_EXEC_USE_MSR_BITMAPS: u32 = 1 << 28;
/// Secondary processor control that enables EPT.
pub const SECONDARY_EXEC_ENABLE_EPT: u32 = 1 << 1;
/// Secondary processor control that lets the guest execute `RDTSCP`.
pub const SECONDARY_EXEC_ENABLE_RDTSCP: u32 = 1 << 3;
/// Secondary processor control that lets the guest execute `INVPCID`.
pub const SECONDARY_EXEC_ENABLE_INVPCID: u32 = 1 << 12;
/// Secondary processor control that lets the guest execute `XSAVES` and `XRSTORS`.
pub const SECONDARY_EXEC_ENABLE_XSAVES: u32 = 1 << 20;
/// Secondary processor control that lets the guest execute `UMWAIT` and `TPAUSE`.
pub const SECONDARY_EXEC_ENABLE_USER_WAIT_PAUSE: u32 = 1 << 26;
/// Secondary processor control that enables unrestricted guests.
pub const SECONDARY_EXEC_UNRESTRICTED_GUEST: u32 = 1 << 7;

/// VM-exit control selecting 64-bit host mode.
pub const VM_EXIT_HOST_ADDRESS_SPACE_SIZE: u32 = 1 << 9;
/// VM-exit control that loads `IA32_PERF_GLOBAL_CTRL`.
pub const VM_EXIT_LOAD_IA32_PERF_GLOBAL_CTRL: u32 = 1 << 12;
/// VM-exit control that saves `IA32_PAT`.
pub const VM_EXIT_SAVE_IA32_PAT: u32 = 1 << 18;
/// VM-exit control that loads `IA32_PAT`.
pub const VM_EXIT_LOAD_IA32_PAT: u32 = 1 << 19;
/// VM-exit control that saves `IA32_EFER`.
pub const VM_EXIT_SAVE_IA32_EFER: u32 = 1 << 20;
/// VM-exit control that loads `IA32_EFER`.
pub const VM_EXIT_LOAD_IA32_EFER: u32 = 1 << 21;

/// VM-entry control selecting IA-32e guest mode.
pub const VM_ENTRY_IA32E_MODE: u32 = 1 << 9;
/// VM-entry control that loads `IA32_PERF_GLOBAL_CTRL`.
pub const VM_ENTRY_LOAD_IA32_PERF_GLOBAL_CTRL: u32 = 1 << 13;
/// VM-entry control that loads `IA32_PAT`.
pub const VM_ENTRY_LOAD_IA32_PAT: u32 = 1 << 14;
/// VM-entry control that loads `IA32_EFER`.
pub const VM_ENTRY_LOAD_IA32_EFER: u32 = 1 << 15;

/// Unusable-segment bit in a guest access-rights field.
pub const GUEST_SEGMENT_UNUSABLE: u32 = 1 << 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_encodings_cover_every_width_and_class() {
        assert_eq!(GUEST_ES_SELECTOR, 0x0800);
        assert_eq!(MSR_BITMAP, 0x2004);
        assert_eq!(EPT_POINTER, 0x201a);
        assert_eq!(VM_EXIT_REASON, 0x4402);
        assert_eq!(GUEST_CS_AR_BYTES, 0x4816);
        assert_eq!(EXIT_QUALIFICATION, 0x6400);
        assert_eq!(HOST_RIP, 0x6c16);
    }
}
