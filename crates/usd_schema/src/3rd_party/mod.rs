//! Third-party glue for Omniverse-authored scenes.
//!
//! - [`convert`] — author a `*.preview.usda` override layer that
//!   replaces MDL/OmniPBR materials with `UsdPreviewSurface`
//!   fallbacks so MDL-only stages render through the
//!   pure-OpenUSD shading pipeline.

pub mod convert;
