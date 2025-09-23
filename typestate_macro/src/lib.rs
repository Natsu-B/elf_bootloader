use proc_macro::TokenStream;
use quote::quote;
use syn::DeriveInput;
use syn::Error;
use syn::Result;
use syn::parse_macro_input;
use syn::spanned::Spanned;
use syn::{self};

/// Ensures the input is a `#[repr(transparent)]` single-field tuple struct.
/// Returns the inner field type on success; on failure returns `syn::Error`
/// with a message that includes the derive name.
fn check_transparent_single_tuple_struct(ast: &DeriveInput, derive: &str) -> Result<syn::Type> {
    let ident = &ast.ident;

    // Verify presence of #[repr(transparent)]
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
            "Struct must be #[repr(transparent)] to #[derive(RawReg/BytePod)]",
        ));
    }

    // Verify it is a single-field tuple struct and return the inner type.
    match &ast.data {
        syn::Data::Struct(s) => match &s.fields {
            syn::Fields::Unnamed(t) if t.unnamed.len() == 1 => {
                let ty = t.unnamed.first().unwrap().ty.clone();
                Ok(ty)
            }
            _ => Err(Error::new(
                s.fields.span(),
                match derive {
                    "RawReg" => "Struct must be a single-field tuple struct to #[derive(RawReg)]",
                    "BytePod" => "Struct must be a single-field tuple struct to #[derive(BytePod)]",
                    _ => "Struct must be a single-field tuple struct",
                },
            )),
        },
        _ => Err(Error::new(
            ast.span(),
            "Only tuple structs are supported by this derive",
        )),
    }
}

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

#[proc_macro_derive(RawReg)]
pub fn derive_rawreg(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    let ident = &ast.ident;

    let raw_ty = match check_transparent_single_tuple_struct(&ast, "RawReg") {
        Ok(ty) => ty,
        Err(e) => return e.to_compile_error().into(),
    };

    let expanded = quote! {
        // Size/align equality with inner raw type
        const _: [(); ::core::mem::size_of::<#ident>()] =
            [(); ::core::mem::size_of::<#raw_ty>()];
        const _: [(); ::core::mem::align_of::<#ident>()] =
            [(); ::core::mem::align_of::<#raw_ty>()];

        // POD marker for the wrapper itself
        unsafe impl ::typestate::BytePod for #ident {}

        // RawReg implementation, delegating to inner raw type
        unsafe impl ::typestate::RawReg for #ident
        where
            #raw_ty: Copy + ::typestate::RawReg,
        {
            type Raw = #raw_ty;
            #[inline] fn to_raw(self) -> Self::Raw { self.0 }
            #[inline] fn from_raw(raw: Self::Raw) -> Self { Self(raw) }
            #[inline] fn to_le(self) -> Self { Self(::typestate::RawReg::to_le(self.0)) }
            #[inline] fn from_le(self) -> Self { Self(::typestate::RawReg::from_le(self.0)) }
            #[inline] fn to_be(self) -> Self { Self(::typestate::RawReg::to_be(self.0)) }
            #[inline] fn from_be(self) -> Self { Self(::typestate::RawReg::from_be(self.0)) }
        }

        // Bitwise ops
        impl ::core::ops::BitOr for #ident
        where #raw_ty: ::core::ops::BitOr<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
        }
        impl ::core::ops::BitAnd for #ident
        where #raw_ty: ::core::ops::BitAnd<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn bitand(self, rhs: Self) -> Self { Self(self.0 & rhs.0) }
        }
        impl ::core::ops::BitXor for #ident
        where #raw_ty: ::core::ops::BitXor<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn bitxor(self, rhs: Self) -> Self { Self(self.0 ^ rhs.0) }
        }
        impl ::core::ops::Not for #ident
        where #raw_ty: ::core::ops::Not<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn not(self) -> Self { Self(!self.0) }
        }

        // Arithmetic ops
        impl ::core::ops::Add for #ident
        where #raw_ty: ::core::ops::Add<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn add(self, rhs: Self) -> Self { Self(self.0 + rhs.0) }
        }
        impl ::core::ops::Sub for #ident
        where #raw_ty: ::core::ops::Sub<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn sub(self, rhs: Self) -> Self { Self(self.0 - rhs.0) }
        }
        impl ::core::ops::Mul for #ident
        where #raw_ty: ::core::ops::Mul<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn mul(self, rhs: Self) -> Self { Self(self.0 * rhs.0) }
        }
        impl ::core::ops::Div for #ident
        where #raw_ty: ::core::ops::Div<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn div(self, rhs: Self) -> Self { Self(self.0 / rhs.0) }
        }
        impl ::core::ops::Rem for #ident
        where #raw_ty: ::core::ops::Rem<Output = #raw_ty> + Copy {
            type Output = Self;
            #[inline] fn rem(self, rhs: Self) -> Self { Self(self.0 % rhs.0) }
        }

        // Assign variants
        impl ::core::ops::BitOrAssign for #ident
        where #raw_ty: ::core::ops::BitOrAssign + Copy {
            #[inline] fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
        }
        impl ::core::ops::BitAndAssign for #ident
        where #raw_ty: ::core::ops::BitAndAssign + Copy {
            #[inline] fn bitand_assign(&mut self, rhs: Self) { self.0 &= rhs.0; }
        }
        impl ::core::ops::BitXorAssign for #ident
        where #raw_ty: ::core::ops::BitXorAssign + Copy {
            #[inline] fn bitxor_assign(&mut self, rhs: Self) { self.0 ^= rhs.0; }
        }
        impl ::core::ops::AddAssign for #ident
        where #raw_ty: ::core::ops::AddAssign + Copy {
            #[inline] fn add_assign(&mut self, rhs: Self) { self.0 += rhs.0; }
        }
        impl ::core::ops::SubAssign for #ident
        where #raw_ty: ::core::ops::SubAssign + Copy {
            #[inline] fn sub_assign(&mut self, rhs: Self) { self.0 -= rhs.0; }
        }
        impl ::core::ops::MulAssign for #ident
        where #raw_ty: ::core::ops::MulAssign + Copy {
            #[inline] fn mul_assign(&mut self, rhs: Self) { self.0 *= rhs.0; }
        }
        impl ::core::ops::DivAssign for #ident
        where #raw_ty: ::core::ops::DivAssign + Copy {
            #[inline] fn div_assign(&mut self, rhs: Self) { self.0 /= rhs.0; }
        }
        impl ::core::ops::RemAssign for #ident
        where #raw_ty: ::core::ops::RemAssign + Copy {
            #[inline] fn rem_assign(&mut self, rhs: Self) { self.0 %= rhs.0; }
        }
    };
    expanded.into()
}
