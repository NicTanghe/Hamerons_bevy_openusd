//! MaterialX-in-UsdShade graph compilation and Bevy WESL integration.
//!
//! The bundled registry and WESL modules come from the pinned
//! `vendor/materialx-wesl` submodule. The compiler deliberately targets a
//! renderer adapter for MaterialX Standard Surface through Bevy PBR.
//!
//! The first slice supports MaterialX and universal surface terminals,
//! Standard Surface base/base-color, metalness, specular roughness, emission,
//! opacity and normal, translated constants/arithmetic, and image sampling on
//! UV0. Unknown or invalid graphs receive structured diagnostics and a visible
//! magenta fallback. Limits are 64 uniform values and four 2D images per
//! material. External `.mtlx` references are compiled by the renderer-neutral
//! `materialx_wesl` crate. Its dielectric transmission closure is evaluated in
//! Bevy's transmissive forward pass and remains distinct from alpha opacity.
//! Subsurface and the remaining closure families, arbitrary primvars/UV sets,
//! UDIMs, displacement and volume remain deliberately unsupported.

pub mod compiler;
pub mod diagnostic;
pub mod emit;
pub mod external;
pub mod material;
pub mod registry;

/// Translator repository revision pinned by the submodule handover.
pub const TRANSLATOR_REVISION: &str = materialx_wesl::TRANSLATOR_REVISION;
