use core::marker::PhantomData;

pub trait FieldSpec<Reg> {
    const OFF: u32;
    const SZ: u32;
}

pub trait FieldReadable<Reg>: FieldSpec<Reg> {}

pub trait FieldWritable<Reg>: FieldSpec<Reg> {}

pub struct Field<Reg, const OFF: u32, const SZ: u32>(pub PhantomData<Reg>);

impl<Reg, const OFF: u32, const SZ: u32> FieldSpec<Reg> for Field<Reg, OFF, SZ> {
    const OFF: u32 = OFF;
    const SZ: u32 = SZ;
}

impl<Reg, const OFF: u32, const SZ: u32> Field<Reg, OFF, SZ> {
    #[inline]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

/// bitregs macro
///
/// - Integer types: u8 / u16 / u32 / u64 / u128
/// - Bit ranges: `@[MSB:LSB]` (ARM-like) or `(offset, size)`
/// - Reserved: `reserved@[..] [res0|res1|ignore]` / `reserved(off, sz) [..]`
/// - Compile-time checks:
///   * Every item fits into the register width
///   * **Full coverage**: every bit is covered by a field or reserved
///   * **No overlap**: any two regions do not overlap
///   * Enum values fit the declared width
/// - `bits()` **applies res0/res1 policy** (encode integrated)
/// - `new()`/`Default` start with res1 bits set, res0 cleared
///
/// Usage:
/// 'Foo::new().set(Foo::bar1, 0b1).set_enum(Foo::bar2, Bar2::baz1).bits();'
///
/// Example:
/// ```rust
/// use typestate::bitregs;
/// bitregs!{
///     pub struct Foo: u32 {
///         pub bar1@[3:0],
///         reserved@[7:4] [res0],
///         pub bar2@[9:8] as Bar2 {
///             baz1 = 0b01,
///             baz2 = 0b10,
///             baz3 = 0b11,
///         },
///         reserved@[31:10] [ignore],
///     }
/// }
/// ```
#[macro_export]
macro_rules! bitregs {
    // Entry: `struct Name : Ty { ... }`
    ( $(#[$m:meta])* $vis:vis struct $Name:ident : $ty:ty { $($body:tt)* } ) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash /*, ::typestate_macro::RawReg*/)]
        $vis struct $Name($ty);

        const _: () = {
            let cond = (0 as $ty) < (!0 as $ty);
            let _ = [()][(!cond) as usize];
        };

        impl $Name {
            // --- Construction -------------------------------------------------
            /// Construct from raw bits (unchecked).
            #[inline] pub const fn from_bits(bits: $ty) -> Self { Self(bits) }

            /// Default constructor obeying reserved rules:
            /// - [res1] bits are set to 1
            /// - [res0] bits are set to 0
            #[inline] pub const fn new() -> Self { Self(Self::__RES1_MASK) }

            // --- Raw I/O ------------------------------------------------------
            /// Returns bits **with res0/res1 policy applied**.
            #[inline] pub const fn bits(self) -> $ty {
                (self.0 & !Self::__RES0_MASK) | Self::__RES1_MASK
            }

            /// Replace raw bits (builder style).
            #[inline] pub const fn with_bits(mut self, bits: $ty) -> Self { self.0 = bits; self }

            // --- Type-level field API ----------------------------------------
            /// Generic getter via a value-level field marker.
            pub fn get<F>(&self, _f: F) -> $ty
            where
                F: $crate::bitflags::FieldSpec<$Name>,
            {
                let off = F::OFF; let sz = F::SZ;
                let bits = (core::mem::size_of::<$ty>() as u32) * 8;
                debug_assert!(sz > 0 && off < bits && off + sz <= bits, "bitregs:get: out-of-range field");
                let val: $ty = if sz >= bits { !0 as $ty } else { ((1 as $ty) << sz) - (1 as $ty) };
                (self.0 >> off) & val
            }

            /// Generic setter via a value-level field marker (builder style).
            pub fn set<F>(mut self, _f: F, v: $ty) -> Self
            where
                F: $crate::bitflags::FieldSpec<$Name>,
            {
                let off = F::OFF; let sz = F::SZ;
                let bits = (core::mem::size_of::<$ty>() as u32) * 8;
                debug_assert!(sz > 0 && off < bits && off + sz <= bits, "bitregs:set: out-of-range field");
                let val: $ty = if sz >= bits { !0 as $ty } else { ((1 as $ty) << sz) - (1 as $ty) };
                let mask: $ty = val << off;
                self.0 = (self.0 & !mask) | ((v & val) << off);
                self
            }

            /// Enum getter (returns `Option<Enum>`).
            pub fn get_enum<F, E>(&self, _f: F) -> Option<E>
            where
                F: $crate::bitflags::FieldSpec<$Name>,
                E: ::core::convert::TryFrom<$ty>,
            { ::core::convert::TryFrom::try_from(self.get(_f)).ok() }

            /// Enum setter (builder style).
            pub fn set_enum<F, E>(mut self, _f: F, e: E) -> Self
            where
                F: $crate::bitflags::FieldSpec<$Name>,
                E: Copy + Into<$ty>,
            { self = self.set(_f, e.into()); self }
        }

        impl ::core::default::Default for $Name {
            #[inline] fn default() -> Self { Self::new() }
        }

        // --- Expand fields & reserved items ----------------------------------
        bitregs!{ @fields $Name : $ty ; $($body)* }

        // --- Reserved masks / coverage / overlap checks ----------------------
        impl $Name {
            /// Mask of all [res0] bits (forced to 0 by `bits()`/`new()`).
            const __RES0_MASK: $ty = bitregs!{@collect_res<$ty> res0; 0 as $ty; $($body)*};
            /// Mask of all [res1] bits (forced to 1 by `bits()`/`new()`).
            const __RES1_MASK: $ty = bitregs!{@collect_res<$ty> res1; 0 as $ty; $($body)*};

            /// Union of all declared ranges (fields + reserved).
            const __DECLARED_MASK: $ty = bitregs!{@collect_mask<$ty>; 0 as $ty; $($body)*};

            /// Overlap mask: OR of pairwise intersections while folding.
            const __OVERLAP_MASK: $ty = bitregs!{@collect_overlap<$ty>; 0 as $ty; 0 as $ty; $($body)*};
        }

        // Full coverage assert
        const _: () = {
            let full: $ty = !0 as $ty;
            let covered_all = $Name::__DECLARED_MASK == full;
            let _ = [()][(!covered_all) as usize]; // compile error if not fully covered
        };
        // No-overlap assert
        const _: () = {
            let no_overlap = $Name::__OVERLAP_MASK == (0 as $ty);
            let _ = [()][(!no_overlap) as usize]; // compile error if any region overlaps
        };

        // Simple Debug
        impl ::core::fmt::Debug for $Name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                write!(f, concat!(stringify!($Name), "({:#x})"), self.0)
            }
        }
    };

    (@assert_resattr res0) => {};
    (@assert_resattr res1) => {};
    (@assert_resattr ignore) => {};
    (@assert_resattr $other:ident) => {
        compile_error!("bitregs: reserved attribute must be one of [res0|res1|ignore]");
    };

    // =========================
    // Field/Reserved expansion
    // =========================

    (@fields $Name:ident : $ty:ty ; ) => {};

    // ---- reserved @[MSB:LSB] → normalized (off, sz)
    (@fields $Name:ident : $ty:ty ;
        reserved @ [ $msb:tt : $lsb:tt ] [ $attr:ident ] , $($rest:tt)*
    ) => {
        bitregs!{ @fields $Name : $ty ;
            reserved ( ($lsb) , (($msb) - ($lsb) + 1) ) [ $attr ],
            $($rest)*
        }
    };
    (@fields $Name:ident : $ty:ty ;
        reserved @ [ $msb:tt : $lsb:tt ] [ $attr:ident ]
    ) => {
        bitregs!{ @fields $Name : $ty ;
            reserved ( ($lsb) , (($msb) - ($lsb) + 1) ) [ $attr ],
        }
    };

    // ---- reserved (off, sz)
    (@fields $Name:ident : $ty:ty ;
        reserved ( $off:expr , $sz:expr ) [ $attr:ident ] , $($rest:tt)*
    ) => {
        const _: () = {
            let bits = (core::mem::size_of::<$ty>() as u32) * 8;
            let off = $off as u32; let sz = $sz as u32;
            let cond = sz > 0 && off < bits && off + sz <= bits;
            let _ = [()][(!cond) as usize];
        };
        bitregs!{ @fields $Name : $ty ; $($rest)* }
    };
    (@fields $Name:ident : $ty:ty ;
        reserved ( $off:expr , $sz:expr ) [ $attr:ident ]
    ) => {
        const _: () = {
            let bits = (core::mem::size_of::<$ty>() as u32) * 8;
            let off = $off as u32; let sz = $sz as u32;
            let cond = sz > 0 && off < bits && off + sz <= bits;
            let _ = [()][(!cond) as usize];
        };
    };

    // ---- plain field @[MSB:LSB]
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ] , $($rest:tt)*
    ) => {
        bitregs!{ @fields $Name : $ty ;
            $fvis $Field ( ($lsb) , (($msb) - ($lsb) + 1) ),
            $($rest)*
        }
    };
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ]
    ) => {
        bitregs!{ @fields $Name : $ty ;
            $fvis $Field ( ($lsb) , (($msb) - ($lsb) + 1) ),
        }
    };

    // ---- enum field @[MSB:LSB]
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ] as $E:ident
        { $($V:ident = $val:expr),+ $(,)? } , $($rest:tt)*
    ) => {
        bitregs!{ @fields $Name : $ty ;
            $fvis $Field ( ($lsb) , (($msb) - ($lsb) + 1) ) as $E { $($V = $val),+ },
            $($rest)*
        }
    };
    // ---- enum field @[MSB:LSB] （末尾カンマなし版を追加）
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ] as $E:ident
        { $($V:ident = $val:expr),+ $(,)? }
    ) => {
        bitregs!{ @fields $Name : $ty ;
            $fvis $Field ( ($lsb) , (($msb) - ($lsb) + 1) ) as $E { $($V = $val),+ }
        }
    };
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ] as $E:ident [ $acc:ident ]
        { $($V:ident = $val:expr),+ $(,)? }
    ) => { compile_error!("bitregs: access qualifiers like [rw]/[ro]/[w]/[r]/[wo] are not supported; remove the [...]"); };

    // ---- plain field (off, sz)
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) , $($rest:tt)*
    ) => {
        impl $Name {
            /// Value-level field marker (associated const)
            #[allow(non_upper_case_globals)]
            $fvis const $Field:
                $crate::bitflags::Field<$Name, { ($off as u32) }, { ($sz as u32) }> =
                $crate::bitflags::Field::<$Name, { ($off as u32) }, { ($sz as u32) }>::new();
        }
        const _: () = {
            let bits = (core::mem::size_of::<$ty>() as u32) * 8;
            let off = $off as u32; let sz = $sz as u32;
            let cond = sz > 0 && off < bits && off + sz <= bits;
            let _ = [()][(!cond) as usize];
        };
        bitregs!{ @fields $Name : $ty ; $($rest)* }
    };
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) [ $acc:ident ]
    ) => { compile_error!("bitregs: access qualifiers are not supported; remove the [...]"); };

    // ---- enum field (off, sz)
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident { $($V:ident = $val:expr),+ $(,)? } , $($rest:tt)*
    ) => {
        #[repr(u128)]
        #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
        $fvis enum $E { $( $V = $val ),+ }
        impl From<$E> for $ty { fn from(e: $E) -> $ty { e as u128 as $ty } }
        impl ::core::convert::TryFrom<$ty> for $E {
            type Error = ();
            fn try_from(x: $ty) -> Result<Self, ()> {
                match x as u128 { $( v if v == $E::$V as u128 => Ok($E::$V), )+ _ => Err(()) }
            }
        }
        const _: () = {
            let sz = $sz as u32;
            let vmax = if sz >= 128 { u128::MAX } else { (1u128 << sz) - 1 };
            $( { let cond = ($val as u128) <= vmax; let _ = [()][(!cond) as usize]; } )+
        };

        impl $Name {
            #[allow(non_upper_case_globals)]
            $fvis const $Field:
                $crate::bitflags::Field<$Name, { ($off as u32) }, { ($sz as u32) }> =
                $crate::bitflags::Field::<$Name, { ($off as u32) }, { ($sz as u32) }>::new();
        }
        const _: () = {
            let bits = (core::mem::size_of::<$ty>() as u32) * 8;
            let off = $off as u32; let sz = $sz as u32;
            let cond = sz > 0 && off < bits && off + sz <= bits;
            let _ = [()][(!cond) as usize];
        };
        bitregs!{ @fields $Name : $ty ; $($rest)* }
    };
    // ---- enum field (off, sz) 末尾カンマなし（ここで終端）
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident
        { $($V:ident = $val:expr),+ $(,)? }
    ) => {
        #[repr(u128)]
        #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
        $fvis enum $E { $( $V = $val ),+ }
        impl From<$E> for $ty { fn from(e: $E) -> $ty { e as u128 as $ty } }
        impl ::core::convert::TryFrom<$ty> for $E {
            type Error = ();
            fn try_from(x: $ty) -> Result<Self, ()> {
                match x as u128 { $( v if v == $E::$V as u128 => Ok($E::$V), )+ _ => Err(()) }
            }
        }
        const _: () = {
            let sz = $sz as u32;
            let vmax = if sz >= 128 { u128::MAX } else { (1u128 << sz) - 1 };
            $( { let cond = ($val as u128) <= vmax; let _ = [()][(!cond) as usize]; } )+
        };

        impl $Name {
            #[allow(non_upper_case_globals)]
            $fvis const $Field:
                $crate::bitflags::Field<$Name, { ($off as u32) }, { ($sz as u32) }> =
                $crate::bitflags::Field::<$Name, { ($off as u32) }, { ($sz as u32) }>::new();
        }
        const _: () = {
            let bits = (core::mem::size_of::<$ty>() as u32) * 8;
            let off = $off as u32; let sz = $sz as u32;
            let cond = sz > 0 && off < bits && off + sz <= bits;
            let _ = [()][(!cond) as usize];
        };
    };
    (@fields $Name:ident : $ty:ty ;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident [ $acc:ident ]
        { $($V:ident = $val:expr),+ $(,)? }
    ) => { compile_error!("bitregs: access qualifiers are not supported; remove the [...]"); };

    // =========================
    // Helpers: masks & folding
    // =========================

    // Build a mask (typed) from (off, sz)
    (@mask<$ty:ty> ($off:expr, $sz:expr)) => {{
        let bits = (core::mem::size_of::<$ty>() as u32) * 8;
        ((if ($sz as u32) >= bits { !0 as $ty } else { ((1 as $ty) << ($sz as u32)) - (1 as $ty) }) << ($off as u32))
    }};

    // Reserved mask collectors (for res0/res1)
    (@collect_res<$ty:ty> $k:ident; $acc:expr; ) => { $acc };
    (@collect_res<$ty:ty> res0; $acc:expr; reserved ( $off:expr , $sz:expr ) [ res0 ] , $($rest:tt)* ) => {
        bitregs!{@collect_res<$ty> res0; ($acc | bitregs!{@mask<$ty>($off,$sz)}); $($rest)*}
    };
    (@collect_res<$ty:ty> res0; $acc:expr; reserved ( $off:expr , $sz:expr ) [ res0 ] ) => { ($acc | bitregs!{@mask<$ty>($off,$sz)}) };
    (@collect_res<$ty:ty> res1; $acc:expr; reserved ( $off:expr , $sz:expr ) [ res1 ] , $($rest:tt)* ) => {
        bitregs!{@collect_res<$ty> res1; ($acc | bitregs!{@mask<$ty>($off,$sz)}); $($rest)*}
    };
    (@collect_res<$ty:ty> res1; $acc:expr; reserved ( $off:expr , $sz:expr ) [ res1 ] ) => { ($acc | bitregs!{@mask<$ty>($off,$sz)}) };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; reserved ( $off:expr , $sz:expr ) [ ignore ] , $($rest:tt)* ) => {
        bitregs!{@collect_res<$ty> $k; $acc; $($rest)*}
    };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; reserved ( $off:expr , $sz:expr ) [ ignore ] ) => { $acc };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; reserved @ [ $msb:tt : $lsb:tt ] [ $attr:ident ] , $($rest:tt)* ) => {
        bitregs!{@collect_res<$ty> $k; $acc; reserved( ($lsb), (($msb)-($lsb)+1) )[ $attr ], $($rest)*}
    };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; reserved @ [ $msb:tt : $lsb:tt ] [ $attr:ident ] ) => {
        bitregs!{@collect_res<$ty> $k; $acc; reserved( ($lsb), (($msb)-($lsb)+1) )[ $attr ],}
    };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; $other:tt , $($rest:tt)* ) => {
        bitregs!{@collect_res<$ty> $k; $acc; $($rest)*}
    };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; $other:tt $($rest:tt)+ ) => {
        bitregs!{@collect_res<$ty> $k; $acc; $($rest)*}
    };
    (@collect_res<$ty:ty> $k:ident; $acc:expr; $other:tt ) => { $acc };

    // Coverage collector (union of all ranges)
    (@collect_mask<$ty:ty>; $acc:expr; ) => { $acc };
    (@collect_mask<$ty:ty>; $acc:expr; reserved ( $off:expr , $sz:expr ) [ $attr:ident ] , $($rest:tt)* ) => {
        bitregs!{@collect_mask<$ty>; ($acc | bitregs!{@mask<$ty>($off,$sz)}); $($rest)*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; reserved ( $off:expr , $sz:expr ) [ $attr:ident ] ) => { ($acc | bitregs!{@mask<$ty>($off,$sz)}) };
    (@collect_mask<$ty:ty>; $acc:expr; $fvis:vis $Field:ident ( $off:expr , $sz:expr ) , $($rest:tt)* ) => {
        bitregs!{@collect_mask<$ty>; ($acc | bitregs!{@mask<$ty>($off,$sz)}); $($rest)*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; $fvis:vis $Field:ident ( $off:expr , $sz:expr ) ) => {
        ($acc | bitregs!{@mask<$ty>($off,$sz)})
    };
    (@collect_mask<$ty:ty>; $acc:expr; $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident { $($V:ident = $val:expr),+ $(,)? } , $($rest:tt)* ) => {
        bitregs!{@collect_mask<$ty>; ($acc | bitregs!{@mask<$ty>($off,$sz)}); $($rest)*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident { $($V:ident = $val:expr),+ $(,)? } ) => {
        ($acc | bitregs!{@mask<$ty>($off,$sz)})
    };
    (@collect_mask<$ty:ty>; $acc:expr; $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ] $( $tail:tt )* ) => {
        bitregs!{@collect_mask<$ty>; $acc; $fvis $Field ( ($lsb), (($msb)-($lsb)+1) ) $( $tail )*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; reserved @ [ $msb:tt : $lsb:tt ] [ $attr:ident ] $(,)? $($rest:tt)* ) => {
        bitregs!{@collect_mask<$ty>; $acc; reserved( ($lsb), (($msb)-($lsb)+1) )[ $attr ], $($rest)*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; $other:tt , $($rest:tt)* ) => {
        bitregs!{@collect_mask<$ty>; $acc; $($rest)*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; $other:tt $($rest:tt)+ ) => {
        bitregs!{@collect_mask<$ty>; $acc; $($rest)*}
    };
    (@collect_mask<$ty:ty>; $acc:expr; $other:tt ) => { $acc };

    // Overlap collector (fold with running union & overlap)
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr; ) => { $oacc };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        reserved ( $off:expr , $sz:expr ) [ $attr:ident ] , $($rest:tt)*
    ) => {
        {
        bitregs!{@collect_overlap<$ty>;
            ($uacc | bitregs!{@mask<$ty>($off,$sz)});
            ($oacc | ($uacc & bitregs!{@mask<$ty>($off,$sz)}));
            $($rest)*}
        };
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        reserved ( $off:expr , $sz:expr ) [ $attr:ident ]
    ) => {
        {
            ($oacc | ($uacc & bitregs!{@mask<$ty>($off,$sz)}))
        }
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) [ $accs:ident ] , $($rest:tt)*
    ) => {
        {
            bitregs!{@collect_overlap<$ty>;
                ($uacc | bitregs!{@mask<$ty>($off,$sz)});
                ($oacc | ($uacc & bitregs!{@mask<$ty>($off,$sz)}));
                $($rest)*
            }
        }
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) [ $accs:ident ]
    ) => {
        {
            ($oacc | ($uacc & bitregs!{@mask<$ty>($off,$sz)}))
        }
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident [ $accs:ident ] { $($V:ident = $val:expr),+ $(,)? } , $($rest:tt)*
    ) => {
        {
            bitregs!{@collect_overlap<$ty>;
                ($uacc | bitregs!{@mask<$ty>($off,$sz)});
                ($oacc | ($uacc & bitregs!{@mask<$ty>($off,$sz)}));
                $($rest)*
            }
        }
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        $fvis:vis $Field:ident ( $off:expr , $sz:expr ) as $E:ident [ $accs:ident ] { $($V:ident = $val:expr),+ $(,)? }
    ) => {
        {
            ($oacc | ($uacc & bitregs!{@mask<$ty>($off,$sz)}))
        }
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        $fvis:vis $Field:ident @ [ $msb:tt : $lsb:tt ] $( $tail:tt )*
    ) => {
        bitregs!{@collect_overlap<$ty>; $uacc; $oacc; $fvis $Field ( ($lsb), (($msb)-($lsb)+1) ) $( $tail )*}
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr;
        reserved @ [ $msb:tt : $lsb:tt ] [ $attr:ident ] $(,)? $($rest:tt)*
    ) => {
        bitregs!{@collect_overlap<$ty>; $uacc; $oacc; reserved( ($lsb), (($msb)-($lsb)+1) )[ $attr ], $($rest)*}
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr; $other:tt , $($rest:tt)* ) => {
        bitregs!{@collect_overlap<$ty>; $uacc; $oacc; $($rest)*}
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr; $other:tt $($rest:tt)+ ) => {
        bitregs!{@collect_overlap<$ty>; $uacc; $oacc; $($rest)*}
    };
    (@collect_overlap<$ty:ty>; $uacc:expr; $oacc:expr; $other:tt ) => { $oacc };
}

#[cfg(test)]
mod test {
    use super::*;

    bitregs! {
        pub(super) struct Timer: u32 {
            pub period@[7:0],
            reserved@[15:8] [res0],
            pub enable(16, 1),
            reserved@[23:17] [ignore],
            reserved@[31:24] [res1],
        }
    }

    bitregs! {
        pub(super) struct Status: u16 {
            pub state@[2:0] as State {
                Idle = 0b000,
                Busy = 0b001,
                Done = 0b010,
                Fault = 0b011,
            },
            reserved@[7:3] [res0],
            pub error(8, 1),
            reserved@[13:9] [ignore],
            reserved@[15:14] [res1],
        }
    }

    #[test]
    fn bitregs_applies_reserved_policies() {
        let reg = Timer::new().set(Timer::period, 0xAA).set(Timer::enable, 1);

        assert_eq!(reg.bits(), 0xFF01_00AA);
        assert_eq!(reg.get(Timer::period), 0xAA);
        assert_eq!(reg.get(Timer::enable), 1);
    }

    #[test]
    fn bitregs_enum_roundtrip() {
        let mut reg = Status::new().set_enum(Status::state, State::Done);

        assert_eq!(reg.get_enum(Status::state), Some(State::Done));
        assert_eq!(reg.bits() & 0xC000, 0xC000);
        assert_eq!(reg.bits() & 0x00F8, 0);

        reg = reg.with_bits(0x01FF);
        assert_eq!(reg.get(Status::error), 1);
        assert_eq!(reg.bits() & 0x00F8, 0);
    }

    #[test]
    fn bitregs_enum_invalid_pattern_returns_none() {
        let reg = Status::new().set(Status::state, 0b111);

        assert_eq!(reg.get_enum(Status::state), None::<State>);
    }
}
