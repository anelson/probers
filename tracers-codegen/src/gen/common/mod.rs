//! This module contains some code which is shared between the different code generation
//! implementations.  It does not contain a working code generator implementation itself.
use crate::build_rs::BuildInfo;
use crate::spec::ProbeSpecification;
use crate::spec::ProviderInitSpecification;
use crate::spec::ProviderSpecification;
use crate::TracersResult;
use heck::SnakeCase;
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::spanned::Spanned;

/// Base trait for the provider generators.  Contains logic that is common to all of the
/// generators
pub(super) trait ProviderTraitGeneratorBase {
    fn spec(&self) -> &ProviderSpecification;

    fn build_info(&self) -> &BuildInfo;

    /// Generates the an additional doc comment for the generated provider trait/struct/whatever, which
    /// provides some helpful information about how to use that provider with the various tracing
    /// platforms.  This way callers can simply generate docs on their own crates and get help with
    /// tracing.
    fn generate_trait_comment(&self) -> TokenStream {
        let comment = format!(
            r###"
# Probing

This trait is translated at compile-time by `tracers` into a platform-specific tracing
provider, which allows very high-performance and low-overhead tracing of the probes it
fires.

The exact details of how to use interact with the probes depends on the underlying
probing implementation.

## SystemTap/USDT (Linux x64)

This trait corresponds to a SystemTap/USDT provider named `{provider_name}`,

## Other platforms

TODO: No other platforms supported yet
"###,
            provider_name = self.spec().name()
        );

        generate_multiline_comments(&comment)
    }

    /// Generates the declaration (but *NOT* the *implementation*) of the `__try_init_provider` method.
    /// This includes a detailed doc comment.  Each generator must implement this method, but this
    /// generates the declaration, looking something like this:
    ///
    /// ```no_execute
    /// #[allow(dead_code)]
    /// #vis fn __try_init_provider() -> Result<&'static str, &'static str>
    /// ```
    ///
    /// The actual generator should insert this token stream in the generated trait/struct, followed by
    /// an implementation contained in `{}` chars
    fn generate_try_init_decl(&self) -> TokenStream {
        let vis = &self.spec().item_trait().vis;

        quote! {
            /// **NOTE**: This function was generated by the `tracers` macro
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
            /// # Returns
            ///
            /// The return value is a `Result<&'static str, &'static str>`.  The `Ok` value is a string
            /// with some information about the provider.  The `Err` value is an error message
            /// indicating the provider failed to initialize, and why.
            ///
            /// Note that whether or not the provider initialization failed, the `probe!` macros never
            /// return an error or panic.  They will detect the initialization failed and do nothing,
            /// not even evaluate the arguments to the probe.
            ///
            /// # Caution
            ///
            /// Callers should not call this method directly.  Instead use the provided
            /// `init_provider!` macro.  This will correctly elide the call when probing is
            /// compile-time disabled.  When `tracers` is compiled with tracing disabled, this function
            /// will not be generated so any code that assumes its presence will break;
            ///
            /// # Example
            ///
            /// ```
            /// use tracers_macros::{init_provider, tracer, probe};
            ///
            /// #[tracer]
            /// trait MyProbes {
            ///     fn probe0();
            /// }
            ///
            /// if let Err(err) = init_provider!(MyProbes) {
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
            #vis fn __try_init_provider() -> ::core::result::Result<&'static str, &'static str>
        }
    }

    /// Returns the name of the module in which most of the implementation code for this trait will be
    /// located.
    fn get_provider_impl_mod_name(&self) -> syn::Ident {
        let snake_case_name = format!("{}Provider", self.spec().item_trait().ident).to_snake_case();

        syn::Ident::new(
            &format!("__{}", snake_case_name),
            self.spec().item_trait().ident.span(),
        )
    }

    /// The name of the struct type within the impl module which represents the provider, eg `MyProbesProviderImpl`.
    /// Note that this is not the same as the struct which we generate which has the same name as the
    /// trait and implements its methods.
    fn get_provider_impl_struct_type_name(&self) -> syn::Ident {
        crate::syn_helpers::add_suffix_to_ident(&self.spec().item_trait().ident, "ProviderImpl")
    }
}

/// Base trait for the provider generators.  Contains logic that is common to all of the
/// generators
pub(super) trait ProbeGeneratorBase {
    fn spec(&self) -> &ProbeSpecification;

    /// Generates the `#[deprecated...]` attribute which triggers a warning if anyone tries to call the
    /// probe method directly, not through the `probe!` attribute
    fn generate_probe_deprecation_attribute(
        &self,
        provider: &ProviderSpecification,
    ) -> TokenStream {
        let deprecation_message = format!( "Probe methods should not be called directly.  Use the `probe!` macro, e.g. `probe!({}::{}(...))`",
            provider.item_trait().ident,
            self.spec().method_name);
        let span = self.spec().method_name.span();

        quote_spanned! {span=>
            #[deprecated(note = #deprecation_message)]
        }
    }

    /// Generates a doc comment to attach to the probe's method.  This includes additional information
    /// about how to work with this probe on various platforms.
    fn generate_probe_doc_comment(&self, provider: &ProviderSpecification) -> TokenStream {
        let probe_comment = format!(r###"
# Probing

This method is translated at compile-time by `tracers` into a platform-specific tracing
probe, which allows very high-performance and low-overhead tracing.

## How to fire probe

To fire this probe, don't call this method directly. Instead, use the `probe!` macro, for example:

```ignore
// If the probe is enabled, fires the probe.  If the probe isn't enabled, or if provider
// initialization failed for some reason, does not fire the probe, and does NOT evaluate the
// arguments to the probe.
probe!({trait_name}::{probe_name}(...));
```

The exact details of how to interact with the probes depends on the underlying
probing implementation.

## SystemTap/USDT (Linux x64)

To trace the firing of this probe, use `bpftrace`, e.g.:
```text
sudo bpftrace -p ${{PID}} -e 'usdt::{provider}:{probe_name} {{ printf("Hello from {probe_name}\n"); }}'
```

where `${{PID}}` should be the actual process ID of the process you are tracing.

## Other platforms

TODO: No other platforms supported yet

"###,
        trait_name = &provider.item_trait().ident,
        probe_name = &self.spec().name,
        provider = provider.name(),
);

        generate_multiline_comments(&probe_comment)
    }
}

/// Generates the standard provider init call.  Some implementations may use a different one but
/// this is the typical impl.
pub(super) fn generate_init_provider(
    init: ProviderInitSpecification,
) -> TracersResult<TokenStream> {
    //This couldn't be simpler.  We must assume the caller provided a valid provider trait.  If
    //they didn't this will fail at compile time in a fairly obvious way.
    //
    //So we just generate code to call the init function that the provider trait generator will
    //have already generated on the trait itself.
    let provider = init.provider;
    let span = provider.span();
    Ok(quote_spanned! {span=>
        #provider::__try_init_provider()
    })
}

/// When generating a comment with `#[doc]` if the string is multi-lined then `quote_spanned` seems
/// to get confused and puts the rest of the lines of the string in a separate token tree.
/// This method simply takes a multi-line string literal, breaks it up into separate lines, and
/// generates one `#[doc...]` comment per line
fn generate_multiline_comments(comment: &str) -> TokenStream {
    let lines = comment.lines().map(|line| {
        let with_leading_space = format!(" {}", line);
        quote! {
            #[doc = #with_leading_space]
        }
    });

    quote! {
        #(#lines)*
    }
}