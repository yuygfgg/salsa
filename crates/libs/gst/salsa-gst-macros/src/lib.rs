//! Procedural macros shared by the Salsa GStreamer plugins.
//!
//! `plugin_elements!` keeps an element's wrapper type, base class, factory
//! name and registration code together so a plugin lists each element once.
//!
//! ```ignore
//! salsa_gst::plugin_elements! {
//!     caps = "audio/x-eac3";
//!
//!     Eac3JocParse(parse::Eac3JocParse)
//!         extends gst_base::BaseParse = "salsaeac3jocparse" as EAC3_PARSE;
//!     Eac3JocDec(decoder::Eac3JocDec)
//!         extends gst_audio::AudioDecoder = "salsaeac3jocdec" as EAC3_DEC;
//! }
//! ```
//!
//! The macro defines `pub struct` wrappers extending the given base class plus
//! `gst::Element` and `gst::Object`, the factory-name constants, `plugin_init`
//! and `register_all()`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Ident, LitStr, Token,
    parse::{Parse, ParseStream},
};

struct Element {
    name: Ident,
    subclass: syn::Type,
    base: syn::Type,
    constant: Ident,
    factory: LitStr,
}

struct PluginElements {
    caps: Option<LitStr>,
    elements: Vec<Element>,
}

impl Parse for PluginElements {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut caps = None;
        let fork = input.fork();
        if let Ok(ident) = fork.parse::<Ident>()
            && ident == "caps"
            && fork.peek(Token![=])
        {
            input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;
            caps = Some(input.parse()?);
            input.parse::<Token![;]>()?;
        }

        let mut elements = Vec::new();
        while !input.is_empty() {
            let name = input.parse()?;
            let content;
            syn::parenthesized!(content in input);
            let subclass = content.parse()?;
            if !content.is_empty() {
                return Err(content.error("expected a single subclass type"));
            }
            let extends = input.parse::<Ident>()?;
            if extends != "extends" {
                return Err(syn::Error::new(extends.span(), "expected `extends`"));
            }
            let base = input.parse()?;
            input.parse::<Token![=]>()?;
            let factory = input.parse()?;
            input.parse::<Token![as]>()?;
            let constant = input.parse()?;
            elements.push(Element {
                name,
                subclass,
                base,
                constant,
                factory,
            });

            if input.is_empty() {
                break;
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            } else if input.peek(Token![;]) {
                input.parse::<Token![;]>()?;
            } else {
                return Err(input.error("expected `,` or `;` after an element"));
            }
        }
        Ok(Self { caps, elements })
    }
}

#[proc_macro]
pub fn plugin_elements(input: TokenStream) -> TokenStream {
    let PluginElements { caps, elements } = syn::parse_macro_input!(input as PluginElements);
    let caps = caps.map(|caps| quote! { pub const CAPS: &str = #caps; });
    let wrappers = elements.iter().map(|element| {
        let name = &element.name;
        let subclass = &element.subclass;
        let base = &element.base;
        quote! {
            ::gstreamer::glib::wrapper! {
                pub struct #name(ObjectSubclass<#subclass>)
                    @extends #base, ::gstreamer::Element, ::gstreamer::Object;
            }
        }
    });
    let constants = elements.iter().map(|element| {
        let constant = &element.constant;
        let factory = &element.factory;
        quote! { pub const #constant: &str = #factory; }
    });
    let registrations = elements.iter().map(|element| {
        let name = &element.name;
        let constant = &element.constant;
        quote! {
            ::gstreamer::Element::register(
                Some(plugin),
                #constant,
                ::gstreamer::Rank::PRIMARY,
                <#name as ::gstreamer::glib::prelude::StaticType>::static_type(),
            )?;
        }
    });
    let in_process = elements.iter().map(|element| {
        let name = &element.name;
        let constant = &element.constant;
        quote! {
            let _ = ::gstreamer::Element::register(
                None,
                #constant,
                ::gstreamer::Rank::PRIMARY,
                <#name as ::gstreamer::glib::prelude::StaticType>::static_type(),
            );
        }
    });

    quote! {
        #caps
        #(#wrappers)*
        #(#constants)*

        /// Register this plugin's elements without loading the plugin binary.
        #[doc(hidden)]
        pub fn register_all() {
            #(#in_process)*
        }

        fn plugin_init(
            plugin: &::gstreamer::Plugin,
        ) -> ::std::result::Result<(), ::gstreamer::glib::BoolError> {
            #(#registrations)*
            Ok(())
        }
    }
    .into()
}
