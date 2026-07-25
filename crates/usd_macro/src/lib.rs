//! The `usd!` macro (PLAN P3) — the `bsn!` analog for a USD-native scene
//! system.
//!
//! `usd!` takes a `usda` snippet as a (raw) string literal, **validates it at
//! compile time** by running openusd's text parser during macro expansion
//! (malformed USD becomes a `rustc` error pointing at the literal), and expands
//! to a `usd_bevy::snippet::UsdSnippet` holding the final text.
//!
//! `${expr}` splices a runtime Rust expression into a value position. The
//! validated skeleton substitutes a neutral placeholder for each `${…}`, so
//! interpolation sites are checked structurally, not value-wise (an exotic
//! runtime value can still produce invalid USD — see PLAN P3 risks).
//!
//! ```ignore
//! let hp = 100.0_f64;
//! let name = "Goblin";
//! let snippet = usd!(r#"#usda 1.0
//! def Xform "${name}"
//! {
//!     custom double bevy:Health:max = ${hp}
//! }
//! "#);
//! let stage = snippet.open_stage()?;
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{Expr, LitStr, parse_macro_input};

/// One piece of the parsed template: literal usda text, or an interpolated
/// Rust expression (the source between `${` and the matching `}`).
enum Part {
    Lit(String),
    Expr(String),
}

/// Split a template into literal / `${expr}` parts. Interpolation does not
/// support a `}` inside the expression (keep interpolations simple; bind
/// complex values to a `let` first).
fn split(src: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut chars = src.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '$' && matches!(chars.peek(), Some((_, '{'))) {
            chars.next(); // consume '{'
            if !lit.is_empty() {
                parts.push(Part::Lit(std::mem::take(&mut lit)));
            }
            let mut expr = String::new();
            let mut closed = false;
            for (_, ec) in chars.by_ref() {
                if ec == '}' {
                    closed = true;
                    break;
                }
                expr.push(ec);
            }
            if !closed {
                return Err("unterminated `${` interpolation".to_string());
            }
            if expr.trim().is_empty() {
                return Err("empty `${}` interpolation".to_string());
            }
            parts.push(Part::Expr(expr));
        } else {
            lit.push(c);
        }
    }
    if !lit.is_empty() {
        parts.push(Part::Lit(lit));
    }
    Ok(parts)
}

#[proc_macro]
pub fn usd(input: TokenStream) -> TokenStream {
    let lit = parse_macro_input!(input as LitStr);
    let src = lit.value();

    let parts = match split(&src) {
        Ok(p) => p,
        Err(e) => return syn::Error::new(lit.span(), e).to_compile_error().into(),
    };

    // Compile-time validation: substitute a neutral placeholder (`0`, valid in
    // most value positions and inside quotes) for each interpolation and parse.
    let validation: String = parts
        .iter()
        .map(|p| match p {
            Part::Lit(s) => s.as_str(),
            Part::Expr(_) => "0",
        })
        .collect();
    if let Err(e) = openusd::usda::parse(&validation) {
        return syn::Error::new(
            lit.span(),
            format!("invalid USD in `usd!`: {e}\n--- validated skeleton ---\n{validation}"),
        )
        .to_compile_error()
        .into();
    }

    // Runtime: a format string (usda braces doubled) with `{}` per interpolation.
    let mut fmt = String::new();
    let mut exprs: Vec<Expr> = Vec::new();
    for part in &parts {
        match part {
            Part::Lit(s) => fmt.push_str(&s.replace('{', "{{").replace('}', "}}")),
            Part::Expr(src) => match syn::parse_str::<Expr>(src) {
                Ok(e) => {
                    fmt.push_str("{}");
                    exprs.push(e);
                }
                Err(err) => {
                    return syn::Error::new(
                        lit.span(),
                        format!("invalid Rust expression in `${{{src}}}`: {err}"),
                    )
                    .to_compile_error()
                    .into();
                }
            },
        }
    }

    quote! {
        ::usd_bevy::snippet::UsdSnippet::new(format!(#fmt #(, #exprs)*))
    }
    .into()
}
