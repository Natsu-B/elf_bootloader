//! Read-only firmware CPU identities, before any AP takeover or VMX ownership.
//!
//! MP Services indices are not APIC IDs. Disabled processors still occupy a
//! slot: an OS may later start them, so they cannot disappear from qualification.

#[cfg(feature = "physical-preflight")]
use crate::SerialPort;
use crate::chainload;
use crate::chainload::Error;
#[cfg(feature = "physical-preflight")]
use core::fmt::Write;
use core::ptr;
use r_efi::efi;
use r_efi::efi::protocols::mp_services as mp;

/// Fixed pre-entry inventory bound, not a promise of supported L1 SMP.
const MAX_CPUS: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Processor {
    pub(crate) apic_id: u32,
    pub(crate) flags: u32,
}

/// Complete validated inventory. Every processor is still firmware-owned.
pub(crate) struct Inventory {
    processors: [Processor; MAX_CPUS],
    pub(crate) total: usize,
    pub(crate) enabled: usize,
    pub(crate) bsp: usize,
}

fn invalid(operation: &'static str) -> Error {
    Error::Firmware(operation, efi::Status::COMPROMISED_DATA.as_usize())
}

impl Inventory {
    /// Query every slot exactly once; no processor startup/disable/reset occurs.
    fn collect(
        total: usize,
        enabled: usize,
        bsp: usize,
        mut query: impl FnMut(usize) -> Result<(u64, u32), Error>,
    ) -> Result<Self, Error> {
        if total == 0 || total > MAX_CPUS || enabled == 0 || enabled > total || bsp >= total {
            return Err(invalid("MP inventory count/current bounds"));
        }
        let mut inventory = Self {
            processors: [Processor::default(); MAX_CPUS],
            total,
            enabled,
            bsp,
        };
        let mut observed_enabled = 0;
        for index in 0..total {
            let (id, flags) = query(index)?;
            let apic_id = u32::try_from(id).map_err(|_| invalid("MP APIC ID width"))?;
            if flags & !7 != 0
                || (flags & mp::PROCESSOR_AS_BSP_BIT != 0) != (index == bsp)
                || (index == bsp && flags & mp::PROCESSOR_ENABLED_BIT == 0)
                || inventory.processors[..index]
                    .iter()
                    .any(|cpu| cpu.apic_id == apic_id)
            {
                return Err(invalid("MP inventory duplicate ID or processor flags"));
            }
            observed_enabled += usize::from(flags & mp::PROCESSOR_ENABLED_BIT != 0);
            inventory.processors[index] = Processor { apic_id, flags };
        }
        if observed_enabled != enabled {
            return Err(invalid("MP inventory enabled count mismatch"));
        }
        Ok(inventory)
    }

    pub(crate) fn processors(&self) -> &[Processor] {
        &self.processors[..self.total]
    }

    #[cfg(feature = "physical-preflight")]
    pub(crate) fn log(&self, serial: &mut SerialPort) {
        for (index, cpu) in self.processors().iter().enumerate() {
            let _ = writeln!(
                serial,
                "thin-hv: CPU inventory index={index} apic_id={} flags={} state=firmware-owned",
                cpu.apic_id, cpu.flags
            );
        }
        let _ = writeln!(
            serial,
            "thin-hv: CPU inventory PASS total={} enabled={} bsp={} project_ap_start=0 project_vmx=0",
            self.total, self.enabled, self.bsp
        );
    }
}

pub(crate) fn read(system: *mut efi::SystemTable) -> Result<Inventory, Error> {
    let services = chainload::boot_services(system)?;
    let mut guid = mp::PROTOCOL_GUID;
    let mut interface = ptr::null_mut();
    // SAFETY: checked live Boot Services and owned GUID/output slots. Locating
    // this firmware protocol does not start or change the ownership of any AP.
    let status =
        unsafe { ((*services).locate_protocol)(&mut guid, ptr::null_mut(), &mut interface) };
    if status.is_error() {
        return Err(Error::Firmware(
            "physical MP Services unavailable",
            status.as_usize(),
        ));
    }
    if interface.is_null() {
        return Err(invalid("physical MP Services null interface"));
    }
    let protocol = interface.cast::<mp::Protocol>();
    let (mut total, mut enabled, mut bsp) = (0, 0, usize::MAX);
    // SAFETY: firmware supplied the live interface; these synchronous read-only
    // calls initialize separate aligned scalar outputs owned on this BSP stack.
    let (count_status, who_status) = unsafe {
        (
            ((*protocol).get_number_of_processors)(protocol, &mut total, &mut enabled),
            ((*protocol).who_am_i)(protocol, &mut bsp),
        )
    };
    if count_status.is_error() {
        return Err(Error::Firmware(
            "physical GetNumberOfProcessors",
            count_status.as_usize(),
        ));
    }
    if who_status.is_error() {
        return Err(Error::Firmware("physical WhoAmI", who_status.as_usize()));
    }
    Inventory::collect(total, enabled, bsp, |index| {
        let mut info = core::mem::MaybeUninit::<mp::ProcessorInformation>::zeroed();
        // SAFETY: collect bounds this index by the successful firmware count.
        // Aligned storage covers the full output, including the untouched union.
        let status =
            unsafe { ((*protocol).get_processor_info)(protocol, index, info.as_mut_ptr()) };
        if status.is_error() {
            return Err(Error::Firmware(
                "physical GetProcessorInfo",
                status.as_usize(),
            ));
        }
        // SAFETY: successful legacy GetProcessorInfo initialized these prefix
        // scalars. Do not form a reference to its unrequested extended union.
        Ok(unsafe {
            (
                ptr::addr_of!((*info.as_ptr()).processor_id).read(),
                ptr::addr_of!((*info.as_ptr()).status_flag).read(),
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_uses_distinct_apic_ids_and_keeps_disabled_processors() {
        for count in [1, 2, 4, 8, MAX_CPUS] {
            let bsp = count - 1;
            let inventory = Inventory::collect(count, count, bsp, |i| {
                Ok((256 + i as u64 * 3, 6 | u32::from(i == bsp)))
            })
            .unwrap();
            assert_eq!(inventory.processors().len(), count);
            assert_eq!(inventory.processors()[bsp].apic_id, 256 + bsp as u32 * 3);
        }
        let inventory =
            Inventory::collect(2, 1, 0, |i| Ok((i as u64, if i == 0 { 7 } else { 4 }))).unwrap();
        assert_eq!(inventory.processors().len(), 2);
        assert_eq!(inventory.processors()[1].flags, 4);
    }

    #[test]
    fn inventory_rejects_partial_or_inconsistent_firmware_answers() {
        for (total, enabled, bsp) in [
            (0, 0, 0),
            (65, 1, 0),
            (2, 3, 0),
            (2, 0, 0),
            (2, 2, 2),
            (usize::MAX, 1, 0),
        ] {
            assert!(
                Inventory::collect(total, enabled, bsp, |_| panic!("out-of-range query")).is_err()
            );
        }
        for (ids, flags, enabled) in [
            ([1, 1], [7, 6], 2),
            ([1, 2], [7, 7], 2),
            ([1, 2], [6, 6], 2),
            ([1, 2], [5, 6], 1),
            ([1, 2], [7, 4], 2),
            ([1, 2], [7, 14], 2),
            ([1, u64::MAX], [7, 6], 2),
        ] {
            assert!(Inventory::collect(2, enabled, 0, |i| Ok((ids[i], flags[i]))).is_err());
        }
        let mut queried = 0;
        assert!(
            Inventory::collect(4, 4, 0, |i| {
                queried += 1;
                if i == 2 {
                    Err(invalid("injected AP query failure"))
                } else {
                    Ok((i as u64, 6 | u32::from(i == 0)))
                }
            })
            .is_err()
        );
        assert_eq!(queried, 3);
    }
}
