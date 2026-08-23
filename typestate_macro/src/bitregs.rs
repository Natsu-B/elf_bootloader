//! Parser, validation, and expansion for the `bitregs!` register DSL.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::HashSet;

use proc_macro2::TokenStream;
use quote::quote;
use syn::Attribute;
use syn::Error;
use syn::Ident;
use syn::LitInt;
use syn::Result;
use syn::Token;
use syn::Visibility;
use syn::braced;
use syn::bracketed;
use syn::parse::Parse;
use syn::parse::ParseStream;
use syn::parse2;

syn::custom_keyword!(reserved);
syn::custom_keyword!(view);

/// Expands a register definition after parsing and validating the complete layout.
pub(crate) fn expand(tokens: TokenStream) -> Result<TokenStream> {
    parse2::<Input>(tokens)?.expand()
}

/// Input passed by the declarative wrapper, including its hygienic crate path.
struct Input {
    crate_path: TokenStream,
    register: Register,
}

impl Parse for Input {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let path;
        bracketed!(path in input);
        let crate_path: TokenStream = path.parse()?;
        if crate_path.is_empty() {
            return Err(path.error("bitregs: missing typestate crate path"));
        }

        let register = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("bitregs: unexpected tokens after register definition"));
        }
        Ok(Self {
            crate_path,
            register,
        })
    }
}

impl Input {
    /// Validates the layout once, then emits only the public register API.
    #[allow(clippy::too_many_lines)]
    fn expand(self) -> Result<TokenStream> {
        let (width, raw) = self.register.raw_type()?;
        let (res0, res1) = Validator::new(width).validate(&self.register)?;

        let Register {
            attrs,
            vis,
            name,
            raw: _,
            items,
        } = self.register;
        let crate_path = self.crate_path;
        let raw_impl = crate::expand_rawreg_impl(&name, &raw, &crate_path);

        let mut fields = Vec::new();
        collect_fields(&items, &mut fields);
        let expanded_fields = fields
            .into_iter()
            .map(|field| field.expand(&name, &raw, &crate_path))
            .collect::<Result<Vec<_>>>()?;

        Ok(quote! {
            #(#attrs)*
            #[repr(transparent)]
            #[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
            #vis struct #name(#raw);

            #raw_impl

            impl #name {
                /// Constructs a register from raw bits without applying reserved-bit policy.
                #[inline]
                pub const fn from_bits(bits: #raw) -> Self { Self(bits) }

                /// Constructs a register with RES1 bits set and all other bits clear.
                #[inline]
                pub const fn new() -> Self { Self(Self::__RES1_MASK) }

                /// Returns the value after applying RES0 and RES1 policy.
                #[inline]
                pub const fn bits(self) -> #raw {
                    (self.0 & !Self::__RES0_MASK) | Self::__RES1_MASK
                }

                /// Replaces the raw value without applying reserved-bit policy.
                #[inline]
                pub const fn with_bits(self, bits: #raw) -> Self { Self(bits) }

                /// Returns the unshifted mask and offset for a field descriptor.
                #[inline]
                fn field_mask<F: #crate_path::bitflags::FieldSpec<Self>>() -> (u32, #raw) {
                    let off = F::OFF;
                    let size = F::SZ;
                    let bits = (::core::mem::size_of::<#raw>() as u32) * 8;
                    ::core::assert!(
                        size > 0 && off < bits && size <= bits - off,
                        "bitregs: invalid field (reg={}, field={}, off={}, size={}, bits={})",
                        stringify!(#name),
                        ::core::any::type_name::<F>(),
                        off,
                        size,
                        bits,
                    );
                    let mask = if size == bits {
                        !0 as #raw
                    } else {
                        ((1 as #raw) << size) - 1
                    };
                    (off, mask)
                }

                /// Reads a field and shifts it to bit zero.
                #[inline]
                pub fn get<F: #crate_path::bitflags::FieldSpec<Self>>(&self, _field: F) -> #raw {
                    let (off, mask) = Self::field_mask::<F>();
                    (self.0 >> off) & mask
                }

                /// Writes an unshifted field value and returns the updated register.
                #[inline]
                pub fn set<F>(mut self, _field: F, value: #raw) -> Self
                where
                    F: #crate_path::bitflags::FieldSpec<Self>,
                {
                    let (off, mask) = Self::field_mask::<F>();
                    ::core::debug_assert!(
                        value & !mask == 0,
                        "bitregs: field value exceeds its width (reg={}, field={}, value={:#x}, mask={:#x})",
                        stringify!(#name),
                        ::core::any::type_name::<F>(),
                        value,
                        mask,
                    );
                    let positioned = mask << off;
                    self.0 = (self.0 & !positioned) | ((value & mask) << off);
                    self
                }

                /// Reads a field while retaining its register bit position.
                #[inline]
                pub fn get_raw<F: #crate_path::bitflags::FieldSpec<Self>>(&self, field: F) -> #raw {
                    self.get(field) << F::OFF
                }

                /// Writes a field value already shifted to its register bit position.
                #[inline]
                pub fn set_raw<F>(self, field: F, value: #raw) -> Self
                where
                    F: #crate_path::bitflags::FieldSpec<Self>,
                {
                    let (off, mask) = Self::field_mask::<F>();
                    let positioned = mask << off;
                    ::core::debug_assert!(
                        value & !positioned == 0,
                        "bitregs: raw field value exceeds its mask (reg={}, field={}, value={:#x}, mask={:#x})",
                        stringify!(#name),
                        ::core::any::type_name::<F>(),
                        value,
                        positioned,
                    );
                    self.set(field, value >> off)
                }

                /// Reads a field and converts a recognized value to its enum.
                #[inline]
                pub fn get_enum<F, E>(&self, field: F) -> ::core::option::Option<E>
                where
                    F: #crate_path::bitflags::FieldSpec<Self>,
                    E: ::core::convert::TryFrom<#raw>,
                {
                    E::try_from(self.get(field)).ok()
                }

                /// Writes an enum value and returns the updated register.
                #[inline]
                pub fn set_enum<F, E>(self, field: F, value: E) -> Self
                where
                    F: #crate_path::bitflags::FieldSpec<Self>,
                    E: ::core::convert::Into<#raw>,
                {
                    self.set(field, value.into())
                }

                /// Mask of bits forced to zero when the register is encoded.
                const __RES0_MASK: #raw = #res0 as #raw;
                /// Mask of bits forced to one when the register is encoded.
                const __RES1_MASK: #raw = #res1 as #raw;
            }

            impl ::core::default::Default for #name {
                #[inline]
                fn default() -> Self { Self::new() }
            }

            impl ::core::fmt::Debug for #name {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    ::core::write!(f, concat!(stringify!(#name), "({:#x})"), self.0)
                }
            }

            #(#expanded_fields)*
        })
    }
}

/// A complete register declaration.
struct Register {
    attrs: Vec<Attribute>,
    vis: Visibility,
    name: Ident,
    raw: Ident,
    items: Vec<Item>,
}

impl Parse for Register {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        let vis = input.parse()?;
        input.parse::<Token![struct]>()?;
        let name = input.parse()?;
        input.parse::<Token![:]>()?;
        let raw = input.parse()?;
        let body;
        braced!(body in input);
        let items = parse_items(&body)?;
        Ok(Self {
            attrs,
            vis,
            name,
            raw,
            items,
        })
    }
}

impl Register {
    /// Returns the width and canonical path of a supported primitive register.
    fn raw_type(&self) -> Result<(u32, TokenStream)> {
        match self.raw.to_string().as_str() {
            "u16" => Ok((16, quote!(::core::primitive::u16))),
            "u32" => Ok((32, quote!(::core::primitive::u32))),
            "u64" => Ok((64, quote!(::core::primitive::u64))),
            _ => Err(Error::new(
                self.raw.span(),
                "bitregs: register type must be u16, u32, or u64",
            )),
        }
    }
}

/// One field, reserved range, or alternative-view union.
enum Item {
    Field(FieldDef),
    Reserved(Reserved),
    Union(Union),
}

impl Item {
    /// Returns the range and diagnostic span for this partition item.
    fn layout(&self) -> (&BitRange, proc_macro2::Span) {
        match self {
            Self::Field(field) => (&field.range, field.name.span()),
            Self::Reserved(item) => (&item.range, item.range.msb.span()),
            Self::Union(union) => (&union.range, union.name.span()),
        }
    }
}

/// A named field and its optional inline enum.
struct FieldDef {
    attrs: Vec<Attribute>,
    vis: Visibility,
    name: Ident,
    range: BitRange,
    values: Option<EnumDef>,
}

impl FieldDef {
    /// Emits the associated descriptor and optional enum conversion API.
    fn expand(
        &self,
        register: &Ident,
        raw: &TokenStream,
        crate_path: &TokenStream,
    ) -> Result<TokenStream> {
        let attrs = &self.attrs;
        let vis = &self.vis;
        let name = &self.name;
        let (msb, lsb) = self.range.values()?;
        let size = msb - lsb + 1;
        let enum_tokens = self
            .values
            .as_ref()
            .map(|values| values.expand(vis, name, raw));

        Ok(quote! {
            impl #register {
                #(#attrs)*
                #[doc = concat!("Descriptor for the `", stringify!(#name), "` bitfield.")]
                #[allow(non_upper_case_globals)]
                #vis const #name: #crate_path::bitflags::Field<#register, #lsb, #size> =
                    #crate_path::bitflags::Field::new();
            }
            #enum_tokens
        })
    }
}

/// An inline enum attached to a field.
struct EnumDef {
    name: Ident,
    variants: Vec<EnumVariant>,
}

impl EnumDef {
    /// Emits the enum and raw-value conversions used by generic field accessors.
    fn expand(&self, vis: &Visibility, field: &Ident, raw: &TokenStream) -> TokenStream {
        let name = &self.name;
        let variants = self.variants.iter().map(|variant| {
            let attrs = &variant.attrs;
            let name = &variant.name;
            let source = &variant.value;
            let value = LitInt::new(source.base10_digits(), source.span());
            quote! {
                #(#attrs)*
                #[doc = concat!("Raw value `", stringify!(#source), "`.")]
                #name = #value
            }
        });
        let names = self.variants.iter().map(|variant| &variant.name);
        let values = self
            .variants
            .iter()
            .map(|variant| LitInt::new(variant.value.base10_digits(), variant.value.span()));

        quote! {
            #[doc = concat!("Values accepted by the `", stringify!(#field), "` bitfield.")]
            #[repr(u128)]
            #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
            #vis enum #name { #(#variants,)* }

            impl From<#name> for #raw {
                #[inline]
                fn from(value: #name) -> #raw { value as u128 as #raw }
            }

            impl ::core::convert::TryFrom<#raw> for #name {
                type Error = ();

                #[inline]
                fn try_from(value: #raw) -> ::core::result::Result<Self, Self::Error> {
                    match value as u128 {
                        #(#values => ::core::result::Result::Ok(Self::#names),)*
                        _ => ::core::result::Result::Err(()),
                    }
                }
            }
        }
    }
}

/// One inline-enum variant.
struct EnumVariant {
    attrs: Vec<Attribute>,
    name: Ident,
    value: LitInt,
}

/// A reserved range and its encoding policy.
struct Reserved {
    range: BitRange,
    policy: ReservedPolicy,
}

/// Encoding policy for one reserved range.
enum ReservedPolicy {
    Preserve,
    Zero,
    One,
}

/// A range with multiple complete interpretations.
struct Union {
    name: Ident,
    range: BitRange,
    views: Vec<View>,
}

/// One complete interpretation of a union range.
struct View {
    name: Ident,
    items: Vec<Item>,
}

/// Inclusive MSB/LSB bit range.
struct BitRange {
    msb: LitInt,
    lsb: LitInt,
}

impl Parse for BitRange {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let body;
        bracketed!(body in input);
        let msb = body.parse()?;
        body.parse::<Token![:]>()?;
        let lsb = body.parse()?;
        if !body.is_empty() {
            return Err(body.error("bitregs: unexpected tokens in bit range"));
        }
        Ok(Self { msb, lsb })
    }
}

impl BitRange {
    /// Parses the inclusive range endpoints.
    fn values(&self) -> Result<(u32, u32)> {
        Ok((self.msb.base10_parse()?, self.lsb.base10_parse()?))
    }

    /// Validates and returns this range's register mask.
    fn mask(&self, width: u32, allowed: u128) -> Result<u128> {
        let (msb, lsb) = self.values()?;
        if msb < lsb {
            return Err(Error::new(
                self.msb.span(),
                "bitregs: range MSB must be greater than or equal to LSB",
            ));
        }
        if msb >= width {
            return Err(Error::new(
                self.msb.span(),
                format!("bitregs: bit {msb} is outside the {width}-bit register"),
            ));
        }
        let mask = low_mask(msb - lsb + 1) << lsb;
        if mask & allowed != mask {
            return Err(Error::new(
                self.msb.span(),
                "bitregs: range is outside its containing register or union",
            ));
        }
        Ok(mask)
    }
}

/// Parses a comma-optional item sequence.
fn parse_items(input: ParseStream<'_>) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    while !input.is_empty() {
        let attrs = input.call(Attribute::parse_outer)?;
        let item = if input.peek(Token![union]) {
            Item::Union(parse_union(input)?)
        } else if input.peek(reserved) {
            Item::Reserved(parse_reserved(input)?)
        } else {
            Item::Field(parse_field(input, attrs)?)
        };
        items.push(item);
        let _ = input.parse::<Option<Token![,]>>()?;
    }
    Ok(items)
}

/// Parses a normal or enum-valued field.
fn parse_field(input: ParseStream<'_>, attrs: Vec<Attribute>) -> Result<FieldDef> {
    let vis = input.parse()?;
    let name = input.parse()?;
    input.parse::<Token![@]>()?;
    let range = input.parse()?;
    let values = if input.peek(Token![as]) {
        input.parse::<Token![as]>()?;
        Some(parse_enum(input)?)
    } else {
        None
    };
    Ok(FieldDef {
        attrs,
        vis,
        name,
        range,
        values,
    })
}

/// Parses an inline enum and its integer-literal variants.
fn parse_enum(input: ParseStream<'_>) -> Result<EnumDef> {
    let name = input.parse()?;
    let body;
    braced!(body in input);
    let mut variants = Vec::new();
    while !body.is_empty() {
        let attrs = body.call(Attribute::parse_outer)?;
        let variant = body.parse()?;
        body.parse::<Token![=]>()?;
        let value = body.parse()?;
        variants.push(EnumVariant {
            attrs,
            name: variant,
            value,
        });
        let _ = body.parse::<Option<Token![,]>>()?;
    }
    Ok(EnumDef { name, variants })
}

/// Parses a reserved range and its res0/res1/ignore policy.
fn parse_reserved(input: ParseStream<'_>) -> Result<Reserved> {
    input.parse::<reserved>()?;
    input.parse::<Token![@]>()?;
    let range = input.parse()?;
    let policy = if input.peek(syn::token::Bracket) {
        let policy_body;
        bracketed!(policy_body in input);
        let policy: Ident = policy_body.parse()?;
        if !policy_body.is_empty() {
            return Err(policy_body.error("bitregs: reserved policy must be one identifier"));
        }
        match policy.to_string().as_str() {
            "res0" => ReservedPolicy::Zero,
            "res1" => ReservedPolicy::One,
            "ignore" => ReservedPolicy::Preserve,
            _ => {
                return Err(Error::new(
                    policy.span(),
                    "bitregs: reserved policy must be res0, res1, or ignore",
                ));
            }
        }
    } else {
        ReservedPolicy::Preserve
    };
    Ok(Reserved { range, policy })
}

/// Parses a union and its comma-optional view sequence.
fn parse_union(input: ParseStream<'_>) -> Result<Union> {
    input.parse::<Token![union]>()?;
    let name = input.parse()?;
    input.parse::<Token![@]>()?;
    let range = input.parse()?;
    let body;
    braced!(body in input);
    let mut views = Vec::new();
    while !body.is_empty() {
        let _attrs = body.call(Attribute::parse_outer)?;
        body.parse::<view>()?;
        let view_name = body.parse()?;
        let view_body;
        braced!(view_body in body);
        views.push(View {
            name: view_name,
            items: parse_items(&view_body)?,
        });
        let _ = body.parse::<Option<Token![,]>>()?;
    }
    Ok(Union { name, range, views })
}

/// Validates names and each independent coverage partition.
struct Validator {
    width: u32,
    fields: HashSet<String>,
    enums: HashSet<String>,
    unions: HashSet<String>,
}

impl Validator {
    /// Creates a validator for one raw register width.
    fn new(width: u32) -> Self {
        Self {
            width,
            fields: HashSet::new(),
            enums: HashSet::new(),
            unions: HashSet::new(),
        }
    }

    /// Validates the root register partition and returns its reserved-bit masks.
    fn validate(mut self, register: &Register) -> Result<(u128, u128)> {
        self.partition(
            &register.items,
            low_mask(self.width),
            register.name.span(),
            "register",
        )
    }

    /// Validates one partition and returns the masks set by its reserved policies.
    fn partition(
        &mut self,
        items: &[Item],
        expected: u128,
        span: proc_macro2::Span,
        kind: &str,
    ) -> Result<(u128, u128)> {
        let mut covered = 0;
        let mut reserved = (0, 0);
        for item in items {
            let (range, span) = item.layout();
            let mask = range.mask(self.width, expected)?;
            if covered & mask != 0 {
                return Err(Error::new(
                    span,
                    "bitregs: field, reserved range, or union overlaps another item",
                ));
            }
            covered |= mask;
            match item {
                Item::Field(field) => self.field(field)?,
                Item::Reserved(item) => match &item.policy {
                    ReservedPolicy::Zero => reserved.0 |= mask,
                    ReservedPolicy::One => reserved.1 |= mask,
                    ReservedPolicy::Preserve => {}
                },
                Item::Union(union) => self.union(union, mask)?,
            }
        }
        if covered != expected {
            return Err(Error::new(
                span,
                format!(
                    "bitregs: {kind} does not cover bits {:#x}",
                    expected & !covered
                ),
            ));
        }
        Ok(reserved)
    }

    /// Validates a field name and its optional inline enum.
    fn field(&mut self, field: &FieldDef) -> Result<()> {
        ensure_unique_name(&mut self.fields, &field.name, "field name")?;
        let Some(values) = &field.values else {
            return Ok(());
        };
        ensure_unique_name(&mut self.enums, &values.name, "enum name")?;
        if values.variants.is_empty() {
            return Err(Error::new(
                values.name.span(),
                "bitregs: inline enum must contain at least one value",
            ));
        }

        let (msb, lsb) = field.range.values()?;
        let maximum = low_mask(msb - lsb + 1);
        let mut names = HashSet::new();
        let mut raw_values = HashSet::new();
        for variant in &values.variants {
            ensure_unique_name(&mut names, &variant.name, "enum variant")?;
            let value = variant.value.base10_parse::<u128>()?;
            if value > maximum {
                return Err(Error::new(
                    variant.value.span(),
                    format!("bitregs: enum value does not fit in {} bits", msb - lsb + 1),
                ));
            }
            if !raw_values.insert(value) {
                return Err(Error::new(
                    variant.value.span(),
                    format!("bitregs: duplicate enum value {value:#x}"),
                ));
            }
        }
        Ok(())
    }

    /// Validates all complete views of a union, including nested unions.
    fn union(&mut self, union: &Union, mask: u128) -> Result<()> {
        ensure_unique_name(&mut self.unions, &union.name, "union name")?;
        if union.views.is_empty() {
            return Err(Error::new(
                union.name.span(),
                "bitregs: union must contain at least one view",
            ));
        }
        let mut names = HashSet::new();
        for view in &union.views {
            ensure_unique_name(&mut names, &view.name, "view name")?;
            self.partition(&view.items, mask, view.name.span(), "union view")?;
        }
        Ok(())
    }
}

/// Records a name or returns the matching duplicate-name diagnostic.
fn ensure_unique_name(names: &mut HashSet<String>, name: &Ident, kind: &str) -> Result<()> {
    names
        .insert(name.to_string())
        .then_some(())
        .ok_or_else(|| Error::new(name.span(), format!("bitregs: duplicate {kind} `{name}`")))
}

/// Returns an all-ones mask of the requested width.
fn low_mask(width: u32) -> u128 {
    1_u128.checked_shl(width).unwrap_or(0).wrapping_sub(1)
}

/// Flattens all view fields because their descriptors share the register type.
fn collect_fields<'a>(items: &'a [Item], fields: &mut Vec<&'a FieldDef>) {
    for item in items {
        match item {
            Item::Field(field) => fields.push(field),
            Item::Reserved(_) => {}
            Item::Union(union) => {
                for view in &union.views {
                    collect_fields(&view.items, fields);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;

    #[test]
    fn accepts_nested_union() {
        let result = expand(quote! {
            [::typestate]
            pub(super) struct Register: u16 {
                union bytes@[7:0] {
                    view split { pub low@[3:0], pub high@[7:4], }
                    view nested {
                        union nibble@[3:0] {
                            view bits { pub bit0@[0:0], reserved@[3:1] [res0], }
                            view raw { pub raw@[3:0], }
                        }
                        pub upper@[7:4],
                    }
                }
                reserved@[15:8] [ignore],
            }
        });
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn rejects_invalid_layouts() {
        let cases = [
            (
                quote!([::typestate] struct R: u16 { pub a@[8:0], pub b@[15:8], }),
                "overlaps",
            ),
            (
                quote!([::typestate] struct R: u16 { pub a@[7:0], pub a@[15:8], }),
                "duplicate field name",
            ),
            (
                quote!([::typestate] struct R: u16 { reserved@[14:0] [ignore], }),
                "does not cover",
            ),
            (
                quote!([::typestate] struct R: u16 { reserved@[15:0] [reserved], }),
                "reserved policy",
            ),
            (
                quote!([::typestate] struct R: u8 { reserved@[4294967296:0], }),
                "register type must be u16, u32, or u64",
            ),
            (
                quote!([::typestate] struct R: u16 { union value@[15:0] {} }),
                "at least one view",
            ),
            (
                quote!([::typestate] struct R: u16 {
                    pub value@[1:0] as Value { TooLarge = 4 },
                    reserved@[15:2] [ignore],
                }),
                "does not fit",
            ),
        ];

        for (input, expected) in cases {
            let error = expand(input).expect_err("invalid layout must fail");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }
}
