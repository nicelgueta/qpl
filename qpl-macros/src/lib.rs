//! `#[qpl::native(read|iread|write)]`: expose a Rust function to qpl. Use it
//! through the `qpl` crate (`qpl::native`), whose `ext` module documents the
//! API.

use proc_macro::TokenStream;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, FnArg, ItemFn, Lit, Meta, Token, Type, parse_macro_input};

/// Mark a function as a qpl extension function and declare its permission:
/// `read` (reads session data only), `iread` (reads outside the session, or
/// changes the session; refused over a read-only IPC handle), or `write`
/// (changes state outside the session; refused in a read-only session).
/// `name = "..."` renames it in qpl.
///
/// Generates a type of the same name implementing `qpl::ext::Native`, which
/// `Extension::with::<name>()` registers. The function itself is unchanged.
#[proc_macro_attribute]
pub fn native(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let func = parse_macro_input!(item as ItemFn);
    expand(args, func)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

const USAGE: &str = "declare a permission: `#[qpl::native(read)]`, `#[qpl::native(iread)]` or `#[qpl::native(write)]`";

fn expand(
    args: Punctuated<Meta, Token![,]>,
    func: ItemFn,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut effect = None;
    let mut name = None;
    for meta in &args {
        match meta {
            Meta::Path(p) if p.is_ident("read") || p.is_ident("iread") || p.is_ident("write") => {
                if effect.is_some() {
                    return Err(syn::Error::new(
                        p.span(),
                        "declare exactly one of `read`, `iread` or `write`",
                    ));
                }
                effect = Some(if p.is_ident("read") {
                    quote!(Read)
                } else if p.is_ident("iread") {
                    quote!(IRead)
                } else {
                    quote!(Write)
                });
            }
            Meta::NameValue(nv) if nv.path.is_ident("name") => {
                let Expr::Lit(lit) = &nv.value else {
                    return Err(syn::Error::new(nv.value.span(), "`name` must be a string"));
                };
                let Lit::Str(s) = &lit.lit else {
                    return Err(syn::Error::new(nv.value.span(), "`name` must be a string"));
                };
                if !is_ident(&s.value()) {
                    return Err(syn::Error::new(
                        s.span(),
                        "`name` must be a plain identifier (letters, digits, '_')",
                    ));
                }
                name = Some(s.value());
            }
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    format!("unknown argument; {USAGE}, optionally with `name = \"...\"`"),
                ));
            }
        }
    }
    let Some(effect) = effect else {
        return Err(syn::Error::new(func.sig.ident.span(), USAGE));
    };

    let sig = &func.sig;
    if let Some(a) = &sig.asyncness {
        return Err(syn::Error::new(
            a.span(),
            "a qpl native function can't be `async`",
        ));
    }
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            sig.generics.span(),
            "a qpl native function can't be generic",
        ));
    }
    if let Some(v) = &sig.variadic {
        return Err(syn::Error::new(
            v.span(),
            "a qpl native function can't be variadic",
        ));
    }

    let mut bindings = Vec::new();
    let mut types = Vec::new();
    for input in &sig.inputs {
        let FnArg::Typed(pt) = input else {
            return Err(syn::Error::new(
                input.span(),
                "a qpl native function can't take `self`",
            ));
        };
        if let Type::Reference(r) = &*pt.ty {
            return Err(syn::Error::new(
                r.span(),
                "qpl passes arguments by value: take an owned type (`String`, not `&str`)",
            ));
        }
        bindings.push(syn::Ident::new(
            &format!("__arg{}", bindings.len()),
            pt.span(),
        ));
        types.push(&pt.ty);
    }
    let indices = 0..bindings.len();
    let arity = bindings.len();

    let ident = &sig.ident;
    let vis = &func.vis;
    let name = name.unwrap_or_else(|| ident.to_string().trim_start_matches("r#").to_owned());

    Ok(quote! {
        #func

        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        #vis struct #ident {}

        impl ::qpl::ext::Native for #ident {
            const NAME: &'static str = #name;
            const EFFECT: ::qpl::ext::Effect = ::qpl::ext::Effect::#effect;
            const ARITY: usize = #arity;
            fn call(
                args: ::std::vec::Vec<::qpl::ast::Value>,
            ) -> ::std::result::Result<::std::option::Option<::qpl::ast::Value>, ::std::string::String> {
                let mut args = args.into_iter();
                #( let #bindings: #types = ::qpl::ext::arg(&mut args, #indices)?; )*
                ::qpl::ext::IntoReturn::into_return(#ident(#(#bindings),*))
            }
        }
    })
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_err(attr: &str, item: &str) -> String {
        let args =
            syn::parse::Parser::parse_str(Punctuated::<Meta, Token![,]>::parse_terminated, attr)
                .expect("attr parses");
        let func: ItemFn = syn::parse_str(item).expect("item parses");
        match expand(args, func) {
            Ok(_) => panic!("expected `#[native({attr})] {item}` to be rejected"),
            Err(e) => e.to_string(),
        }
    }

    fn expands(attr: &str, item: &str) -> String {
        let args =
            syn::parse::Parser::parse_str(Punctuated::<Meta, Token![,]>::parse_terminated, attr)
                .expect("attr parses");
        let func: ItemFn = syn::parse_str(item).expect("item parses");
        expand(args, func).expect("expands").to_string()
    }

    #[test]
    fn a_permission_is_required() {
        assert_eq!(expand_err("", "fn f() {}"), USAGE);
        assert_eq!(expand_err("name = \"g\"", "fn f() {}"), USAGE);
    }

    #[test]
    fn exactly_one_permission() {
        assert!(expand_err("read, write", "fn f() {}").contains("exactly one"));
        assert!(expand_err("read, read", "fn f() {}").contains("exactly one"));
        assert!(expand_err("read, iread", "fn f() {}").contains("exactly one"));
    }

    #[test]
    fn unknown_arguments_and_bad_names_are_rejected() {
        assert!(expand_err("admin", "fn f() {}").contains("unknown argument"));
        assert!(expand_err("read, name = 1", "fn f() {}").contains("must be a string"));
        assert!(expand_err("read, name = \"a.b\"", "fn f() {}").contains("plain identifier"));
    }

    #[test]
    fn unsupported_signatures_are_rejected() {
        assert!(expand_err("read", "fn f(s: &str) {}").contains("owned type"));
        assert!(expand_err("read", "async fn f() {}").contains("async"));
        assert!(expand_err("read", "fn f<T>(t: T) {}").contains("generic"));
        assert!(expand_err("read", "fn f<'a>() {}").contains("generic"));
    }

    #[test]
    fn declares_the_permission_arity_and_name() {
        let out = expands("write, name = \"save\"", "fn f(a: i64, b: String) {}");
        assert!(out.contains("Effect :: Write"), "{out}");
        assert!(out.contains("const ARITY : usize = 2usize"), "{out}");
        assert!(out.contains("\"save\""), "{out}");

        let out = expands("read", "pub fn r#type() -> i64 { 1 }");
        assert!(out.contains("Effect :: Read"), "{out}");
        assert!(out.contains("\"type\""), "{out}");

        let out = expands("iread", "fn f() {}");
        assert!(out.contains("Effect :: IRead"), "{out}");
    }
}
