#![recursion_limit = "256"]

extern crate proc_macro;

mod probe_spec;
mod syn_helpers;

//We have to use the `proc_macro` types for the actual macro impl, but everywhere else we'll use
//`proc_macro2` for better testability
use heck::{ShoutySnakeCase, SnakeCase};
use probe_spec::ProbeSpecification;
use proc_macro::TokenStream as CompilerTokenStream;
use proc_macro2::{Span, TokenStream};
use proc_macro_hack::proc_macro_hack;
use quote::{quote, quote_spanned};
use std::borrow::BorrowMut;
use syn::parse_quote;
use syn::spanned::Spanned;
use syn::{parse_macro_input, Ident, ItemTrait, TraitItem};

#[proc_macro_attribute]
pub fn prober(_attr: CompilerTokenStream, item: CompilerTokenStream) -> CompilerTokenStream {
    // In our case this attribute can only be applied to a trait.  If it's not a trait, this line
    // will cause what looks to the user like a compile error complaining that it expected a trait.
    let input = parse_macro_input!(item as ItemTrait);

    match prober_impl(input) {
        Ok(stream) => stream,
        Err(err) => report_error(&err.message, err.span),
    }
    .into()
}

#[proc_macro_hack]
pub fn probe(input: CompilerTokenStream) -> CompilerTokenStream {
    let input = parse_macro_input!(input as syn::Expr);

    match probe_impl(input) {
        Ok(stream) => stream,
        Err(err) => report_error(&err.message, err.span),
    }
    .into()
}

#[derive(Debug)]
struct ProberError {
    message: String,
    span: Span,
}

impl ProberError {
    fn new<M: ToString>(message: M, span: Span) -> ProberError {
        ProberError {
            message: message.to_string(),
            span: span,
        }
    }
}

type ProberResult<T> = std::result::Result<T, ProberError>;

fn probe_impl(call: syn::Expr) -> ProberResult<TokenStream> {
    match call {
        syn::Expr::Call(mut call) => {
            //`call` is a `syn::ExprCall` struct.  It contains a `Box<Path>` where which specifies
            //the name of the function being called in the form of a (possibly fully or partially)
            //qualified path.  We will take this almost entirely as-is, but with the slight
            //adjustment that we want the name of the function being called (which is the probe to
            //fire) should have the "_impl" appended.
            if let syn::Expr::Path(ref mut func) = call.func.borrow_mut() {
                if let Some(ref mut pair) = func.path.segments.last_mut() {
                    let mut probe_segment = pair.value_mut();
                    probe_segment.ident =
                        syn_helpers::add_suffix_to_ident(&probe_segment.ident, "_impl");
                }
            } else {
                return Err(ProberError::new(
                    format!("Unexpected expression for function call: {:?}", call.func),
                    call.span(),
                ));
            }

            Ok(quote! { #call })
        }
        _ => {
            return Err(ProberError::new(
                    "The probe! macro requires the name of a provider trait and its probe method, e.g. MyProvider::myprobe(...)",
                    call.span(),
                    ));
        }
    }
}

/// Actual implementation of the macro logic, factored out of the proc macro itself so that it's
/// more testable
fn prober_impl(item: ItemTrait) -> ProberResult<TokenStream> {
    if item.generics.type_params().next() != None || item.generics.lifetimes().next() != None {
        return Err(ProberError::new(
            "Probe traits must not take any lifetime or type parameters",
            item.span(),
        ));
    }

    // Look at the methods on the trait and translate each one into a probe specification
    let probes = get_probes(&item)?;

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
    for probe in probes.iter() {
        probe_methods.push(probe.generate_trait_methods(&item.ident, &struct_type_path)?);
    }

    // Re-generate the trait method that we took as input, with the modifications to support
    // probing
    let span = item.span();
    let ident = &item.ident;
    let vis = &item.vis;

    let mod_name = get_provider_impl_mod_name(&item.ident);
    let struct_type_name = get_provider_impl_struct_type_name(&item.ident);

    let result = quote_spanned! { span =>
        #vis struct #ident;

        impl #ident {
            #(#probe_methods)*

            #[allow(dead_code)]
            fn get_init_error() -> Option<&'static ::failure::Error> {
                #mod_name::#struct_type_name::get_init_error()
            }
        }
    };

    Ok(result)
}

/// Looking at the methods defined on the trait, deduce from those methods the probes that we will
/// need to define, including their arg counts and arg types.
///
/// If the trait contains anything other than method declarations, or any of the declarations are
/// not suitable as probes, an error is returned
fn get_probes(item: &ItemTrait) -> ProberResult<Vec<ProbeSpecification>> {
    let mut specs: Vec<ProbeSpecification> = Vec::new();
    for f in item.items.iter() {
        match f {
            TraitItem::Method(ref m) => {
                specs.push(ProbeSpecification::from_method(m)?);
            }
            _ => {
                return Err(ProberError::new(
                    "Probe traits must consist entirely of methods, no other contents",
                    f.span(),
                ));
            }
        }
    }

    Ok(specs)
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
            use ::failure::{bail, Fallible};
            use ::probers::{SystemTracer,SystemProvider,SystemProbe,ProviderProbe,Provider};
            use ::probers_core::{ProviderBuilder,Tracer};
            use ::once_cell::sync::OnceCell;

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

               pub(super) fn get_init_error() -> Option<&'static failure::Error> {
                    //Don't do a whole re-init cycle again, but if the initialization has happened,
                    //check for failure
                    #struct_var_name.get().and_then(|fallible|  fallible.as_ref().err() )
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

    quote_spanned! { item.span() =>
        SystemTracer::define_provider(module_path!(), |mut #builder| {
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
        .map(|p| {
            p.get_args_with_separate_lifetimes()
                .into_iter()
                .map(|(_, _, lifetimes)| lifetimes)
                .flatten()
                .collect::<Vec<syn::Lifetime>>()
        })
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
    Ident::new(
        &format!("{}Provider", trait_name).to_snake_case(),
        trait_name.span(),
    )
}

/// The name of the struct type within the impl module which represents the provider, eg `MyProbesProviderImpl`.
/// Note that this is not the same as the struct which we generate which has the same name as the
/// trait and implements its methods.
fn get_provider_impl_struct_type_name(trait_name: &Ident) -> Ident {
    syn_helpers::add_suffix_to_ident(trait_name, "ProviderImpl")
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

/// Reports a compile error in our macro, which is then reported to the user via the
/// `compile_error!` macro injected into the token stream.  Cool idea stolen from
/// https://internals.rust-lang.org/t/custom-error-diagnostics-with-procedural-macros-on-almost-stable-rust/8113
fn report_error(msg: &str, span: Span) -> TokenStream {
    //NB: When the unstable feature `proc_macro_diagnostic` is stabilized, use that instead of this
    //hack
    //
    //span.unwrap().error(msg).emit();
    //TokenStream::new()
    quote_spanned! { span =>
        compile_error! { #msg }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use quote::quote;
    use syn::{parse_quote, ItemTrait};

    mod data {
        use super::*;

        pub(crate) fn simple_valid() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: i32);
                    fn probe1(arg0: &str);
                    fn probe2(arg0: &str, arg1: usize);
                }
            }
        }

        pub(crate) fn valid_with_many_refs() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: i32);
                    fn probe1(arg0: &str);
                    fn probe2(arg0: &str, arg1: usize);
                    fn probe3(arg0: &str, arg1: &usize, arg2: &Option<i32>);
                }
            }
        }

        pub(crate) fn has_trait_type_param() -> ItemTrait {
            parse_quote! {
                trait TestTrait<T: Debug> {
                    fn probe0(arg0: i32);
                    fn probe1(arg0: &str);
                    fn probe2(arg0: &str, arg1: usize);
                }
            }
        }

        pub(crate) fn has_const() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: i32);
                    const FOO: usize = 5;
                }
            }
        }

        pub(crate) fn has_type() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: i32);
                    type Foo = Debug;
                }
            }
        }

        pub(crate) fn has_macro_invocation() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    println!("WTF");

                    fn probe0(arg0: i32);
                }
            }
        }

        pub(crate) fn has_const_fn() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    const fn probe0(arg0: i32);
                }
            }
        }

        pub(crate) fn has_unsafe_fn() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    unsafe fn probe0(arg0: i32);
                }
            }
        }

        pub(crate) fn has_extern_fn() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    extern "C" fn probe0(arg0: i32);
                }
            }
        }

        pub(crate) fn has_fn_type_param() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0<T: Debug>(arg0: T);
                }
            }
        }

        pub(crate) fn has_explicit_unit_retval() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: usize) -> ();
                }
            }
        }

        pub(crate) fn has_non_unit_retval() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: usize) -> bool;
                }
            }
        }
        pub(crate) fn has_default_impl() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(arg0: i32) { prinln!("{}", arg0); }
                }
            }
        }

        pub(crate) fn has_non_static_method() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(&self, arg0: i32);
                }
            }
        }

        pub(crate) fn has_mut_self_method() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(&mut self, arg0: i32);
                }
            }
        }

        pub(crate) fn has_self_by_val_method() -> ItemTrait {
            parse_quote! {
                trait TestTrait {
                    fn probe0(self, arg0: i32);
                }
            }
        }

    }

    #[test]
    fn works_with_valid_cases() {
        assert_eq!(true, prober_impl(data::simple_valid()).is_ok());
        assert_eq!(true, prober_impl(data::valid_with_many_refs()).is_ok());
    }

    #[test]
    fn trait_type_params_not_allowed() {
        // We need to be able to programmatically generate an impl of the probe trait which means
        // it cannot take any type parameters which we would not know how to provide
        assert_eq!(true, prober_impl(data::has_trait_type_param()).is_err());
    }

    #[test]
    fn non_method_items_not_allowed() {
        // A probe trait can't have anything other than methods.  That means no types, consts, etc
        assert_eq!(true, prober_impl(data::has_const()).is_err());
        assert_eq!(true, prober_impl(data::has_type()).is_err());
        assert_eq!(true, prober_impl(data::has_macro_invocation()).is_err());
    }

    #[test]
    fn method_modifiers_not_allowed() {
        // None of the Rust method modifiers like const, unsafe, async, or extern are allowed
        assert_eq!(true, prober_impl(data::has_const_fn()).is_err());
        assert_eq!(true, prober_impl(data::has_unsafe_fn()).is_err());
        assert_eq!(true, prober_impl(data::has_extern_fn()).is_err());
    }

    #[test]
    fn generic_methods_not_allowed() {
        // Probe methods must not be generic
        assert_eq!(true, prober_impl(data::has_fn_type_param()).is_err());
    }

    #[test]
    fn method_retvals_not_allowed() {
        // Probe methods never return a value.  I would like to be able to support methods that
        // explicitly return `()`, but it wasn't immediately obvious how to do that with `syn` and
        // it's more convenient to declare probe methods without any return type anyway
        assert_eq!(true, prober_impl(data::has_explicit_unit_retval()).is_err());
        assert_eq!(true, prober_impl(data::has_non_unit_retval()).is_err());
    }

    #[test]
    fn methods_must_not_take_self() {
        // Probe methods should not take a `self` parameter
        assert_eq!(true, prober_impl(data::has_non_static_method()).is_err());
        assert_eq!(true, prober_impl(data::has_mut_self_method()).is_err());
        assert_eq!(true, prober_impl(data::has_self_by_val_method()).is_err());
    }

    #[test]
    fn methods_must_not_have_default_impl() {
        // The whole point of this macro is to generate implementations of the probe methods so ti
        // doesn't make sense for the caller to provide their own
        assert_eq!(true, prober_impl(data::has_default_impl()).is_err());
    }
}