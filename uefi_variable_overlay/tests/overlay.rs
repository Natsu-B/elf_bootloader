//! Policy, mapping, and filtered-enumeration checks.

use uefi_variable_overlay::Catalog;
use uefi_variable_overlay::EFI_GLOBAL_VARIABLE_GUID;
use uefi_variable_overlay::Guid;
use uefi_variable_overlay::MONITOR_VENDOR_GUID;
use uefi_variable_overlay::ProfileId;
use uefi_variable_overlay::VariableScope;
use uefi_variable_overlay::classify;
use uefi_variable_overlay::map_private_variable;
use uefi_variable_overlay::unmap_private_variable;

fn utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

#[test]
fn classification_is_exact_and_keeps_secure_boot_shared() {
    for name in [
        "BootOrder",
        "BootNext",
        "BootCurrent",
        "Boot0000",
        "Boot0A2F",
        "DriverOrder",
        "DriverFFFF",
    ] {
        assert_eq!(
            classify(EFI_GLOBAL_VARIABLE_GUID, &utf16(name)),
            VariableScope::ProfilePrivate,
            "{name}"
        );
    }

    for name in [
        "PK",
        "KEK",
        "db",
        "dbx",
        "BootOptionSupport",
        "Boot00000",
        "Boot00af",
        "DriverCurrent",
    ] {
        assert_eq!(
            classify(EFI_GLOBAL_VARIABLE_GUID, &utf16(name)),
            VariableScope::Shared,
            "{name}"
        );
    }

    let other_guid = Guid::new(1, 2, 3, [4; 8]);
    assert_eq!(
        classify(other_guid, &utf16("BootOrder")),
        VariableScope::Shared
    );
}

#[test]
fn mapping_is_deterministic_and_profile_specific() {
    let logical_name = utf16("BootOrder");
    let windows = ProfileId(0x1234_abcd);
    let linux = ProfileId(0x0000_0002);
    let windows_key =
        map_private_variable(windows, EFI_GLOBAL_VARIABLE_GUID, &logical_name).unwrap();
    let same_key = map_private_variable(windows, EFI_GLOBAL_VARIABLE_GUID, &logical_name).unwrap();
    let linux_key = map_private_variable(linux, EFI_GLOBAL_VARIABLE_GUID, &logical_name).unwrap();

    assert_eq!(windows_key, same_key);
    assert_ne!(windows_key, linux_key);
    assert_eq!(windows_key.guid(), MONITOR_VENDOR_GUID);
    assert_eq!(windows_key.name(), utf16("P1234ABCD:BootOrder"));

    let logical = unmap_private_variable(windows, windows_key.guid(), windows_key.name()).unwrap();
    assert_eq!(logical.guid(), EFI_GLOBAL_VARIABLE_GUID);
    assert_eq!(logical.name(), logical_name);
    assert!(unmap_private_variable(linux, windows_key.guid(), windows_key.name()).is_none());
    assert!(map_private_variable(windows, EFI_GLOBAL_VARIABLE_GUID, &utf16("PK")).is_none());
}

#[test]
fn enumeration_filters_and_translates_backend_keys() {
    let profile = ProfileId(7);
    let other_profile = ProfileId(8);
    let other_guid = Guid::new(9, 8, 7, [6; 8]);
    let selected_boot =
        map_private_variable(profile, EFI_GLOBAL_VARIABLE_GUID, &utf16("Boot0002")).unwrap();
    let other_boot =
        map_private_variable(other_profile, EFI_GLOBAL_VARIABLE_GUID, &utf16("Boot0001")).unwrap();
    let mut catalog = Catalog::<8, 32>::new();

    assert!(
        catalog
            .push_backend(profile, EFI_GLOBAL_VARIABLE_GUID, &utf16("Lang"))
            .unwrap()
    );
    assert!(
        !catalog
            .push_backend(profile, EFI_GLOBAL_VARIABLE_GUID, &utf16("BootOrder"))
            .unwrap()
    );
    assert!(
        !catalog
            .push_backend(profile, other_boot.guid(), other_boot.name())
            .unwrap()
    );
    assert!(
        catalog
            .push_backend(profile, selected_boot.guid(), selected_boot.name())
            .unwrap()
    );
    assert!(
        catalog
            .push_backend(profile, EFI_GLOBAL_VARIABLE_GUID, &utf16("PK"))
            .unwrap()
    );
    assert!(
        catalog
            .push_backend(profile, other_guid, &utf16("BootOrder"))
            .unwrap()
    );
    assert_eq!(catalog.len(), 4);

    let first = catalog.get_next(other_guid, &[]).unwrap();
    assert_eq!(
        (first.guid(), first.name()),
        (EFI_GLOBAL_VARIABLE_GUID, utf16("Lang").as_slice())
    );
    let second = catalog.get_next(first.guid(), first.name()).unwrap();
    assert_eq!(
        (second.guid(), second.name()),
        (EFI_GLOBAL_VARIABLE_GUID, utf16("Boot0002").as_slice())
    );
    let third = catalog.get_next(second.guid(), second.name()).unwrap();
    assert_eq!(
        (third.guid(), third.name()),
        (EFI_GLOBAL_VARIABLE_GUID, utf16("PK").as_slice())
    );
    let fourth = catalog.get_next(third.guid(), third.name()).unwrap();
    assert_eq!(
        (fourth.guid(), fourth.name()),
        (other_guid, utf16("BootOrder").as_slice())
    );
    assert!(catalog.get_next(fourth.guid(), fourth.name()).is_none());
    assert!(
        catalog
            .get_next(EFI_GLOBAL_VARIABLE_GUID, &utf16("missing"))
            .is_none()
    );
}
