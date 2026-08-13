//! MaterialX-in-UsdShade graph compilation and Bevy WESL integration.
//!
//! The bundled registry and WESL modules come from the pinned
//! `vendor/materialx-wesl` submodule. The compiler deliberately targets a
//! documented approximation of MaterialX Standard Surface through Bevy PBR.
//!
//! The first slice supports MaterialX and universal surface terminals,
//! Standard Surface base/base-color, metalness, specular roughness, emission,
//! opacity and normal, translated constants/arithmetic, and image sampling on
//! UV0. Unknown or invalid graphs receive structured diagnostics and a visible
//! magenta fallback. Limits are 64 uniform values and four 2D images per
//! material. Direct `.mtlx`, closures, arbitrary primvars/UV sets, UDIMs,
//! displacement and volume remain deliberately unsupported.

pub mod compiler;
pub mod diagnostic;
pub mod emit;
pub mod material;
pub mod registry;

/// Translator repository revision pinned by the submodule handover.
pub const TRANSLATOR_REVISION: &str = "01878225a2d7f0e0d200421984d5081cf964db2b";
