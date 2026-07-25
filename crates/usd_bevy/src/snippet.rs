//! [`UsdSnippet`] (PLAN P3) — the runtime value the [`usd!`](macro@crate::usd)
//! macro expands to: a validated `usda` fragment that can be opened as a stage
//! and projected through the routing registry (dogfooding P1).
//!
//! In-place composition of a snippet into an already-live stage awaits an
//! openusd in-memory `Layer::from_string` (there is no public text→`Layer`
//! constructor yet); until then [`UsdSnippet::open_stage`] materializes the
//! snippet through a temp file. Tracked as a PLAN P3 / upstreaming follow-up.

use std::sync::atomic::{AtomicU64, Ordering};

use openusd::sdf;
use openusd::usd::Stage;

/// A `usda` text fragment produced by the [`usd!`](macro@crate::usd) macro (or
/// built directly). The text is already compile-time validated when it comes
/// from the macro; [`parse`](UsdSnippet::parse) re-checks an ad-hoc one.
#[derive(Debug, Clone)]
pub struct UsdSnippet {
    text: String,
}

impl UsdSnippet {
    /// Wrap raw `usda` text. The macro calls this with validated text; callers
    /// building text by hand should [`parse`](UsdSnippet::parse) to check it.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The `usda` source.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Parse the snippet into an in-memory [`sdf::Data`] (validation only; does
    /// not compose a stage).
    pub fn parse(&self) -> anyhow::Result<sdf::Data> {
        openusd::usda::parse(&self.text)
    }

    /// Open the snippet as a standalone [`Stage`]. Wrap the result in
    /// `LiveStage` to project it through the routing registry.
    ///
    /// Currently materializes through a temporary `.usda` file, because openusd
    /// has no public in-memory text→`Layer`/`Stage` constructor yet. The file
    /// is written under the OS temp dir with a process-unique name and removed
    /// after the stage is opened (the stage holds the parsed data, not the
    /// file).
    pub fn open_stage(&self) -> anyhow::Result<Stage> {
        use std::io::Write;

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let file_name = format!("usd_snippet_{}_{}.usda", std::process::id(), n);
        let path = std::env::temp_dir().join(file_name);

        {
            let mut file = std::fs::File::create(&path)?;
            file.write_all(self.text.as_bytes())?;
        }
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("temp path is not valid UTF-8"))?;
        let stage = Stage::open(path_str);
        // Best-effort cleanup; the opened stage no longer needs the file.
        let _ = std::fs::remove_file(&path);
        stage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_snippet_parses_and_opens() {
        let s = UsdSnippet::new(
            "#usda 1.0\ndef Xform \"Foo\"\n{\n    custom double x = 2\n}\n",
        );
        assert!(s.parse().is_ok(), "well-formed usda parses");
        let stage = s.open_stage().expect("opens as a stage");
        assert!(
            stage.prim(openusd::sdf::path("/Foo").unwrap()).is_valid().unwrap_or(false),
            "the prim exists on the opened stage"
        );
    }

    #[test]
    fn malformed_snippet_fails_to_parse() {
        // This is the same validator the `usd!` macro runs at compile time; a
        // broken body here is a compile error there.
        let s = UsdSnippet::new("#usda 1.0\ndef Xform \"Foo\" { this is not usda ]]");
        assert!(s.parse().is_err(), "malformed usda is rejected");
    }

    #[test]
    fn macro_escapes_braces_and_interpolates_multiple() {
        let a = 1_i32;
        let b = 2_i32;
        let s = crate::usd!(
            "#usda 1.0\n\
             def Scope \"S\"\n\
             {\n\
                 int x = ${a}\n\
                 int y = ${b}\n\
             }\n"
        );
        let t = s.text();
        assert!(t.contains("int x = 1"), "first interpolation: {t}");
        assert!(t.contains("int y = 2"), "second interpolation: {t}");
        assert!(
            t.contains('{') && t.contains('}'),
            "literal usda braces survive format escaping: {t}"
        );
        assert!(s.parse().is_ok(), "interpolated result is valid usda");
    }

    #[test]
    fn macro_static_only() {
        let s = crate::usd!("#usda 1.0\ndef Scope \"S\"\n{\n}\n");
        assert!(s.parse().is_ok());
        assert!(s.text().contains("def Scope"));
    }

    #[test]
    fn macro_string_interpolation_inside_quotes() {
        let name = "Widget";
        let s = crate::usd!("#usda 1.0\ndef Xform \"${name}\"\n{\n}\n");
        assert!(
            s.text().contains("\"Widget\""),
            "interpolation inside quotes: {}",
            s.text()
        );
        assert!(s.parse().is_ok());
    }
}
