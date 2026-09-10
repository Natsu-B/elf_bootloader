//! Policy, mapping, and filtered-enumeration checks.

use uefi_variable_overlay::Catalog;
use uefi_variable_overlay::EFI_GLOBAL_VARIABLE_GUID;
use uefi_variable_overlay::Guid;
use uefi_variable_overlay::MONITOR_VENDOR_GUID;
use uefi_variable_overlay::ProfileId;
use uefi_variable_overlay::RuntimeVariableOverlay;
use uefi_variable_overlay::StoredVariable;
use uefi_variable_overlay::VariableBackend;
use uefi_variable_overlay::VariableInfo;
use uefi_variable_overlay::VariableScope;
use uefi_variable_overlay::VariableStatus;
use uefi_variable_overlay::classify;
use uefi_variable_overlay::map_private_variable;
use uefi_variable_overlay::unmap_private_variable;

fn utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

#[test]
fn primary_profiles_preserve_existing_persistent_names() {
    use uefi_variable_overlay::UefiProfile;
    for (profile, id, name) in [
        (UefiProfile::Windows, 1, "P00000001:BootOrder"),
        (UefiProfile::Linux, 2, "P00000002:BootOrder"),
    ] {
        assert_eq!(profile.id(), ProfileId(id));
        assert_eq!(UefiProfile::from_id(profile.id()), Some(profile));
        let key = map_private_variable(profile.id(), EFI_GLOBAL_VARIABLE_GUID, &utf16("BootOrder"))
            .unwrap();
        assert_eq!(key.name(), utf16(name));
    }
    for invalid in [0, 3, u32::MAX] {
        assert_eq!(UefiProfile::from_id(ProfileId(invalid)), None);
    }
}

#[test]
fn classification_is_exact_and_keeps_secure_boot_shared() {
    for name in [
        "BootOrder",
        "BootNext",
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
        "BootCurrent",
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
fn boot_current_passes_through_while_boot_selection_state_is_private() {
    let profile = ProfileId(7);

    for name in ["BootOrder", "BootNext", "Boot0000", "BootFFFF"] {
        assert!(
            map_private_variable(profile, EFI_GLOBAL_VARIABLE_GUID, &utf16(name)).is_some(),
            "{name}"
        );
    }

    let boot_current = utf16("BootCurrent");
    assert_eq!(
        classify(EFI_GLOBAL_VARIABLE_GUID, &boot_current),
        VariableScope::Shared
    );
    assert!(map_private_variable(profile, EFI_GLOBAL_VARIABLE_GUID, &boot_current).is_none());
    assert!(
        unmap_private_variable(
            profile,
            MONITOR_VENDOR_GUID,
            &utf16("P00000007:BootCurrent")
        )
        .is_none()
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

struct Record {
    guid: Guid,
    name: Vec<u16>,
    attributes: u32,
    data: Vec<u8>,
}

#[derive(Default)]
struct MemoryBackend {
    records: Vec<Record>,
    queried_attributes: core::cell::Cell<u32>,
}

impl VariableBackend for MemoryBackend {
    fn get_variable(&self, guid: Guid, name: &[u16]) -> Result<StoredVariable<'_>, VariableStatus> {
        self.records
            .iter()
            .find(|record| record.guid == guid && record.name == name)
            .map(|record| StoredVariable {
                attributes: record.attributes,
                data: &record.data,
            })
            .ok_or(VariableStatus::NotFound)
    }

    fn set_variable(
        &mut self,
        guid: Guid,
        name: &[u16],
        attributes: u32,
        data: &[u8],
    ) -> VariableStatus {
        let existing = self
            .records
            .iter()
            .position(|record| record.guid == guid && record.name == name);
        if data.is_empty() {
            return existing.map_or(VariableStatus::NotFound, |index| {
                self.records.remove(index);
                VariableStatus::Success
            });
        }
        if let Some(index) = existing {
            self.records[index].attributes = attributes;
            self.records[index].data = data.to_vec();
        } else {
            self.records.push(Record {
                guid,
                name: name.to_vec(),
                attributes,
                data: data.to_vec(),
            });
        }
        VariableStatus::Success
    }

    fn visit_keys(&self, visitor: &mut dyn FnMut(Guid, &[u16]) -> bool) -> VariableStatus {
        for record in &self.records {
            if !visitor(record.guid, &record.name) {
                break;
            }
        }
        VariableStatus::Success
    }

    fn query_variable_info(&self, attributes: u32) -> Result<VariableInfo, VariableStatus> {
        self.queried_attributes.set(attributes);
        Ok(VariableInfo {
            maximum_storage_size: 4096,
            remaining_storage_size: 3072,
            maximum_variable_size: 1024,
        })
    }
}

#[test]
fn runtime_adapter_dispatches_four_operations_and_isolates_profiles() {
    let windows = ProfileId(1);
    let linux = ProfileId(2);
    let guid = EFI_GLOBAL_VARIABLE_GUID;
    let boot_order = utf16("BootOrder");
    let lang = utf16("Lang");
    let attributes = 7;
    let mut backend = MemoryBackend::default();

    {
        let mut overlay = RuntimeVariableOverlay::<_, 8, 32>::new(windows, &mut backend);
        assert_eq!(
            overlay.set_variable(guid, &boot_order, attributes, &[1, 0]),
            VariableStatus::Success
        );
        assert_eq!(
            overlay.set_variable(guid, &lang, attributes, b"en-US"),
            VariableStatus::Success
        );

        let mut too_small = [0xaa];
        let result = overlay.get_variable(guid, &boot_order, &mut too_small);
        assert_eq!(result.status, VariableStatus::BufferTooSmall);
        assert_eq!(result.data_size, 2);
        assert_eq!(result.attributes, Some(attributes));
        assert_eq!(too_small, [0xaa]);
    }

    {
        let mut overlay = RuntimeVariableOverlay::<_, 8, 32>::new(linux, &mut backend);
        assert_eq!(
            overlay.get_variable(guid, &boot_order, &mut []).status,
            VariableStatus::NotFound
        );
        assert_eq!(
            overlay.set_variable(guid, &boot_order, attributes, &[2, 0]),
            VariableStatus::Success
        );

        let mut shared = [0; 5];
        let result = overlay.get_variable(guid, &lang, &mut shared);
        assert_eq!(result.status, VariableStatus::Success);
        assert_eq!(result.attributes, Some(attributes));
        assert_eq!(&shared, b"en-US");
    }

    {
        let overlay = RuntimeVariableOverlay::<_, 8, 32>::new(windows, &mut backend);
        let mut data = [0; 2];
        let result = overlay.get_variable(guid, &boot_order, &mut data);
        assert_eq!(result.status, VariableStatus::Success);
        assert_eq!(data, [1, 0]);

        let mut short_name = [0xaaaa; 2];
        let result =
            overlay.get_next_variable_name(Guid::new(0, 0, 0, [0; 8]), &[], &mut short_name);
        assert_eq!(result.status, VariableStatus::BufferTooSmall);
        assert_eq!(result.name_size, (boot_order.len() + 1) * 2);
        assert_eq!(short_name, [0xaaaa; 2]);

        let mut name = [0; 16];
        let first = overlay.get_next_variable_name(Guid::new(0, 0, 0, [0; 8]), &[], &mut name);
        assert_eq!(first.status, VariableStatus::Success);
        assert_eq!(first.guid, Some(EFI_GLOBAL_VARIABLE_GUID));
        assert_eq!(&name[..boot_order.len()], &boot_order);
        assert_eq!(name[boot_order.len()], 0);

        let second = overlay.get_next_variable_name(first.guid.unwrap(), &boot_order, &mut name);
        assert_eq!(second.status, VariableStatus::Success);
        assert_eq!(&name[..lang.len()], &lang);
        assert_eq!(name[lang.len()], 0);
        assert_eq!(
            overlay
                .get_next_variable_name(second.guid.unwrap(), &lang, &mut name)
                .status,
            VariableStatus::NotFound
        );

        assert_eq!(
            overlay.query_variable_info(attributes).unwrap(),
            VariableInfo {
                maximum_storage_size: 4096,
                remaining_storage_size: 3072,
                maximum_variable_size: 1024,
            }
        );
    }
    assert_eq!(backend.queried_attributes.get(), attributes);

    {
        let mut overlay = RuntimeVariableOverlay::<_, 8, 32>::new(linux, &mut backend);
        assert_eq!(
            overlay.set_variable(guid, &boot_order, attributes, &[]),
            VariableStatus::Success
        );
        assert_eq!(
            overlay.get_variable(guid, &boot_order, &mut []).status,
            VariableStatus::NotFound
        );
    }

    let overlay = RuntimeVariableOverlay::<_, 8, 32>::new(windows, &mut backend);
    let mut data = [0; 2];
    assert_eq!(
        overlay.get_variable(guid, &boot_order, &mut data).status,
        VariableStatus::Success
    );
    assert_eq!(data, [1, 0]);
}

#[test]
fn enumeration_rejects_unknown_and_other_profile_cursors_without_changing_outputs() {
    let guid = EFI_GLOBAL_VARIABLE_GUID;
    let mut backend = MemoryBackend::default();
    for (profile, name) in [(ProfileId(1), "Boot0001"), (ProfileId(2), "Boot0002")] {
        let mut overlay = RuntimeVariableOverlay::<_, 8>::new(profile, &mut backend);
        assert_eq!(
            overlay.set_variable(guid, &utf16(name), 7, &[1]),
            VariableStatus::Success
        );
    }
    let overlay = RuntimeVariableOverlay::<_, 8>::new(ProfileId(1), &mut backend);
    for (cursor_guid, name) in [
        (guid, "Boot0002"),
        (guid, "Missing"),
        (MONITOR_VENDOR_GUID, "P00000001:Boot0001"),
        (MONITOR_VENDOR_GUID, "P00000002:Boot0002"),
    ] {
        let mut output = [0xaaaa; 32];
        let result = overlay.get_next_variable_name(cursor_guid, &utf16(name), &mut output);
        assert_eq!(result.status, VariableStatus::InvalidParameter);
        assert_eq!(result.guid, None);
        assert_eq!(result.name_size, 0);
        assert_eq!(output, [0xaaaa; 32]);
    }
    assert_eq!(
        overlay
            .get_next_variable_name(guid, &utf16("Boot0001"), &mut [0; 32])
            .status,
        VariableStatus::NotFound,
    );
}

#[test]
fn empty_profile_has_no_successor_but_rejects_nonempty_cursor() {
    let mut backend = MemoryBackend::default();
    let overlay = RuntimeVariableOverlay::<_, 8>::new(ProfileId(1), &mut backend);
    let guid = EFI_GLOBAL_VARIABLE_GUID;
    assert_eq!(
        overlay.get_next_variable_name(guid, &[], &mut []).status,
        VariableStatus::NotFound
    );
    assert_eq!(
        overlay
            .get_next_variable_name(guid, &utf16("BootOrder"), &mut [0; 16])
            .status,
        VariableStatus::InvalidParameter,
    );
}
