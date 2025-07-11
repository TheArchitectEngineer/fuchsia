// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::punctuated::Punctuated;

/// Apply to a struct to decompose any `SplitStorage` enum fields.
///
/// Each field must be have the `#[split_enum_storage::decomposed]` attribute to have its
/// discriminant stored out-of-line in the struct. This macro generates:
///
/// 1. The original type with any `decomposed` fields replaced with separate discriminants and
///    payloads.
/// 1. Getter and setter methods for any `decomposed` fields. Getters have the same name as the
///    field, setters are prefixed with `set_`.
/// 1. Several common trait implementations that are commonly derived: Clone, Debug, Eq, PartialEq.
/// 1. A `${STRUCT_NAME}Unsplit` type with the original enum definition for construction. Convert it
///    into the smaller container from (1) using the `decompose()` method.
///
/// # Safety
///
/// Any field with the `#[split_enum_storage::decomposed]` attribute will have its definition
/// replaced by two private fields: one for the discriminant and one for the payload. These fields
/// are never made public, but code from the same module sub-tree as the type will be able to
/// access them.
///
/// [Until unsafe fields are available][unsafe-fields], users of this macro should ensure that:
///
/// 1. it is only used on types in "leaf" modules to keep the scope of code reviews small
/// 1. only code generated by this macro interacts with the discriminant and payload fields
///
/// [unsafe-fields]: https://fxbug.dev/417769377
#[proc_macro_attribute]
pub fn container(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let arg_parser = syn::meta::parser(|meta| Err(meta.error("unrecognized argument")));
    syn::parse_macro_input!(args with arg_parser);

    match generate_container(syn::parse_macro_input!(input as syn::ItemStruct)) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn generate_container(input: syn::ItemStruct) -> syn::Result<TokenStream> {
    let syn::ItemStruct {
        attrs,
        vis,
        struct_token,
        ident: struct_name,
        generics,
        fields: orig_fields,
        semi_token: _,
    } = &input;

    if generics.lt_token.is_some()
        || !generics.params.is_empty()
        || generics.gt_token.is_some()
        || generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &generics,
            "Structs with generics can't be containers for split enums.",
        ));
    }

    let syn::Fields::Named(named_fields) = &orig_fields else {
        return Err(syn::Error::new_spanned(
            &input,
            "only structs with named fields are supported",
        ));
    };

    let debug_name = struct_name.to_string();
    let unsplit_ident = quote::format_ident!("{}Unsplit", struct_name);

    let mut updated_fields = vec![];
    let mut accessors = vec![];

    let mut field_clones = vec![];
    let mut field_debugs = vec![];
    let mut field_partial_eqs: Punctuated<TokenStream, syn::Token![&&]> = Default::default();
    let mut field_drops = vec![];

    let mut field_decomp_destructurings = vec![];
    let mut field_decompositions = vec![];

    for field in &named_fields.named {
        let Some(ident) = &field.ident else {
            return Err(syn::Error::new_spanned(field, "fields must have identifiers"));
        };
        let debug_ident = ident.to_string();
        let contains_container_attrs = field.attrs.iter().any(is_decomposed_attr);
        if contains_container_attrs {
            let disc_field_name = format_ident!("{}_discriminant_do_not_use_manually", ident);
            let payload_field_name = format_ident!("{}_payload_do_not_use_manually", ident);
            let setter_name = format_ident!("set_{}", ident);
            let syn::Type::Path(field_ty) = &field.ty else {
                return Err(syn::Error::new_spanned(
                    &field.ty,
                    "only path fields to enums can be decomposed",
                ));
            };
            let Some(field_ty_name) = field_ty.path.segments.last() else {
                return Err(syn::Error::new_spanned(
                    &field.ty,
                    "must have at least one path segment in a struct field type",
                ));
            };

            let disc_ty = format_ident!("{}Discriminant", field_ty_name.ident);
            let payload_ty = format_ident!("{}Payload", field_ty_name.ident);

            // TODO(https://fxbug.dev/417769377) make these two fields unsafe to as guardrail to
            // prevent callers from desynchronizing them
            updated_fields.push(quote!(#disc_field_name: #disc_ty));
            updated_fields.push(quote!(#payload_field_name: std::mem::ManuallyDrop<#payload_ty>));
            accessors.push(quote! {
                pub fn #ident(&self) -> #field_ty {
                    // SAFETY: generated code keeps the discriminant and payload in sync
                    unsafe { self.#payload_field_name.read(self.#disc_field_name) }
                }
                pub fn #setter_name(&mut self, new: #field_ty) {
                    use split_enum_storage::SplitStorage;
                    let (disc, payload) = new.decompose();
                    // SAFETY: generated code keeps the discriminant and payload in sync.
                    unsafe { self.#payload_field_name.free(self.#disc_field_name) };
                    self.#disc_field_name = disc;
                    self.#payload_field_name = payload;
                }
            });
            field_clones.push(quote!(#disc_field_name: self.#disc_field_name));
            field_clones.push(quote! {
                // SAFETY: generated code keeps the discriminant and payload in sync
                #payload_field_name: unsafe {
                    self.#payload_field_name.clone(self.#disc_field_name)
                }
            });
            field_debugs.push(quote!(#debug_ident, &self.#ident()));
            field_partial_eqs.push(quote!(self.#ident() == other.#ident()));
            field_drops.push(quote! {
                // SAFETY: generated code keeps the discriminant and payload in sync
                unsafe { self.#payload_field_name.free(self.#disc_field_name); }
            });
            field_decomp_destructurings.push(quote! {
                let (#disc_field_name, #payload_field_name) = self.#ident.decompose();
            });
            field_decompositions.push(quote!(#disc_field_name));
            field_decompositions.push(quote!(#payload_field_name));
        } else {
            updated_fields.push(field.to_token_stream());
            field_clones.push(quote!(#ident: self.#ident.clone()));
            field_debugs.push(quote!(#debug_ident, &self.#ident));
            field_partial_eqs.push(quote!(self.#ident == other.#ident));
            field_decompositions.push(quote!(#ident: self.#ident));
        }
    }

    let mut cleaned_orig_fields = named_fields.clone();
    for field in &mut cleaned_orig_fields.named {
        field.attrs.retain(|a| !is_decomposed_attr(a));
    }

    let expanded = quote! {
        #(#attrs)*
        #vis #struct_token #struct_name {
            #(#updated_fields),*
        }

        impl #struct_name {
            #(#accessors)*
        }

        impl std::clone::Clone for #struct_name {
            fn clone(&self) -> Self {
                Self {
                    #(#field_clones),*
                }
            }
        }

        impl std::fmt::Debug for #struct_name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut s = f.debug_struct(#debug_name);
                #(s.field(#field_debugs);)*
                s.finish()
            }
        }

        impl std::cmp::PartialEq for #struct_name {
            fn eq(&self, other: &Self) -> bool {
                #field_partial_eqs
            }
        }

        impl std::cmp::Eq for #struct_name {}

        impl Drop for #struct_name {
            fn drop(&mut self) {
                #(#field_drops)*
            }
        }

        struct #unsplit_ident #cleaned_orig_fields

        impl #unsplit_ident {
            fn decompose(self) -> #struct_name {
                use split_enum_storage::SplitStorage;

                #(#field_decomp_destructurings),*
                #struct_name {
                    #(#field_decompositions),*
                }
            }
        }
    };

    Ok(expanded)
}

fn is_decomposed_attr(a: &syn::Attribute) -> bool {
    if let syn::Meta::Path(p) = &a.meta {
        if p.segments.len() == 2 {
            let first = p.segments.first().unwrap();
            let second = p.segments.get(1).unwrap();
            return first.ident == "split_enum_storage" && second.ident == "decomposed";
        }
    }
    false
}

/// Derive the `SplitStorage` trait for an enum so that its discriminant and payload can be stored
/// separately. Structs which store such enums can use the `#[split_enum_storage::container]` macro
/// to take advantage of the generated trait implementation.
#[proc_macro_derive(SplitStorage)]
pub fn generate_split_storage_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    match generate_split_storage_impl_inner(syn::parse_macro_input!(input as syn::DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn generate_split_storage_impl_inner(input: syn::DeriveInput) -> syn::Result<TokenStream> {
    let syn::DeriveInput { attrs: _, vis, ident, generics, data } = input;

    let syn::Data::Enum(data_enum) = data else {
        return Err(syn::Error::new_spanned(
            ident,
            "Discriminant decomposition only works on enums.",
        ));
    };

    if generics.lt_token.is_some()
        || !generics.params.is_empty()
        || generics.gt_token.is_some()
        || generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(&generics, "Generic enums can't be decomposed."));
    }

    let discriminant_name = format_ident!("{}Discriminant", ident);
    let payload_name = format_ident!("{}Payload", ident);

    let mut discriminant_variants = vec![];
    let mut payload_fields = vec![];

    let mut decompose_arms = vec![];
    let mut clone_arms = vec![];
    let mut read_arms = vec![];
    let mut free_arms = vec![];

    for variant in data_enum.variants.iter() {
        let variant_name = &variant.ident;

        discriminant_variants.push(variant_name);

        let payload_field_name = format_ident!("{}_data", variant_name.to_string().to_lowercase());

        match &variant.fields {
            syn::Fields::Unit => {
                decompose_arms.push(quote! {
                    #ident::#variant_name => (
                        #discriminant_name::#variant_name,
                        #payload_name { empty: () },
                    )
                });
                clone_arms.push(quote! {
                    #discriminant_name::#variant_name => Self { empty: () }
                });
                read_arms.push(quote! {
                    #discriminant_name::#variant_name => #ident::#variant_name
                });
                free_arms.push(quote! {
                    #discriminant_name::#variant_name => ()
                });
            }
            syn::Fields::Unnamed(fields) => {
                if fields.unnamed.len() != 1 {
                    return Err(syn::Error::new_spanned(
                        fields,
                        "SplitStorage only supports single-field tuple variants",
                    ));
                }

                let field = &fields.unnamed[0];
                if field.ident.is_some() {
                    return Err(syn::Error::new_spanned(
                        &field,
                        "decomposed variant fields can't be named",
                    ));
                }

                let field_type = &field.ty;
                payload_fields.push(quote! {
                    #payload_field_name: std::mem::ManuallyDrop<#field_type>
                });

                decompose_arms.push(quote! {
                    #ident::#variant_name(inner) => (
                        #discriminant_name::#variant_name,
                        #payload_name { #payload_field_name: std::mem::ManuallyDrop::new(inner) },
                    )
                });
                clone_arms.push(quote! {
                    #discriminant_name::#variant_name => Self {
                        #payload_field_name: self.#payload_field_name.clone(),
                    }
                });
                read_arms.push(quote! {
                    #discriminant_name::#variant_name => #ident::#variant_name(
                        std::mem::ManuallyDrop::into_inner(self.#payload_field_name.clone())
                    )
                });
                free_arms.push(quote! {
                    #discriminant_name::#variant_name =>
                        std::mem::ManuallyDrop::drop(&mut self.#payload_field_name)
                });
            }
            syn::Fields::Named(fields) => {
                return Err(syn::Error::new_spanned(
                    fields,
                    "SplitStorage not supported on struct variants.",
                ));
            }
        }
    }

    if payload_fields.is_empty() {
        return Err(syn::Error::new_spanned(
            ident,
            "No data-carrying payload fields to decompose.",
        ));
    }
    // Add a catch-all empty field for any unit variants.
    payload_fields.push(quote!(empty: ()));

    let drop_message =
        format!("{} cannot safely be dropped without a {}", payload_name, discriminant_name,);

    let expanded = quote! {
        // SAFETY: the trait is unsafe to limit implementations to this code
        unsafe impl split_enum_storage::SplitStorage for #ident {
            type Discriminant = #discriminant_name;
            type Payload = #payload_name;

            fn decompose(self) -> (#discriminant_name, std::mem::ManuallyDrop<#payload_name>) {
                let (discriminant, payload) = match self {
                    #(#decompose_arms),*
                };
                (discriminant, std::mem::ManuallyDrop::new(payload))
            }
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #vis enum #discriminant_name {
            #(#discriminant_variants),*
        }

        #vis union #payload_name {
            #(#payload_fields),*
        }

        impl #payload_name {
            /// Clone `self`'s live fields (if any) into a new payload.
            ///
            /// # Safety
            ///
            /// The provided `disc` must be equal to the type that was returned with this value's
            /// creation.
            unsafe fn clone(&self, disc: #discriminant_name) -> std::mem::ManuallyDrop<Self> {
                std::mem::ManuallyDrop::new(match disc {
                    #(#clone_arms),*
                })
            }

            /// Consume `self` to recover the original enum with an inline tag.
            ///
            /// # Safety
            ///
            /// The provided `disc` must be equal to the value that was returned with this payload's
            /// creation.
            unsafe fn read(&self, disc: #discriminant_name) -> #ident {
                match disc {
                    #(#read_arms),*
                }
            }

            /// Consume `self` to drop its data.
            ///
            /// # Safety
            ///
            /// The provided `disc` must be equal to the type that was returned with this value's
            /// creation.
            ///
            /// Invalidates `self`, safe code must not be exposed to its value after calling this
            /// method.
            unsafe fn free(&mut self, disc: #discriminant_name) {
                match disc {
                    #(#free_arms),*
                }
            }
        }

        impl Drop for #payload_name {
            fn drop(&mut self) {
                panic!(#drop_message);
            }
        }
    };

    Ok(expanded)
}
