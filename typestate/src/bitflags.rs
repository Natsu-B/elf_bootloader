//! Bitfield register definition macros and helpers.

use core::marker::PhantomData;

/// Specifies the bit offset and size of a register field.
pub trait FieldSpec<Reg> {
    /// Bit offset from LSB.
    const OFF: u32;
    /// Field width in bits.
    const SZ: u32;
}

/// Marker trait for fields that can be read.
pub trait FieldReadable<Reg>: FieldSpec<Reg> {}

/// Marker trait for fields that can be written.
pub trait FieldWritable<Reg>: FieldSpec<Reg> {}

/// A compile-time field descriptor with offset and size as const generics.
pub struct Field<Reg, const OFF: u32, const SZ: u32>(pub PhantomData<Reg>);

impl<Reg, const OFF: u32, const SZ: u32> FieldSpec<Reg> for Field<Reg, OFF, SZ> {
    const OFF: u32 = OFF;
    const SZ: u32 = SZ;
}

impl<Reg, const OFF: u32, const SZ: u32> Default for Field<Reg, OFF, SZ> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Reg, const OFF: u32, const SZ: u32> Field<Reg, OFF, SZ> {
    /// Creates a new field descriptor.
    #[inline]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

/// Defines a checked integer register and typed bitfield descriptors.
///
/// Register types may be `u16`, `u32`, or `u64`. Every range uses the inclusive
/// `@[MSB:LSB]` form with integer literals. Fields and reserved ranges must
/// cover the complete register without overlap.
///
/// A reserved range uses one of these policies:
///
/// - `res0`: force the range to zero in [`bits`](#method.bits).
/// - `res1`: force the range to one in [`bits`](#method.bits) and [`new`](#method.new).
/// - `ignore`: retain the stored value.
/// - omitted: retain the stored value.
///
/// Register-level policies affect encoding. Policies inside union views only
/// describe view coverage and do not modify the encoded value.
///
/// A `union` describes alternative complete views of the same absolute bit
/// range. Every view must cover the union range without overlap; unions may be
/// nested inside views.
///
/// # Example
///
/// ```rust
/// use typestate::bitregs;
///
/// bitregs! {
///     pub struct Status: u32 {
///         pub mode@[1:0] as Mode {
///             Idle = 0b00,
///             Run = 0b01,
///         },
///         reserved@[7:2] [res0],
///         union payload@[15:8] {
///             view bytes { pub byte@[15:8], }
///             view halves {
///                 pub low@[11:8],
///                 pub high@[15:12],
///             }
///         }
///         reserved@[31:16] [ignore],
///     }
/// }
///
/// let status = Status::new()
///     .set_enum(Status::mode, Mode::Run)
///     .set(Status::byte, 0x5a);
/// assert_eq!(status.bits(), 0x5a01);
/// ```
#[macro_export]
macro_rules! bitregs {
    ($($tokens:tt)*) => {
        $crate::__bitregs_impl!([$crate] $($tokens)*);
    };
}

#[cfg(test)]
mod tests {
    use crate::RawReg;

    bitregs! {
        /// Register used to exercise recursive union views.
        pub(super) struct PacketNested: u16 {
            union header@[7:0] {
                view split {
                    pub low@[3:0],
                    pub high@[7:4],
                }
                view nested {
                    union nibble@[3:0] {
                        view halves {
                            pub lower@[1:0],
                            pub upper@[3:2],
                        }
                        view raw {
                            pub raw_nibble@[3:0],
                        }
                    }
                    pub top@[7:4],
                }
                view raw {
                    pub raw_byte@[7:0],
                }
            }
            reserved@[15:8] [ignore],
        }
    }

    bitregs! {
        /// Register used to exercise reserved-bit encoding.
        pub(super) struct Timer: u32 {
            pub period@[7:0],
            reserved@[15:8] [res0],
            pub enable@[16:16],
            reserved@[23:17] [ignore],
            reserved@[31:24] [res1],
        }
    }

    bitregs! {
        /// Register used to exercise inline enum conversion.
        pub(super) struct Status: u16 {
            pub state@[2:0] as State {
                Idle = 0b000,
                Busy = 0b001,
                Done = 0b010,
                Fault = 0b011u8,
            },
            reserved@[7:3] [res0],
            pub error@[8:8],
            reserved@[13:9] [ignore],
            reserved@[15:14] [res1],
        }
    }

    #[test]
    fn nested_union_fields_share_storage() {
        let register = PacketNested::new()
            .set(PacketNested::lower, 0b01)
            .set(PacketNested::upper, 0b10)
            .set(PacketNested::top, 0xa);
        assert_eq!(register.bits() & 0xff, 0xa9);

        let register = PacketNested::new().set(PacketNested::raw_byte, 0x5c);
        assert_eq!(register.bits() & 0xff, 0x5c);
        assert_eq!(register.get(PacketNested::high), 5);
        assert_eq!(register.get(PacketNested::low), 0xc);
        assert_eq!(register.get(PacketNested::raw_nibble), 0xc);
    }

    #[test]
    fn reserved_policy_and_enum_round_trip() {
        let timer = Timer::new().with_bits(0x0000_ffff);
        assert_eq!(timer.bits(), 0xff00_00ff);

        let status = Status::new()
            .set_enum(Status::state, State::Done)
            .set(Status::error, 1);
        assert_eq!(status.get_enum(Status::state), Some(State::Done));
        assert_eq!(status.get(Status::error), 1);
        assert_eq!(status.bits() & 0xc000, 0xc000);

        let invalid = Status::from_bits(0b111);
        assert_eq!(invalid.get_enum(Status::state), None::<State>);
    }

    #[test]
    fn raw_access_retains_field_position() {
        let status = Status::new().set_raw(Status::error, 1 << 8);
        assert_eq!(status.get(Status::error), 1);
        assert_eq!(status.get_raw(Status::error), 1 << 8);
        assert_eq!(Status::from_raw(status.to_raw()), status);
    }

    #[test]
    fn primitive_register_type_cannot_be_shadowed() {
        #[allow(non_camel_case_types, dead_code)]
        type u16 = u32;
        bitregs! {
            struct Shadowed: u16 { pub value@[15:0], }
        }
        assert_eq!(core::mem::size_of::<Shadowed>(), 2);
    }
}
