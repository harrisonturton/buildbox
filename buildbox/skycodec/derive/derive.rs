//! `#[derive(Encode, Decode)]` for `skycodec`.
//!
//! Struct fields are written in declaration order. Enum variants are written
//! as their index, then their fields. Changing a type's fields or variants
//! changes its encoding, which is fine for values that never leave the
//! process.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{parse_macro_input, parse_quote, Data, DeriveInput, Fields, GenericParam, Generics, Index};

#[proc_macro_derive(Encode)]
pub fn derive_encode(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let generics = add_bound(input.generics.clone(), parse_quote!(::skycodec::Encode));
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();
    let body = match &input.data {
        Data::Struct(data) => encode_struct(&data.fields),
        Data::Enum(data) => {
            let arms = data.variants.iter().enumerate().map(|(index, variant)| {
                let ident = &variant.ident;
                let index = index as u32;
                let (pattern, encode) = encode_fields(&variant.fields);
                quote! { Self::#ident #pattern => { e.u32(#index); #encode } }
            });
            quote! { match self { #(#arms)* } }
        }
        Data::Union(_) => return error("Encode can't be derived for unions"),
    };
    quote! {
        impl #impl_generics ::skycodec::Encode for #name #type_generics #where_clause {
            fn encode(&self, e: &mut ::skycodec::Encoder) {
                #body
            }
        }
    }
    .into()
}

#[proc_macro_derive(Decode)]
pub fn derive_decode(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let generics = add_bound(input.generics.clone(), parse_quote!(::skycodec::Decode));
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();
    let body = match &input.data {
        Data::Struct(data) => {
            let construct = decode_fields(&data.fields);
            quote! { Ok(Self #construct) }
        }
        Data::Enum(data) => {
            let arms = data.variants.iter().enumerate().map(|(index, variant)| {
                let ident = &variant.ident;
                let index = index as u32;
                let construct = decode_fields(&variant.fields);
                quote! { #index => Ok(Self::#ident #construct), }
            });
            let type_name = name.to_string();
            quote! {
                let position = d.position();
                match d.u32()? {
                    #(#arms)*
                    value => Err(::skycodec::DecodeError::Invalid {
                        what: concat!(#type_name, " variant"),
                        value: value as u64,
                        position,
                    }),
                }
            }
        }
        Data::Union(_) => return error("Decode can't be derived for unions"),
    };
    quote! {
        impl #impl_generics ::skycodec::Decode for #name #type_generics #where_clause {
            fn decode(d: &mut ::skycodec::Decoder<'_>) -> Result<Self, ::skycodec::DecodeError> {
                #body
            }
        }
    }
    .into()
}

fn encode_struct(fields: &Fields) -> TokenStream2 {
    match fields {
        Fields::Named(named) => {
            let names = named.named.iter().map(|field| field.ident.as_ref().expect("named fields have names"));
            quote! { #(::skycodec::Encode::encode(&self.#names, e);)* }
        }
        Fields::Unnamed(unnamed) => {
            let indexes = (0..unnamed.unnamed.len()).map(Index::from);
            quote! { #(::skycodec::Encode::encode(&self.#indexes, e);)* }
        }
        Fields::Unit => quote! {},
    }
}

/// The pattern that binds a variant's fields, and the code that encodes them.
fn encode_fields(fields: &Fields) -> (TokenStream2, TokenStream2) {
    match fields {
        Fields::Named(named) => {
            let names: Vec<_> = named.named.iter().map(|field| field.ident.clone().expect("named fields have names")).collect();
            (quote! { { #(#names),* } }, quote! { #(::skycodec::Encode::encode(#names, e);)* })
        }
        Fields::Unnamed(unnamed) => {
            let names: Vec<_> = (0..unnamed.unnamed.len()).map(|i| format_ident!("field{}", i)).collect();
            (quote! { ( #(#names),* ) }, quote! { #(::skycodec::Encode::encode(#names, e);)* })
        }
        Fields::Unit => (quote! {}, quote! {}),
    }
}

fn decode_fields(fields: &Fields) -> TokenStream2 {
    match fields {
        Fields::Named(named) => {
            let names = named.named.iter().map(|field| field.ident.as_ref().expect("named fields have names"));
            quote! { { #(#names: ::skycodec::Decode::decode(d)?,)* } }
        }
        Fields::Unnamed(unnamed) => {
            let decodes = unnamed.unnamed.iter().map(|_| quote! { ::skycodec::Decode::decode(d)? });
            quote! { ( #(#decodes),* ) }
        }
        Fields::Unit => quote! {},
    }
}

fn add_bound(mut generics: Generics, bound: syn::TypeParamBound) -> Generics {
    for param in &mut generics.params {
        if let GenericParam::Type(param) = param {
            param.bounds.push(bound.clone());
        }
    }
    generics
}

fn error(message: &str) -> TokenStream {
    syn::Error::new(Span::call_site(), message).to_compile_error().into()
}
