//! Proc-macros for declaring a Shiku service's platform needs.
//!
//! Each macro call:
//!   1. Inserts a `Need` entry into a `linkme` distributed slice — the
//!      compiler aggregates these across the binary so the runtime can
//!      enumerate them via `--shiku-manifest`.
//!   2. Expands to the corresponding runtime lookup call, returning the
//!      resolved value at runtime.
//!
//! The arguments must be string literals — we extract them at compile time
//! to populate the manifest. Dynamic names are intentionally not supported;
//! a service that wants a secret called `bot_token` should say so visibly,
//! not behind a `format!`. (The compile error for non-literals is also a
//! design choice: it forces the source to be the manifest.)
//!
//! ## Examples
//!
//! ```ignore
//! let bot_token = shiku::secret!("BOT_TOKEN")?;
//! let narrator  = shiku::binding!("narrator")?;
//! let addr      = shiku::listen_http!()?;
//! ```

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, LitStr};

/// Declare a secret this service needs and read its value at runtime.
///
/// `let token = shiku::secret!("BOT_TOKEN")?;`
///
/// Compiles to a `linkme` registration of `Need::Secret("BOT_TOKEN")` plus
/// a call to `shiku_runtime::secrets::require("BOT_TOKEN")`.
#[proc_macro]
pub fn secret(input: TokenStream) -> TokenStream {
    let name = parse_macro_input!(input as LitStr);
    let lit = name.value();
    // Generate a unique static identifier per call site so multiple
    // `secret!()` calls in the same module don't collide.
    let id = format_ident!("__SHIKU_NEED_SECRET_{}", sanitise(&lit));
    let expanded = quote! {
        {
            #[::shiku_runtime::__macro_support::linkme::distributed_slice(::shiku_runtime::__macro_support::SHIKU_NEEDS)]
            #[linkme(crate = ::shiku_runtime::__macro_support::linkme)]
            static #id: ::shiku_runtime::Need = ::shiku_runtime::Need::Secret(#name);
            ::shiku_runtime::secrets::require(#name)
        }
    };
    expanded.into()
}

/// Declare a binding this service depends on and resolve it at runtime.
///
/// `let narrator = shiku::binding!("narrator")?;`
#[proc_macro]
pub fn binding(input: TokenStream) -> TokenStream {
    let name = parse_macro_input!(input as LitStr);
    let lit = name.value();
    let id = format_ident!("__SHIKU_NEED_BINDING_{}", sanitise(&lit));
    let expanded = quote! {
        {
            #[::shiku_runtime::__macro_support::linkme::distributed_slice(::shiku_runtime::__macro_support::SHIKU_NEEDS)]
            #[linkme(crate = ::shiku_runtime::__macro_support::linkme)]
            static #id: ::shiku_runtime::Need = ::shiku_runtime::Need::Binding(#name);
            ::shiku_runtime::bindings::require(#name)
        }
    };
    expanded.into()
}

/// Declare that this service needs an HTTP listen port and resolve it at
/// runtime. The agent allocates the port at registration; this macro reads
/// `SHIKU_LISTEN_HTTP` and returns a `SocketAddr`.
///
/// `let addr = shiku::listen_http!()?;`
#[proc_macro]
pub fn listen_http(input: TokenStream) -> TokenStream {
    if !input.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "listen_http! takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let expanded = quote! {
        {
            #[::shiku_runtime::__macro_support::linkme::distributed_slice(::shiku_runtime::__macro_support::SHIKU_NEEDS)]
            #[linkme(crate = ::shiku_runtime::__macro_support::linkme)]
            static __SHIKU_NEED_LISTEN_HTTP: ::shiku_runtime::Need = ::shiku_runtime::Need::ListenHttp;
            ::shiku_runtime::listen::require_http()
        }
    };
    expanded.into()
}

/// Sanitise an arbitrary string literal into a valid Rust identifier
/// fragment (used to disambiguate per-call-site statics).
fn sanitise(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
