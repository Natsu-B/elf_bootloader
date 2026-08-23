//! Procedural macros for the typestate crate.
//!
//! Provides register-layout and derive macros for the `typestate` crate.

mod bitregs;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::format_ident;
use quote::quote;
use syn::DeriveInput;
use syn::Error;
use syn::Result;
use syn::parse_macro_input;
use syn::spanned::Spanned;

/// Implements the register-layout DSL exported as `typestate::bitregs!`.
#[proc_macro]
pub fn bitregs_impl(input: TokenStream) -> TokenStream {
    bitregs::expand(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Ensures the input is a `#[repr(transparent)]` single-field tuple struct.
/// Returns the inner field type on success; on failure returns `syn::Error`
/// with a message that includes the derive name.
fn check_transparent_single_tuple_struct(ast: &DeriveInput, derive: &str) -> Result<syn::Type> {
    let ident = &ast.ident;

    let mut is_transparent = false;
    for attr in &ast.attrs {
        if attr.path().is_ident("repr") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("transparent") {
                    is_transparent = true;
                }
                Ok(())
            })?;
        }
    }
    if !is_transparent {
        return Err(Error::new(
            ident.span(),
            format!("Struct must be #[repr(transparent)] to #[derive({derive})]"),
        ));
    }

    match &ast.data {
        syn::Data::Struct(s) => match &s.fields {
            syn::Fields::Unnamed(t) if t.unnamed.len() == 1 => {
                let ty = t.unnamed.first().expect("checked len() == 1").ty.clone();
                Ok(ty)
            }
            _ => Err(Error::new(
                s.fields.span(),
                format!("Struct must be a single-field tuple struct to #[derive({derive})]"),
            )),
        },
        _ => Err(Error::new(
            ast.span(),
            format!("Only tuple structs are supported by #[derive({derive})]"),
        )),
    }
}

fn ensure_exact_primitive(inner_ty: &syn::Type, expected: &str, derive_name: &str) -> Result<()> {
    match inner_ty {
        syn::Type::Path(path) if path.qself.is_none() && path.path.is_ident(expected) => Ok(()),
        _ => Err(Error::new(
            inner_ty.span(),
            format!("Inner type must be {expected} for #[derive({derive_name})]"),
        )),
    }
}

fn parse_atomic_pod_mask(ast: &DeriveInput) -> Result<Option<syn::LitInt>> {
    let mut mask: Option<syn::LitInt> = None;

    for attr in ast
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("atomic_pod"))
    {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("mask") {
                if mask.is_some() {
                    return Err(meta.error("duplicate `mask` in #[atomic_pod(...)]"));
                }
                let lit: syn::Lit = meta.value()?.parse()?;
                match lit {
                    syn::Lit::Int(mask_lit) => {
                        mask = Some(mask_lit);
                        Ok(())
                    }
                    other => Err(Error::new(
                        other.span(),
                        "`mask` must be an integer literal",
                    )),
                }
            } else {
                Err(meta.error("unsupported key in #[atomic_pod(...)] (expected `mask`)"))
            }
        })?;
    }

    Ok(mask)
}

fn parse_repr_flags(ast: &DeriveInput) -> Result<(bool, bool, bool)> {
    let mut has_c = false;
    let mut has_transparent = false;
    let mut has_packed = false;

    for attr in ast.attrs.iter().filter(|attr| attr.path().is_ident("repr")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") {
                has_c = true;
            } else if meta.path.is_ident("transparent") {
                has_transparent = true;
            } else if meta.path.is_ident("packed") {
                has_packed = true;
            }
            Ok(())
        })?;
    }

    Ok((has_c, has_transparent, has_packed))
}

fn derive_width(
    input: TokenStream,
    derive_name: &'static str,
    raw_name: &'static str,
    width_trait_name: &'static str,
) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let ident = &ast.ident;

    let mask = match parse_atomic_pod_mask(&ast) {
        Ok(mask) => mask,
        Err(e) => return e.to_compile_error().into(),
    };

    let raw_ty: syn::Type = match syn::parse_str(raw_name) {
        Ok(ty) => ty,
        Err(e) => return e.to_compile_error().into(),
    };
    let width_trait_ident = format_ident!("{width_trait_name}");
    let width_bytes = match raw_name {
        "u8" => 1usize,
        "u16" => 2usize,
        "u32" => 4usize,
        "u64" => 8usize,
        _ => {
            return Error::new(ast.span(), "unsupported width for derive")
                .to_compile_error()
                .into();
        }
    };
    let mask_expr = if let Some(mask_lit) = mask {
        quote! {
            let mask: #raw_ty = #mask_lit;
            canon &= mask;
        }
    } else {
        quote! {}
    };

    let transparent_path = check_transparent_single_tuple_struct(&ast, derive_name);
    if let Ok(inner_ty) = transparent_path {
        if let Err(e) = ensure_exact_primitive(&inner_ty, raw_name, derive_name) {
            return e.to_compile_error().into();
        }

        let expanded = quote! {
            const _: [(); ::core::mem::size_of::<#ident>()] =
                [(); ::core::mem::size_of::<#raw_ty>()];
            const _: [(); ::core::mem::align_of::<#ident>()] =
                [(); ::core::mem::align_of::<#raw_ty>()];

            unsafe impl ::typestate::AtomicPod for #ident {
                type Raw = #raw_ty;

                #[inline]
                fn to_raw(self) -> Self::Raw {
                    <Self as ::typestate::AtomicPod>::canonicalize_raw(self.0)
                }

                #[inline]
                fn from_raw(raw: Self::Raw) -> Self {
                    Self(<Self as ::typestate::AtomicPod>::canonicalize_raw(raw))
                }

                #[inline]
                fn canonicalize_raw(raw: Self::Raw) -> Self::Raw {
                    let mut canon = raw;
                    #mask_expr
                    canon
                }
            }

            unsafe impl ::typestate::#width_trait_ident for #ident {}
        };
        return expanded.into();
    }

    let (has_c, _, has_packed) = match parse_repr_flags(&ast) {
        Ok(flags) => flags,
        Err(e) => return e.to_compile_error().into(),
    };
    if !has_c {
        return Error::new(
            ident.span(),
            format!(
                "#[derive({derive_name})] supports #[repr(transparent)] single-field tuple structs \
                 or #[repr(C)] named-field structs"
            ),
        )
        .to_compile_error()
        .into();
    }
    if has_packed {
        return Error::new(
            ident.span(),
            format!("#[derive({derive_name})] does not support repr(packed)"),
        )
        .to_compile_error()
        .into();
    }

    let fields = match &ast.data {
        syn::Data::Struct(s) => match &s.fields {
            syn::Fields::Named(named) => named.named.iter().collect::<Vec<_>>(),
            _ => {
                return Error::new(
                    s.fields.span(),
                    format!("#[derive({derive_name})] requires a named-field struct for repr(C)"),
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return Error::new(
                ast.span(),
                format!("#[derive({derive_name})] only supports structs"),
            )
            .to_compile_error()
            .into();
        }
    };

    let field_names: Vec<_> = fields
        .iter()
        .map(|f| f.ident.as_ref().expect("named field"))
        .collect();
    let field_tys: Vec<_> = fields.iter().map(|f| &f.ty).collect();

    let expanded = quote! {
        const _: () = {
            assert!(::core::mem::size_of::<#ident>() <= #width_bytes);
            #( assert!(
                ::core::mem::offset_of!(#ident, #field_names) + ::core::mem::size_of::<#field_tys>()
                    <= #width_bytes
            ); )*
        };

        impl #ident {
            const __TYPESTATE_ATOMIC_POD_CANON_MASK: #raw_ty = {
                let mut bytes = [0u8; #width_bytes];
                #( {
                    let mut i = 0usize;
                    while i < ::core::mem::size_of::<#field_tys>() {
                        let byte = ::core::mem::offset_of!(#ident, #field_names) + i;
                        if byte < #width_bytes {
                            bytes[byte] = 0xFF;
                        }
                        i += 1;
                    }
                } )*
                <#raw_ty>::from_ne_bytes(bytes)
            };
        }

        unsafe impl ::typestate::AtomicPod for #ident
        where
            #( #field_tys: ::typestate::BytePod, )*
        {
            type Raw = #raw_ty;

            #[inline]
            fn to_raw(self) -> Self::Raw {
                let mut bytes = [0u8; #width_bytes];
                #( unsafe {
                    // SAFETY: repr(C) + non-packed guarantees valid field addressability.
                    ::core::ptr::copy_nonoverlapping(
                        (&self.#field_names as *const #field_tys).cast::<u8>(),
                        bytes.as_mut_ptr().add(::core::mem::offset_of!(Self, #field_names)),
                        ::core::mem::size_of::<#field_tys>(),
                    );
                } )*
                let raw = <#raw_ty>::from_ne_bytes(bytes);
                <Self as ::typestate::AtomicPod>::canonicalize_raw(raw)
            }

            #[inline]
            fn from_raw(raw: Self::Raw) -> Self {
                let raw = <Self as ::typestate::AtomicPod>::canonicalize_raw(raw);
                let bytes = raw.to_ne_bytes();
                let mut out = ::core::mem::MaybeUninit::<Self>::zeroed();
                #( unsafe {
                    // SAFETY: repr(C) + bounds assertions ensure destination byte ranges are valid.
                    ::core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(::core::mem::offset_of!(Self, #field_names)),
                        (out.as_mut_ptr() as *mut u8).add(::core::mem::offset_of!(Self, #field_names)),
                        ::core::mem::size_of::<#field_tys>(),
                    );
                } )*
                // SAFETY: each field is written from canonicalized bytes and field types are BytePod.
                unsafe { out.assume_init() }
            }

            #[inline]
            fn canonicalize_raw(raw: Self::Raw) -> Self::Raw {
                let mut canon = raw & Self::__TYPESTATE_ATOMIC_POD_CANON_MASK;
                #mask_expr
                canon
            }
        }

        unsafe impl ::typestate::#width_trait_ident for #ident {}
    };
    expanded.into()
}

/// Derives [`typestate::BytePod`] for transparent tuple structs.
#[proc_macro_derive(BytePod)]
pub fn derive_bytepod(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let ident = &ast.ident;

    let raw_ty = match check_transparent_single_tuple_struct(&ast, "BytePod") {
        Ok(ty) => ty,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        // Size/align equality with inner raw type
        const _: [(); ::core::mem::size_of::<#ident>()] =
            [(); ::core::mem::size_of::<#raw_ty>()];
        const _: [(); ::core::mem::align_of::<#ident>()] =
            [(); ::core::mem::align_of::<#raw_ty>()];

        unsafe impl ::typestate::BytePod for #ident {}
    };
    expanded.into()
}

/// Generates the common `RawReg` implementation against a hygienic typestate path.
pub(crate) fn expand_rawreg_impl(
    ident: &syn::Ident,
    raw_ty: &TokenStream2,
    typestate_path: &TokenStream2,
) -> TokenStream2 {
    // Binary and assignment operators differ only in their trait, method, and token.
    macro_rules! binary_ops {
        ($($trait:ident::$method:ident $op:tt),+ $(,)?) => {
            quote! { $(
                impl ::core::ops::$trait for #ident
                where #raw_ty: ::core::ops::$trait<Output = #raw_ty> + Copy {
                    type Output = Self;
                    #[inline] fn $method(self, rhs: Self) -> Self { Self(self.0 $op rhs.0) }
                }
            )+ }
        };
    }
    macro_rules! assign_ops {
        ($($trait:ident::$method:ident $op:tt),+ $(,)?) => {
            quote! { $(
                impl ::core::ops::$trait for #ident
                where #raw_ty: ::core::ops::$trait + Copy {
                    #[inline] fn $method(&mut self, rhs: Self) { self.0 $op rhs.0; }
                }
            )+ }
        };
    }

    let binary_ops = binary_ops!(
        BitOr::bitor |, BitAnd::bitand &,
        BitXor::bitxor ^, Add::add +,
        Sub::sub -, Mul::mul *,
        Div::div /, Rem::rem %,
    );
    let assign_ops = assign_ops!(
        BitOrAssign::bitor_assign |=, BitAndAssign::bitand_assign &=,
        BitXorAssign::bitxor_assign ^=, AddAssign::add_assign +=,
        SubAssign::sub_assign -=, MulAssign::mul_assign *=,
        DivAssign::div_assign /=, RemAssign::rem_assign %=,
    );

    quote! {
        // Size/align equality with inner raw type
        const _: [(); ::core::mem::size_of::<#ident>()] =
            [(); ::core::mem::size_of::<#raw_ty>()];
        const _: [(); ::core::mem::align_of::<#ident>()] =
            [(); ::core::mem::align_of::<#raw_ty>()];

        // POD marker for the wrapper itself
        unsafe impl #typestate_path::BytePod for #ident {}

        // RawReg implementation, delegating to inner raw type
        unsafe impl #typestate_path::RawReg for #ident
        where
            #raw_ty: Copy + #typestate_path::RawReg,
        {
            type Raw = #raw_ty;
            #[inline] fn to_raw(self) -> Self::Raw { self.0 }
            #[inline] fn from_raw(raw: Self::Raw) -> Self { Self(raw) }
            #[inline] fn to_le(self) -> Self { Self(#typestate_path::RawReg::to_le(self.0)) }
            #[inline] fn from_le(self) -> Self { Self(#typestate_path::RawReg::from_le(self.0)) }
            #[inline] fn to_be(self) -> Self { Self(#typestate_path::RawReg::to_be(self.0)) }
            #[inline] fn from_be(self) -> Self { Self(#typestate_path::RawReg::from_be(self.0)) }
        }

        #binary_ops

        // Unary operators keep their distinct signature explicit.
        impl ::core::ops::Not for #ident
        where #raw_ty: ::core::ops::Not<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn not(self) -> Self { Self(!self.0) }
        }

        #assign_ops
    }
}

/// Derives [`typestate::RawReg`] for transparent tuple structs.
#[proc_macro_derive(RawReg)]
pub fn derive_rawreg(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let ident = &ast.ident;

    let raw_ty = match check_transparent_single_tuple_struct(&ast, "RawReg") {
        Ok(ty) => ty,
        Err(e) => return e.to_compile_error().into(),
    };
    expand_rawreg_impl(ident, &quote!(#raw_ty), &quote!(::typestate)).into()
}

/// Derives [`typestate::U8`] for 8-bit atomic-compatible types.
#[proc_macro_derive(U8, attributes(atomic_pod))]
pub fn derive_u8(input: TokenStream) -> TokenStream {
    derive_width(input, "U8", "u8", "U8")
}

/// Derives [`typestate::U16`] for 16-bit atomic-compatible types.
#[proc_macro_derive(U16, attributes(atomic_pod))]
pub fn derive_u16(input: TokenStream) -> TokenStream {
    derive_width(input, "U16", "u16", "U16")
}

/// Derives [`typestate::U32`] for 32-bit atomic-compatible types.
#[proc_macro_derive(U32, attributes(atomic_pod))]
pub fn derive_u32(input: TokenStream) -> TokenStream {
    derive_width(input, "U32", "u32", "U32")
}

/// Derives [`typestate::U64`] for 64-bit atomic-compatible types.
#[proc_macro_derive(U64, attributes(atomic_pod))]
pub fn derive_u64(input: TokenStream) -> TokenStream {
    derive_width(input, "U64", "u64", "U64")
}
