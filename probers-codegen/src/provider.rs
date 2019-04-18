//! This module provides functionality to scan the AST of a Rust source file and identify
//! `probe-rs` provider traits therein, as well as analyze those traits and produce `ProbeSpec`s for
//! each of the probes they contain.  Once the provider traits have been discovered, other modules
//! in this crate can then process them in various ways
use crate::probe::ProbeSpecification;
use heck::SnakeCase;
use proc_macro2::TokenStream;
use quote::quote;
use std::fmt;
use std::path::PathBuf;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{ItemTrait, TraitItem};

use crate::{ProberError, ProberResult};

pub(crate) struct ProviderSpecification {
    name: String,
    hash: crate::hashing::HashCode,
    item_trait: ItemTrait,
    token_stream: TokenStream,
    probes: Vec<ProbeSpecification>,
}

impl fmt::Debug for ProviderSpecification {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "ProviderSpecification(
    name='{}',
    probes:\n",
            self.name
        )?;

        for probe in self.probes.iter() {
            write!(f, "        {:?},\n", probe)?;
        }

        write!(f, ")")
    }
}

impl ProviderSpecification {
    pub(crate) fn from_trait(item_trait: &ItemTrait) -> ProberResult<ProviderSpecification> {
        let probes = find_probes(item_trait)?;
        let token_stream = quote! { #item_trait };
        let hash = crate::hashing::hash_token_stream(&token_stream);
        // The provider name must be chosen carefully.  As of this writing (2019-04) the `bpftrace`
        // and `bcc` tools have, shall we say, "evolving" support for USDT.  As of now, with the
        // latest git version of `bpftrace`, the provider name can't have dots or colons.  For now,
        // then, the provider name is just the name of the provider trait, converted into
        // snake_case for consistency with USDT naming conventions.  If two modules in the same
        // process have the same provider name, they will conflict and some unspecified `bad
        // things` will happen.
        Ok(ProviderSpecification {
            name: item_trait.ident.to_string().to_snake_case(),
            hash,
            item_trait: item_trait.clone(),
            token_stream,
            probes,
        })
    }

    pub(crate) fn name_with_hash(&self) -> String {
        format!("{}-{:x}", self.name, self.hash)
    }

    pub(crate) fn native_provider_source_filename(&self) -> PathBuf {
        PathBuf::from(format!("{}.cpp", self.name_with_hash()))
    }

    pub(crate) fn native_provider_lib_filename(&self) -> PathBuf {
        PathBuf::from(format!("{}.a", self.name_with_hash()))
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn item_trait(&self) -> &syn::ItemTrait {
        &self.item_trait
    }

    pub(crate) fn probes(&self) -> &Vec<ProbeSpecification> {
        &self.probes
    }
}

/// Scans the AST of a Rust source file, finding all traits marked with the `prober` attribute,
/// parses the contents of the trait, and deduces the provider spec from that.
///
/// Note that if any traits are encountered with the `prober` attribute but which are in some way
/// invalid as providers, those traits will be silently ignored.  At compile time the `prober`
/// attribute will cause a very detailed compile error so there's no chance the user will miss this
/// mistake.
pub(crate) fn find_providers(ast: &syn::File) -> Vec<ProviderSpecification> {
    //Construct an implementation of the `syn` crate's `Visit` trait which will examine all trait
    //declarations in the file looking for possible providers
    struct Visitor {
        providers: Vec<ProviderSpecification>,
    }

    impl<'ast> Visit<'ast> for Visitor {
        fn visit_item_trait(&mut self, i: &'ast ItemTrait) {
            //First pass through to the default impl
            syn::visit::visit_item_trait(self, i);

            //Check for the `prober` or `probers::prober` attribute
            if i.attrs
                .iter()
                .any(|attr| match attr.path.segments.iter().last() {
                    Some(syn::PathSegment { ident, .. }) if ident.to_string() == "prober" => true,
                    _ => false,
                })
            {
                //This looks like a provider trait
                if let Ok(provider) = ProviderSpecification::from_trait(i) {
                    self.providers.push(provider)
                }
            }
        }
    }

    let mut visitor = Visitor {
        providers: Vec::new(),
    };
    visitor.visit_file(ast);

    visitor.providers
}

/// Looking at the methods defined on the trait, deduce from those methods the probes that we will
/// need to define, including their arg counts and arg types.
///
/// If the trait contains anything other than method declarations, or any of the declarations are
/// not suitable as probes, an error is returned
fn find_probes(item: &ItemTrait) -> ProberResult<Vec<ProbeSpecification>> {
    if item.generics.type_params().next() != None || item.generics.lifetimes().next() != None {
        return Err(ProberError::new(
            "Probe traits must not take any lifetime or type parameters",
            item.span(),
        ));
    }

    // Look at the methods on the trait and translate each one into a probe specification
    let mut specs: Vec<ProbeSpecification> = Vec::new();
    for f in item.items.iter() {
        match f {
            TraitItem::Method(ref m) => {
                specs.push(ProbeSpecification::from_method(item, m)?);
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

#[cfg(test)]
mod test {
    use super::*;
    use crate::testdata::*;
    use syn::parse_quote;

    impl PartialEq<ProviderSpecification> for ProviderSpecification {
        fn eq(&self, other: &ProviderSpecification) -> bool {
            self.name == other.name && self.probes == other.probes
        }
    }

    /// Allows tests to compare a test case directly to a ProviderSpecification to ensure they match
    impl PartialEq<TestProviderTrait> for ProviderSpecification {
        fn eq(&self, other: &TestProviderTrait) -> bool {
            self.name == other.provider_name
                && other
                    .probes
                    .as_ref()
                    .map(|probes| &self.probes == probes)
                    .unwrap_or(false)
        }
    }

    fn get_filtered_test_traits(with_errors: bool) -> Vec<TestProviderTrait> {
        get_test_provider_traits(|t: &TestProviderTrait| t.expected_error.is_some() == with_errors)
    }

    #[test]
    fn find_providers_ignores_invalid_traits() {
        for test_trait in get_filtered_test_traits(true) {
            let trait_decl = test_trait.tokenstream;
            let test_file: syn::File = parse_quote! {
                #[prober]
                #trait_decl
            };

            assert_eq!(
                None,
                find_providers(&test_file).first(),
                "The invalid trait '{}' was returned by find_providers as valid",
                test_trait.description
            );
        }
    }

    #[test]
    fn find_providers_finds_valid_traits() {
        for test_trait in get_filtered_test_traits(false) {
            let trait_decl = test_trait.tokenstream.clone();
            let test_file: syn::File = parse_quote! {
                #[prober]
                #trait_decl
            };

            let mut providers = find_providers(&test_file);
            assert_ne!(
                0,
                providers.len(),
                "the test trait '{}' was not properly detected by find_provider",
                test_trait.description
            );

            assert_eq!(providers.pop().unwrap(), test_trait);
        }
    }

    #[test]
    fn find_probes_fails_with_invalid_traits() {
        for test_trait in get_filtered_test_traits(true) {
            let trait_decl = test_trait.tokenstream;
            let item_trait: syn::ItemTrait = parse_quote! {
                #[prober]
                #trait_decl
            };

            let error = find_probes(&item_trait).err();
            assert_ne!(
                None, error,
                "The invalid trait '{}' was returned by find_probes as valid",
                test_trait.description
            );

            let expected_error_substring = test_trait.expected_error.unwrap();
            let message = error.unwrap().message;
            assert!(message.contains(expected_error_substring),
                "The invalid trait '{}' should produce an error containing '{}' but instead it produced '{}'",
                test_trait.description,
                expected_error_substring,
                message
            );
        }
    }

    #[test]
    fn find_probes_succeeds_with_valid_traits() {
        for test_trait in get_filtered_test_traits(false) {
            let trait_decl = test_trait.tokenstream;
            let item_trait: syn::ItemTrait = parse_quote! {
                #[prober]
                #trait_decl
            };

            let probes = find_probes(&item_trait).unwrap();
            assert_eq!(probes, test_trait.probes.unwrap_or(Vec::new()));
        }
    }
}