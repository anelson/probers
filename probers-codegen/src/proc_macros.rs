//! This module is where the *implementation* of the probe-related proc macros are.  The actual
//! proc macro is in the `probers-macros` crate because proc macro crates can _only_ export proc
//! macros and nothing else.  That's an inconvenient restriction, especially since there's quite a
//! lot of overlap between the macro code and the build-time probe code generation logic.  Hence,
//! this bifurcation.
use crate::probe::ProbeSpecification;
use crate::provider::{find_probes, get_provider_name};
use crate::{ProberError, ProberResult};
use heck::{ShoutySnakeCase, SnakeCase};
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use std::borrow::BorrowMut;
use std::fmt::Display;
use syn::parse_quote;
use syn::spanned::Spanned;
use syn::{Ident, ItemTrait};

/// Uses the `syn` library's `Error` struct to report an error in the form of a `TokenStream`, so
/// that a proc macro can insert this token stream into its output and thereby report a detailed
/// error message to the user.
///
/// The span of this error corresponds to the `tokens` parameter, so the user gets the relevant
/// context for the error
pub fn report_error<T: quote::ToTokens, U: Display>(tokens: &T, message: U) -> TokenStream {
    syn::Error::new_spanned(tokens.clone(), message).to_compile_error()
}

/// Translates what looks to be an explicit call to the associated function corresponding to a
/// probe on a provider trait, into something which at runtime will most efficiently attempt to
/// access the global static instance of the probe and, if it's enabled, evaluate the args and fire
/// the probe.
///
/// It translates something like this:
///
/// ```noexecute
/// probe!(MyProvider::myprobe(1, 5, "this is a string", compute_something()));
/// ```
///
/// into:
///
/// ```noexecute
/// {
///     if let Some(probe) = MyProvider::get_myprobe_probe() {
///         if probe.is_enabled() {
///             probe.fire((1, 5, "this is a string", compute_something(),)));
///         }
///     }
/// }
/// ```
///
/// In particular, note that the probe's parameters are not evaluated unless the provider
/// initialized successfully and the probe is enabled.
pub fn probe_impl(call: syn::Expr) -> ProberResult<TokenStream> {
    match call {
        syn::Expr::Call(call) => {
            //Using this call to the probe method as the starting point, modify it so that instead
            //of calling `(probename)` we call `get_(probename)_probe`, which will have been
            //generated by the `probers` proc macro
            let mut get_probe_call = call.clone();

            if let syn::Expr::Path(ref mut func) = get_probe_call.func.borrow_mut() {
                if let Some(ref mut pair) = func.path.segments.last_mut() {
                    let mut probe_segment = pair.value_mut();
                    probe_segment.ident = Ident::new(
                        &format!("get_{}_probe", probe_segment.ident),
                        probe_segment.ident.span(),
                    );
                }

                //the `get_(probename)_probe` method will not be taking any arguments; instead
                //those arguments should be used when calling the `fire` method
                let fire_args = syn::Expr::Tuple(syn::ExprTuple {
                    attrs: Vec::new(),
                    paren_token: get_probe_call.paren_token.clone(),
                    elems: get_probe_call.args,
                });
                get_probe_call.args = syn::punctuated::Punctuated::new();

                Ok(quote_spanned! { call.span() =>
                    {
                        if let Some(__probers_probe) = #get_probe_call {
                            if __probers_probe.is_enabled() {
                                __probers_probe.fire(#fire_args);
                            }
                        }
                    }
                })
            } else {
                return Err(ProberError::new(
                    format!("Unexpected expression for function call: {:?}", call.func),
                    call.span(),
                ));
            }
        }
        _ => {
            return Err(ProberError::new(
                    "The probe! macro requires the name of a provider trait and its probe method, e.g. MyProvider::myprobe(...)",
                    call.span(),
                    ));
        }
    }
}

pub fn init_provider_impl(typ: syn::TypePath) -> ProberResult<TokenStream> {
    Ok(quote_spanned! { typ.span() =>
        #typ::__try_init_provider()
    })
}

/// Actual implementation of the macro logic, factored out of the proc macro itself so that it's
/// more testable
pub fn prober_impl(item: ItemTrait) -> ProberResult<TokenStream> {
    // Look at the methods on the trait and translate each one into a probe specification
    let probes = find_probes(&item)?;

    // Re-generate this trait as a struct with our probing implementation in it
    let probe_struct = generate_prober_struct(&item, &probes)?;

    // Generate code for a struct and some `OnceCell` statics to hold the instance of the provider
    // and individual probe wrappers
    let impl_mod = generate_impl_mod(&item, &probes);

    Ok(quote_spanned! { item.span() =>
        #probe_struct

        #impl_mod
    })
}

/// A provider is described by the user as a `trait`, with methods corresponding to probes.
/// However it's actually implemented as a `struct` with no member fields, with static methods
/// implementing the probes.  Thus, given as input the `trait`, we produce a `struct` of the same
/// name whose implementation actually performs the firing of the probes.
fn generate_prober_struct(
    item: &ItemTrait,
    probes: &Vec<ProbeSpecification>,
) -> ProberResult<TokenStream> {
    // From the probe specifications, generate the corresponding methods that will be on the probe
    // struct.
    let mut probe_methods: Vec<TokenStream> = Vec::new();
    let mod_name = get_provider_impl_mod_name(&item.ident);
    let struct_type_name = get_provider_impl_struct_type_name(&item.ident);
    let struct_type_path: syn::Path = parse_quote! { #mod_name::#struct_type_name };
    let provider_name = get_provider_name(&item);
    for probe in probes.iter() {
        probe_methods.push(probe.generate_trait_methods(
            &item.ident,
            &provider_name,
            &struct_type_path,
        )?);
    }

    // Re-generate the trait method that we took as input, with the modifications to support
    // probing
    // This includes constructing documentation for this trait, using whatever doc strings are already applied by
    // the user, plus a section of our own that has information about the provider and how it
    // translates into the various implementations.
    //
    // Hence, the rather awkward `#[doc...]` bits

    let attrs = &item.attrs;
    let span = item.span();
    let ident = &item.ident;
    let vis = &item.vis;

    let mod_name = get_provider_impl_mod_name(&item.ident);
    let struct_type_name = get_provider_impl_struct_type_name(&item.ident);
    let systemtap_comment = format!(
        "This trait corresponds to a SystemTap/USDT provider named `{}`",
        provider_name
    );

    let result = quote_spanned! { span =>
        #(#attrs)*
        #[doc = "# Probing

This trait is translated at compile-time by `probers` into a platform-specific tracing
provider, which allows very high-performance and low-overhead tracing of the probes it
fires.

The exact details of how to use interact with the probes depends on the underlying
probing implementation.

## SystemTap/USDT (Linux x64)
"]
        #[doc = #systemtap_comment]
        #[doc ="
## Other platforms

TODO: No other platforms supported yet
"]
        #vis struct #ident;

        impl #ident {
            #(#probe_methods)*

            /// **NOTE**: This function was generated by the `probers` macro
            ///
            /// Initializes the provider, if it isn't already initialized, and if initialization
            /// failed returns the error.
            ///
            /// # Usage
            ///
            /// Initializing the provider is not required.  By default, each provider will lazily
            /// initialize the first time a probe is fired.  Explicit initialization can be useful
            /// because it ensures that all of a provider's probes are registered and visible to
            /// the platform-specific tracing tools, like `bpftrace` or `tplist` on Linux.
            ///
            /// It's ok to initialize a provider more than once; init operations are idempotent and
            /// if repeated will not do anything
            ///
            /// # Caution
            ///
            /// Callers should not call this method directly.  Instead use the provided
            /// `init_provider!` macro.  This will correctly elide the call when probing is
            /// compile-time disabled.
            ///
            /// # Example
            ///
            /// ```
            /// use probers::{init_provider, prober, probe};
            ///
            /// #[prober]
            /// trait MyProbes {
            ///     fn probe0();
            /// }
            ///
            /// if let Some(err) = init_provider!(MyProbes) {
            ///     eprintln!("Probe provider failed to initialize: {}", err);
            /// }
            ///
            /// //Note that even if the provider fails to initialize, firing probes will never fail
            /// //or panic...
            ///
            /// println!("Firing anyway...");
            /// probe!(MyProbes::probe0());
            /// ```
            #[allow(dead_code)]
            #vis fn __try_init_provider() -> Option<&'static ::probers::failure::Error> {
                #mod_name::#struct_type_name::get();
                #mod_name::#struct_type_name::get_init_error()
            }

            /// **NOTE**: This function was generated by the `probers` macro
            ///
            /// If the provider has been initialized, and if that initialization failed, this
            /// method returns the error information.  If the provider was not initialized, this
            /// method does not initialize it.
            ///
            /// # Usage
            ///
            /// In general callers should prefer to use the `init_provider!` macro which wraps a
            /// call to `__try_init_provider`.  Calls to `get_init_error()` directly are necessary
            /// only when the caller specifically wants to avoid triggering initialization of the
            /// provider, but merely to test if initialization was attempted and failed previously.
            #[allow(dead_code)]
            #vis fn __get_init_error() -> Option<&'static ::probers::failure::Error> {
                #mod_name::#struct_type_name::get_init_error()
            }
        }
    };

    Ok(result)
}

/// The implementation of the probing logic is complex enough that it involves the declaration of a
/// few variables and one new struct type.  All of this is contained within a module, to avoid the
/// possibility of collissions with other code.  This method generates that module and all its
/// contents.
///
/// The contents are, briefly:
/// * The module itself, named after the trait
/// * A declaration of a `struct` which will hold references to all of the probes
/// * Multiple static `OnceCell` variables which hold the underlying provider instance as well as
/// the instance of the `struct` which holds references to all of the probes
fn generate_impl_mod(item: &ItemTrait, probes: &Vec<ProbeSpecification>) -> TokenStream {
    let mod_name = get_provider_impl_mod_name(&item.ident);
    let struct_type_name = get_provider_impl_struct_type_name(&item.ident);
    let struct_var_name = get_provider_impl_struct_var_name(&item.ident);
    let struct_type_params = get_provider_struct_type_params(probes);
    let instance_var_name = get_provider_instance_var_name(&item.ident);
    let define_provider_call = generate_define_provider_call(&item, probes);
    let provider_var_name = syn::Ident::new("p", item.span());
    let struct_members: Vec<_> = probes
        .iter()
        .map(|probe| probe.generate_struct_member_declaration())
        .collect();

    let struct_initializers: Vec<_> = probes
        .iter()
        .map(|probe| probe.generate_struct_member_initialization(&provider_var_name))
        .collect();

    quote_spanned! { item.span() =>
        mod #mod_name {
            use ::probers::failure::{bail, Fallible};
            use ::probers::{SystemTracer,SystemProvider,Provider};
            use ::probers::{ProviderBuilder,Tracer};
            use ::probers::once_cell::sync::OnceCell;

            #[allow(dead_code)]
            pub(super) struct #struct_type_name<#struct_type_params> {
                #(pub #struct_members),*
            }

            unsafe impl<#struct_type_params> Send for #struct_type_name<#struct_type_params> {}
            unsafe impl<#struct_type_params> Sync for #struct_type_name <#struct_type_params>{}

            static #instance_var_name: OnceCell<Fallible<SystemProvider>> = OnceCell::INIT;
            static #struct_var_name: OnceCell<Fallible<#struct_type_name>> = OnceCell::INIT;
            static IMPL_OPT: OnceCell<Option<&'static #struct_type_name>> = OnceCell::INIT;

            impl<#struct_type_params> #struct_type_name<#struct_type_params> {
               pub(super) fn get_init_error() -> Option<&'static failure::Error> {
                    //Don't do a whole re-init cycle again, but if the initialization has happened,
                    //check for failure
                    #struct_var_name.get().and_then(|fallible|  fallible.as_ref().err() )
               }

               #[allow(dead_code)]
               pub(super) fn get() -> Option<&'static #struct_type_name<#struct_type_params>> {
                   let imp: &'static Option<&'static #struct_type_name> = IMPL_OPT.get_or_init(|| {
                       // The reason for this seemingly-excessive nesting is that it's possible for
                       // both the creation of `SystemProvider` or the subsequent initialization of
                       // #struct_type_name to fail with different and also relevant errors.  By
                       // separting them this way we're able to preserve the details about any init
                       // failures that happen, while at runtime when firing probes it's a simple
                       // call of a method on an `Option<T>`.  I don't have any data to back this
                       // up but I suspect that allows for better optimizations, since we know an
                       // `Option<&T>` is implemented as a simple pointer where `None` is `NULL`.
                       let imp = #struct_var_name.get_or_init(|| {
                           // Initialzie the `SystemProvider`, capturing any initialization errors
                           let #provider_var_name: &Fallible<SystemProvider> = #instance_var_name.get_or_init(|| {
                                #define_provider_call
                           });

                           // Transform this #provider_var_name into an owned `Fallible` containing
                           // references to `T` or `E`, since there's not much useful you can do
                           // with just a `&Result`.
                           match #provider_var_name.as_ref() {
                               Err(e) => bail!("Provider initialization failed: {}", e),
                               Ok(#provider_var_name) => {
                                   // Proceed to create the struct containing each of the probes'
                                   // `ProviderProbe` instances
                                   Ok(
                                       #struct_type_name{
                                           #(#struct_initializers,)*
                                       }
                                   )
                               }
                           }
                       });

                       //Convert this &Fallible<..> into an Option<&T>
                       imp.as_ref().ok()
                   });

                   //Copy this `&Option<&T>` to a new `Option<&T>`.  Since that should be
                   //implemented as just a pointer, this should be effectively free
                   *imp
               }
            }
        }
    }
}

/// A `Provider` is built by calling `define_provider` on a `Tracer` implementation.
/// `define_provider` takes a closure and passes a `ProviderBuilder` parameter to that closure.
/// This method generates the call to `SystemTracer::define_provider`, and includes code to add
/// each of the probes to the provider
fn generate_define_provider_call(
    item: &ItemTrait,
    probes: &Vec<ProbeSpecification>,
) -> TokenStream {
    let builder = Ident::new("builder", item.ident.span());
    let add_probe_calls: Vec<TokenStream> = probes
        .iter()
        .map(|probe| probe.generate_add_probe_call(&builder))
        .collect();
    let provider_name = get_provider_name(item);

    quote_spanned! { item.span() =>
        // The provider name must be chosen carefully.  As of this writing (2019-04) the `bpftrace`
        // and `bcc` tools have, shall we say, "evolving" support for USDT.  As of now, with the
        // latest git version of `bpftrace`, the provider name can't have dots or colons.  For now,
        // then, the provider name is just the name of the provider trait, converted into
        // snake_case for consistency with USDT naming conventions.  If two modules in the same
        // process have the same provider name, they will conflict and some unspecified `bad
        // things` will happen.
        let provider_name = #provider_name;

        SystemTracer::define_provider(&provider_name, |mut #builder| {
            #(#add_probe_calls)*

            Ok(builder)
        })
    }
}

/// The provider struct we declare to hold the probe objects needs to take a lot of type
/// parameters.  One type, 'a, which corresponds to the lifetime parameter of the underling
/// `ProviderProbe`s, and also one lifetime parameter for every reference argument of every probe
/// method.
///
/// The return value of this is a token stream consisting of all of the types, but not including
/// the angle brackets.
fn get_provider_struct_type_params(probes: &Vec<ProbeSpecification>) -> TokenStream {
    // Make a list of all of the reference param lifetimes of all the probes
    let probe_lifetimes: Vec<syn::Lifetime> = probes
        .iter()
        .map(|p| p.args_lifetime_parameters())
        .flatten()
        .collect();

    //The struct simply takes all of these lifetimes plus 'a
    quote! {
        'a, #(#probe_lifetimes),*
    }
}

/// Returns the name of the module in which most of the implementation code for this trait will be
/// located.
fn get_provider_impl_mod_name(trait_name: &Ident) -> Ident {
    let snake_case_name = format!("{}Provider", trait_name).to_snake_case();

    Ident::new(&format!("__{}", snake_case_name), trait_name.span())
}

/// The name of the struct type within the impl module which represents the provider, eg `MyProbesProviderImpl`.
/// Note that this is not the same as the struct which we generate which has the same name as the
/// trait and implements its methods.
fn get_provider_impl_struct_type_name(trait_name: &Ident) -> Ident {
    crate::syn_helpers::add_suffix_to_ident(trait_name, "ProviderImpl")
}

/// The name of the static variable which contains the singleton instance of the provider struct,
/// eg MYPROBESPROVIDERIMPL
fn get_provider_impl_struct_var_name(trait_name: &Ident) -> Ident {
    Ident::new(
        &format!("{}ProviderImpl", trait_name).to_shouty_snake_case(),
        trait_name.span(),
    )
}

/// The name of the static variable which contains the singleton instance of the underlying tracing
/// system's `Provider` instance, eg MYPROBESPROVIDER
fn get_provider_instance_var_name(trait_name: &Ident) -> Ident {
    Ident::new(
        &format!("{}Provider", trait_name).to_shouty_snake_case(),
        trait_name.span(),
    )
}

#[cfg(test)]
mod test {}
