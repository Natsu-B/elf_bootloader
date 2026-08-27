#![no_std]
//! Heap-free policy for profile-private UEFI boot variables.
//!
//! This crate contains no firmware ABI code. Names are UTF-16 slices without
//! their terminating NUL; an ABI adapter is responsible for validation and
//! buffer sizing.

/// The maximum encoded backend-name length, in UTF-16 code units.
pub const BACKEND_NAME_CAPACITY: usize = 21;

/// The UEFI global-variable namespace.
pub const EFI_GLOBAL_VARIABLE_GUID: Guid = Guid::new(
    0x8be4_df61,
    0x93ca,
    0x11d2,
    [0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c],
);

/// Private namespace used by the monitor's variable backend.
pub const MONITOR_VENDOR_GUID: Guid = Guid::new(
    0xd7e7_166a,
    0x574a,
    0x4c70,
    [0xa3, 0xd0, 0x55, 0xd8, 0xd6, 0x6d, 0x3a, 0x42],
);

/// A semantic UEFI GUID value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guid {
    /// First GUID field.
    pub data1: u32,
    /// Second GUID field.
    pub data2: u16,
    /// Third GUID field.
    pub data3: u16,
    /// Final eight GUID bytes.
    pub data4: [u8; 8],
}

impl Guid {
    /// Constructs a GUID from its semantic fields.
    #[must_use]
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// Stable identifier for one boot profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfileId(pub u32);

/// Storage policy for a logical UEFI variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VariableScope {
    /// The variable is shared by every profile.
    Shared,
    /// The variable has a separate value for each profile.
    ProfilePrivate,
}

/// Classifies one logical variable.
///
/// Only the standardized boot and driver state in the global namespace is
/// private. In particular, `PK`, `KEK`, `db`, and `dbx` remain shared.
#[must_use]
pub fn classify(guid: Guid, name: &[u16]) -> VariableScope {
    if guid == EFI_GLOBAL_VARIABLE_GUID
        && (equals_ascii(name, b"BootOrder")
            || equals_ascii(name, b"BootNext")
            || equals_ascii(name, b"BootCurrent")
            || numbered_name(name, b"Boot")
            || equals_ascii(name, b"DriverOrder")
            || numbered_name(name, b"Driver"))
    {
        VariableScope::ProfilePrivate
    } else {
        VariableScope::Shared
    }
}

/// Returns whether a logical variable is private to its selected profile.
#[must_use]
pub fn is_profile_private(guid: Guid, name: &[u16]) -> bool {
    classify(guid, name) == VariableScope::ProfilePrivate
}

/// Owned backend key for a profile-private variable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendKey {
    /// Encoded name storage.
    name: [u16; BACKEND_NAME_CAPACITY],
    /// Used prefix of `name`.
    name_len: usize,
}

impl BackendKey {
    /// Returns the backend vendor namespace.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        MONITOR_VENDOR_GUID
    }

    /// Returns the encoded backend name.
    #[must_use]
    pub fn name(&self) -> &[u16] {
        &self.name[..self.name_len]
    }
}

/// Maps a private logical variable to the monitor's backend namespace.
///
/// The stable backend form is `P<PROFILE_HEX>:<LOGICAL_NAME>`.
#[must_use]
pub fn map_private_variable(profile: ProfileId, guid: Guid, name: &[u16]) -> Option<BackendKey> {
    if !is_profile_private(guid, name) || name.len() + 10 > BACKEND_NAME_CAPACITY {
        return None;
    }

    let mut encoded = [0; BACKEND_NAME_CAPACITY];
    encoded[0] = u16::from(b'P');
    for index in 0..8 {
        let shift = (7 - index) * 4;
        encoded[index + 1] = u16::from(hex_digit(((profile.0 >> shift) & 0xf) as u8));
    }
    encoded[9] = u16::from(b':');
    encoded[10..10 + name.len()].copy_from_slice(name);

    Some(BackendKey {
        name: encoded,
        name_len: name.len() + 10,
    })
}

/// A borrowed logical variable identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VariableRef<'a> {
    /// Logical vendor namespace.
    guid: Guid,
    /// Logical name without its terminating NUL.
    name: &'a [u16],
}

impl<'a> VariableRef<'a> {
    /// Returns the logical vendor GUID.
    #[must_use]
    pub const fn guid(self) -> Guid {
        self.guid
    }

    /// Returns the logical variable name.
    #[must_use]
    pub const fn name(self) -> &'a [u16] {
        self.name
    }
}

/// Decodes a backend key when it belongs to `profile`.
#[must_use]
pub fn unmap_private_variable(
    profile: ProfileId,
    guid: Guid,
    name: &[u16],
) -> Option<VariableRef<'_>> {
    if guid != MONITOR_VENDOR_GUID
        || name.len() < 10
        || name[0] != u16::from(b'P')
        || name[9] != u16::from(b':')
    {
        return None;
    }

    let mut encoded_profile = 0_u32;
    for &unit in &name[1..9] {
        encoded_profile = (encoded_profile << 4) | parse_hex_digit(unit)?;
    }
    let logical_name = &name[10..];
    if encoded_profile != profile.0 || !is_profile_private(EFI_GLOBAL_VARIABLE_GUID, logical_name) {
        return None;
    }

    Some(VariableRef {
        guid: EFI_GLOBAL_VARIABLE_GUID,
        name: logical_name,
    })
}

/// Failure while building a fixed-capacity enumeration catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogError {
    /// The catalog contains its configured maximum number of entries.
    Full,
    /// A visible shared variable name exceeds the configured name capacity.
    NameTooLong,
}

/// Snapshot used to implement filtered `GetNextVariableName` enumeration.
///
/// Feed backend keys in firmware enumeration order. Private physical boot
/// variables, other profiles, and internal monitor keys are omitted; mapped
/// keys for the selected profile are exposed under their logical names.
pub struct Catalog<const ENTRY_CAPACITY: usize, const NAME_CAPACITY: usize = 64> {
    /// Visible logical entries in backend enumeration order.
    entries: [CatalogEntry<NAME_CAPACITY>; ENTRY_CAPACITY],
    /// Used prefix of `entries`.
    len: usize,
}

impl<const ENTRY_CAPACITY: usize, const NAME_CAPACITY: usize>
    Catalog<ENTRY_CAPACITY, NAME_CAPACITY>
{
    /// Creates an empty catalog.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [CatalogEntry::empty(); ENTRY_CAPACITY],
            len: 0,
        }
    }

    /// Adds one raw backend key, returning whether it became visible.
    ///
    /// Duplicate logical keys are ignored. The insertion order of visible
    /// keys is retained for subsequent enumeration.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Full`] when no entry slot remains, or
    /// [`CatalogError::NameTooLong`] when a visible name does not fit.
    pub fn push_backend(
        &mut self,
        profile: ProfileId,
        backend_guid: Guid,
        backend_name: &[u16],
    ) -> Result<bool, CatalogError> {
        let logical = if backend_guid == MONITOR_VENDOR_GUID {
            let Some(logical) = unmap_private_variable(profile, backend_guid, backend_name) else {
                return Ok(false);
            };
            logical
        } else {
            if is_profile_private(backend_guid, backend_name) {
                return Ok(false);
            }
            VariableRef {
                guid: backend_guid,
                name: backend_name,
            }
        };

        if logical.name.len() > NAME_CAPACITY {
            return Err(CatalogError::NameTooLong);
        }
        if self.entries[..self.len]
            .iter()
            .any(|entry| entry.matches(logical.guid, logical.name))
        {
            return Ok(false);
        }
        if self.len == ENTRY_CAPACITY {
            return Err(CatalogError::Full);
        }

        self.entries[self.len].set(logical.guid, logical.name);
        self.len += 1;
        Ok(true)
    }

    /// Returns the next visible key after the supplied logical key.
    ///
    /// An empty `previous_name` starts enumeration. An unknown prior key and
    /// the end of the catalog both return `None`, matching UEFI `NOT_FOUND`.
    #[must_use]
    pub fn get_next(&self, previous_guid: Guid, previous_name: &[u16]) -> Option<VariableRef<'_>> {
        let index = if previous_name.is_empty() {
            0
        } else {
            self.entries[..self.len]
                .iter()
                .position(|entry| entry.matches(previous_guid, previous_name))?
                + 1
        };
        self.entries
            .get(index)
            .filter(|_| index < self.len)
            .map(CatalogEntry::as_ref)
    }

    /// Returns the number of visible keys in the snapshot.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the catalog has no visible keys.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const ENTRY_CAPACITY: usize, const NAME_CAPACITY: usize> Default
    for Catalog<ENTRY_CAPACITY, NAME_CAPACITY>
{
    fn default() -> Self {
        Self::new()
    }
}

/// Status returned by the ABI-independent runtime-variable adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VariableStatus {
    /// The operation completed successfully.
    Success,
    /// The requested variable or enumeration successor does not exist.
    NotFound,
    /// The caller-provided output buffer is too small.
    BufferTooSmall,
    /// An input parameter is invalid.
    InvalidParameter,
    /// The variable store or adapter snapshot has insufficient capacity.
    OutOfResources,
    /// The variable store rejected a write.
    WriteProtected,
    /// The requested operation or attribute combination is unsupported.
    Unsupported,
    /// The variable store rejected a write for security-policy reasons.
    SecurityViolation,
    /// The variable store reported an implementation-specific failure.
    DeviceError,
}

/// One variable borrowed from the physical backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoredVariable<'a> {
    /// UEFI variable attributes.
    pub attributes: u32,
    /// Variable payload bytes.
    pub data: &'a [u8],
}

/// Capacity information returned by `QueryVariableInfo`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VariableInfo {
    /// Maximum storage available for variables with the requested attributes.
    pub maximum_storage_size: u64,
    /// Remaining storage available for variables with the requested attributes.
    pub remaining_storage_size: u64,
    /// Maximum size of one variable payload.
    pub maximum_variable_size: u64,
}

/// Minimal persistent-variable store used by [`RuntimeVariableOverlay`].
///
/// Names do not include a terminating NUL. Returning `false` from the
/// `visit_keys` callback stops enumeration successfully. `set_variable` must
/// interpret an empty `data` slice as deletion, matching UEFI `DataSize == 0`.
pub trait VariableBackend {
    /// Reads one physical backend variable without copying its payload.
    ///
    /// # Errors
    ///
    /// Returns the backend's UEFI-equivalent failure status.
    fn get_variable(&self, guid: Guid, name: &[u16]) -> Result<StoredVariable<'_>, VariableStatus>;

    /// Creates, updates, or deletes one physical backend variable.
    fn set_variable(
        &mut self,
        guid: Guid,
        name: &[u16],
        attributes: u32,
        data: &[u8],
    ) -> VariableStatus;

    /// Visits physical keys in the backend's stable enumeration order.
    fn visit_keys(&self, visitor: &mut dyn FnMut(Guid, &[u16]) -> bool) -> VariableStatus;

    /// Reports physical-store capacity for one UEFI attribute combination.
    ///
    /// # Errors
    ///
    /// Returns the backend's UEFI-equivalent failure status.
    fn query_variable_info(&self, attributes: u32) -> Result<VariableInfo, VariableStatus>;
}

/// Result of an ABI-independent `GetVariable` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GetVariableResult {
    /// Completion status.
    pub status: VariableStatus,
    /// Payload size in bytes, including the required size on buffer failure.
    pub data_size: usize,
    /// Attributes on success; error paths leave the ABI output untouched.
    pub attributes: Option<u32>,
}

/// Result of an ABI-independent `GetNextVariableName` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GetNextVariableNameResult {
    /// Completion status.
    pub status: VariableStatus,
    /// Required UTF-16 name size in bytes, including the terminating NUL.
    pub name_size: usize,
    /// Logical vendor GUID on success.
    pub guid: Option<Guid>,
}

/// Profile-selecting adapter for the four UEFI variable runtime operations.
///
/// `ENTRY_CAPACITY` and `NAME_CAPACITY` bound the stack snapshot rebuilt for
/// each enumeration call. Reads and writes do not allocate or retain state.
pub struct RuntimeVariableOverlay<
    'a,
    B: VariableBackend + ?Sized,
    const ENTRY_CAPACITY: usize,
    const NAME_CAPACITY: usize = 64,
> {
    /// Selected OS profile.
    profile: ProfileId,
    /// Physical variable store.
    backend: &'a mut B,
}

impl<'a, B: VariableBackend + ?Sized, const ENTRY_CAPACITY: usize, const NAME_CAPACITY: usize>
    RuntimeVariableOverlay<'a, B, ENTRY_CAPACITY, NAME_CAPACITY>
{
    /// Wraps `backend` with the selected profile policy.
    #[must_use]
    pub const fn new(profile: ProfileId, backend: &'a mut B) -> Self {
        Self { profile, backend }
    }

    /// Dispatches UEFI `GetVariable` and applies EFI buffer-size semantics.
    pub fn get_variable(&self, guid: Guid, name: &[u16], data: &mut [u8]) -> GetVariableResult {
        if name.is_empty() {
            return get_error(VariableStatus::InvalidParameter);
        }

        with_backend_key(self.profile, guid, name, |backend_guid, backend_name| {
            let stored = match self.backend.get_variable(backend_guid, backend_name) {
                Ok(stored) => stored,
                Err(status) => return get_error(status),
            };
            if data.len() < stored.data.len() {
                return GetVariableResult {
                    status: VariableStatus::BufferTooSmall,
                    data_size: stored.data.len(),
                    attributes: None,
                };
            }

            data[..stored.data.len()].copy_from_slice(stored.data);
            GetVariableResult {
                status: VariableStatus::Success,
                data_size: stored.data.len(),
                attributes: Some(stored.attributes),
            }
        })
    }

    /// Dispatches UEFI `SetVariable`; an empty payload deletes the variable.
    pub fn set_variable(
        &mut self,
        guid: Guid,
        name: &[u16],
        attributes: u32,
        data: &[u8],
    ) -> VariableStatus {
        if name.is_empty() {
            return VariableStatus::InvalidParameter;
        }

        with_backend_key(self.profile, guid, name, |backend_guid, backend_name| {
            self.backend
                .set_variable(backend_guid, backend_name, attributes, data)
        })
    }

    /// Dispatches filtered UEFI `GetNextVariableName`.
    ///
    /// `previous_name` and `name` omit/include the terminating NUL
    /// respectively: the input cursor omits it, while a successful output
    /// writes it into `name` and counts it in `name_size`.
    pub fn get_next_variable_name(
        &self,
        previous_guid: Guid,
        previous_name: &[u16],
        name: &mut [u16],
    ) -> GetNextVariableNameResult {
        // ponytail: rebuild a bounded snapshot per call; persist an index only
        // if measured variable counts or firmware latency make this too slow.
        let mut catalog = Catalog::<ENTRY_CAPACITY, NAME_CAPACITY>::new();
        let mut catalog_failed = false;
        let status = self.backend.visit_keys(&mut |guid, backend_name| {
            if catalog
                .push_backend(self.profile, guid, backend_name)
                .is_err()
            {
                catalog_failed = true;
                false
            } else {
                true
            }
        });
        if status != VariableStatus::Success {
            return next_error(status);
        }
        if catalog_failed {
            return next_error(VariableStatus::OutOfResources);
        }

        let Some(next) = catalog.get_next(previous_guid, previous_name) else {
            return next_error(VariableStatus::NotFound);
        };
        let required_units = next.name().len() + 1;
        let required_bytes = required_units * core::mem::size_of::<u16>();
        if name.len() < required_units {
            return GetNextVariableNameResult {
                status: VariableStatus::BufferTooSmall,
                name_size: required_bytes,
                guid: None,
            };
        }

        name[..next.name().len()].copy_from_slice(next.name());
        name[next.name().len()] = 0;
        GetNextVariableNameResult {
            status: VariableStatus::Success,
            name_size: required_bytes,
            guid: Some(next.guid()),
        }
    }

    /// Dispatches UEFI `QueryVariableInfo` to the shared physical store.
    ///
    /// # Errors
    ///
    /// Returns the physical backend's failure status unchanged.
    pub fn query_variable_info(&self, attributes: u32) -> Result<VariableInfo, VariableStatus> {
        self.backend.query_variable_info(attributes)
    }
}

/// Calls `operation` with the physical key selected by the profile policy.
fn with_backend_key<R>(
    profile: ProfileId,
    guid: Guid,
    name: &[u16],
    operation: impl FnOnce(Guid, &[u16]) -> R,
) -> R {
    if let Some(mapped) = map_private_variable(profile, guid, name) {
        operation(mapped.guid(), mapped.name())
    } else {
        operation(guid, name)
    }
}

/// Constructs a `GetVariable` error result without touching ABI outputs.
const fn get_error(status: VariableStatus) -> GetVariableResult {
    GetVariableResult {
        status,
        data_size: 0,
        attributes: None,
    }
}

/// Constructs a `GetNextVariableName` error result without touching outputs.
const fn next_error(status: VariableStatus) -> GetNextVariableNameResult {
    GetNextVariableNameResult {
        status,
        name_size: 0,
        guid: None,
    }
}

/// Owned entry in an enumeration snapshot.
#[derive(Clone, Copy)]
struct CatalogEntry<const NAME_CAPACITY: usize> {
    /// Logical vendor namespace.
    guid: Guid,
    /// Logical name storage.
    name: [u16; NAME_CAPACITY],
    /// Used prefix of `name`.
    name_len: usize,
}

impl<const NAME_CAPACITY: usize> CatalogEntry<NAME_CAPACITY> {
    /// Returns an unused entry.
    const fn empty() -> Self {
        Self {
            guid: Guid::new(0, 0, 0, [0; 8]),
            name: [0; NAME_CAPACITY],
            name_len: 0,
        }
    }

    /// Replaces the entry with one logical key.
    fn set(&mut self, guid: Guid, name: &[u16]) {
        self.guid = guid;
        self.name[..name.len()].copy_from_slice(name);
        self.name_len = name.len();
    }

    /// Tests one logical key for exact equality.
    fn matches(&self, guid: Guid, name: &[u16]) -> bool {
        self.guid == guid && &self.name[..self.name_len] == name
    }

    /// Borrows this entry as a logical key.
    fn as_ref(&self) -> VariableRef<'_> {
        VariableRef {
            guid: self.guid,
            name: &self.name[..self.name_len],
        }
    }
}

/// Compares a UTF-16 name with an ASCII policy name.
fn equals_ascii(name: &[u16], expected: &[u8]) -> bool {
    name.len() == expected.len()
        && name
            .iter()
            .zip(expected)
            .all(|(&unit, &byte)| unit == u16::from(byte))
}

/// Recognizes a standardized prefix followed by four uppercase hex digits.
fn numbered_name(name: &[u16], prefix: &[u8]) -> bool {
    name.len() == prefix.len() + 4
        && equals_ascii(&name[..prefix.len()], prefix)
        && name[prefix.len()..]
            .iter()
            .all(|&unit| parse_hex_digit(unit).is_some())
}

/// Encodes one hexadecimal nibble with the canonical uppercase spelling.
const fn hex_digit(value: u8) -> u8 {
    if value < 10 {
        b'0' + value
    } else {
        b'A' + value - 10
    }
}

/// Parses one canonical uppercase hexadecimal code unit.
fn parse_hex_digit(unit: u16) -> Option<u32> {
    match unit {
        0x30..=0x39 => Some(u32::from(unit - 0x30)),
        0x41..=0x46 => Some(u32::from(unit - 0x41 + 10)),
        _ => None,
    }
}
