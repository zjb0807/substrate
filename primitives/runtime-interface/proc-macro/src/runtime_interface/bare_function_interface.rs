// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Generates the bare function interface for a given trait definition.
//!
//! The bare functions are the ones that will be called by the user. On the native/host side, these
//! functions directly execute the provided implementation. On the wasm side, these
//! functions will prepare the parameters for the FFI boundary, call the external host function
//! exported into wasm and convert back the result.
//!
//! [`generate`] is the entry point for generating for each
//! trait method one bare function.
//!
//! [`function_for_method`] generates the bare
//! function per trait method. Each bare function contains both implementations. The implementations
//! are feature-gated, so that one is compiled for the native and the other for the wasm side.

use crate::utils::{
	generate_crate_access, create_exchangeable_host_function_ident, get_function_arguments,
	get_function_argument_names, get_runtime_interface, create_function_ident_with_version,
};

use syn::{
	Ident, ItemTrait, TraitItemMethod, FnArg, Signature, Result, spanned::Spanned, parse_quote,
};

use proc_macro2::{TokenStream, Span};

use quote::{quote, quote_spanned};

use std::iter;

/// Generate one bare function per trait method. The name of the bare function is equal to the name
/// of the trait method.
pub fn generate(trait_def: &ItemTrait, is_wasm_only: bool) -> Result<TokenStream> {
	let trait_name = &trait_def.ident;
	let runtime_interface = get_runtime_interface(trait_def)?;

	// latest version dispatch
	let token_stream: Result<TokenStream> = runtime_interface.latest_versions()
		.try_fold(
			TokenStream::new(),
			|mut t, (latest_version, method)| {
				t.extend(function_for_method(method, latest_version, is_wasm_only)?);
				Ok(t)
			}
		);

	// earlier versions compatibility dispatch (only std variant)
	let result: Result<TokenStream> = runtime_interface.all_versions().try_fold(token_stream?, |mut t, (version, method)|
	{
		t.extend(function_std_impl(trait_name, method, version, is_wasm_only)?);
		Ok(t)
	});

	result
}

/// Generates the bare function implementation for the given method for the host and wasm side.
fn function_for_method(
	method: &TraitItemMethod,
	latest_version: u32,
	is_wasm_only: bool,
) -> Result<TokenStream> {
	let std_impl = if !is_wasm_only {
		function_std_latest_impl(method, latest_version)?
	} else {
		quote!()
	};

	let no_std_impl = function_no_std_impl(method)?;

	Ok(
		quote! {
			#std_impl

			#no_std_impl
		}
	)
}

/// Generates the bare function implementation for `cfg(not(feature = "std"))`.
fn function_no_std_impl(method: &TraitItemMethod) -> Result<TokenStream> {
	let function_name = &method.sig.ident;
	let host_function_name = create_exchangeable_host_function_ident(&method.sig.ident);
	let args = get_function_arguments(&method.sig);
	let arg_names = get_function_argument_names(&method.sig);
	let return_value = &method.sig.output;
	let attrs = method.attrs.iter().filter(|a| !a.path.is_ident("version"));

	Ok(
		quote! {
			#[cfg(not(feature = "std"))]
			#( #attrs )*
			pub fn #function_name( #( #args, )* ) #return_value {
				// Call the host function
				#host_function_name.get()( #( #arg_names, )* )
			}
		}
	)
}

/// Generate call to latest function version for `cfg((feature = "std")`
///
/// This should generate simple `fn func(..) { func_version_<latest_version>(..) }`.
fn function_std_latest_impl(
	method: &TraitItemMethod,
	latest_version: u32,
) -> Result<TokenStream> {
	let function_name = &method.sig.ident;
	let args = get_function_arguments(&method.sig).map(FnArg::Typed);
	let arg_names = get_function_argument_names(&method.sig).collect::<Vec<_>>();
	let return_value = &method.sig.output;
	let attrs = method.attrs.iter().filter(|a| !a.path.is_ident("version"));
	let latest_function_name = create_function_ident_with_version(&method.sig.ident, latest_version);

	Ok(quote_spanned! { method.span() =>
		#[cfg(feature = "std")]
		#( #attrs )*
		pub fn #function_name( #( #args, )* ) #return_value {
			#latest_function_name(
				#( #arg_names, )*
			)
		}
	})
}

/// Generates the bare function implementation for `cfg(feature = "std")`.
fn function_std_impl(
	trait_name: &Ident,
	method: &TraitItemMethod,
	version: u32,
	is_wasm_only: bool,
) -> Result<TokenStream> {
	let function_name = create_function_ident_with_version(&method.sig.ident, version);
	let function_name_str = function_name.to_string();

	let crate_ = generate_crate_access();
	let args = get_function_arguments(&method.sig).map(FnArg::Typed).chain(
		// Add the function context as last parameter when this is a wasm only interface.
		iter::from_fn(||
			if is_wasm_only {
				Some(
					parse_quote!(
						mut __function_context__: &mut dyn #crate_::sp_wasm_interface::FunctionContext
					)
				)
			} else {
				None
			}
		).take(1),
	);
	let return_value = &method.sig.output;
	let attrs = method.attrs.iter().filter(|a| !a.path.is_ident("version"));
	// Don't make the function public accessible when this is a wasm only interface.
	let call_to_trait = generate_call_to_trait(trait_name, method, version, is_wasm_only);

	Ok(
		quote_spanned! { method.span() =>
			#[cfg(feature = "std")]
			#( #attrs )*
			fn #function_name( #( #args, )* ) #return_value {
				#crate_::sp_tracing::enter_span!(#function_name_str);
				#call_to_trait
			}
		}
	)
}

/// Generate the call to the interface trait.
fn generate_call_to_trait(
	trait_name: &Ident,
	method: &TraitItemMethod,
	version: u32,
	is_wasm_only: bool,
) -> TokenStream {
	let crate_ = generate_crate_access();
	let method_name = create_function_ident_with_version(&method.sig.ident, version);
	let expect_msg = format!(
		"`{}` called outside of an Externalities-provided environment.",
		method_name,
	);
	let arg_names = get_function_argument_names(&method.sig);

	if takes_self_argument(&method.sig) {
		let instance = if is_wasm_only {
			Ident::new("__function_context__", Span::call_site())
		} else {
			Ident::new("__externalities__", Span::call_site())
		};

		let impl_ = quote!( #trait_name::#method_name(&mut #instance, #( #arg_names, )*) );

		if is_wasm_only {
			quote_spanned! { method.span() => #impl_ }
		} else {
			quote_spanned! { method.span() =>
				#crate_::with_externalities(|mut #instance| #impl_).expect(#expect_msg)
			}
		}
	} else {
		// The name of the trait the interface trait is implemented for
		let impl_trait_name = if is_wasm_only {
			quote!( #crate_::sp_wasm_interface::FunctionContext )
		} else {
			quote!( #crate_::Externalities )
		};

		quote_spanned! { method.span() =>
			<&mut dyn #impl_trait_name as #trait_name>::#method_name(
				#( #arg_names, )*
			)
		}
	}
}

/// Returns if the given `Signature` takes a `self` argument.
fn takes_self_argument(sig: &Signature) -> bool {
	match sig.inputs.first() {
		Some(FnArg::Receiver(_)) => true,
		_ => false,
	}
}
