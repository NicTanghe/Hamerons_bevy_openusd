//! Material route (PLAN P4): a gprim's bound `UsdShade` Material →
//! [`StandardMaterial`].
//!
//! Runs after the mesh route, replacing the placeholder material the mesh route
//! attaches. Reads the `material:binding` and decodes the bound
//! `UsdPreviewSurface` (and Omni/MaterialX equivalents) via [`read::shade`].
//! Scalar/colour channels apply headless; texture channels resolve through the
//! [`AssetServer`] when present (path resolution relative to the source layer
//! is a follow-up — see PLAN P4 asset-path handling).

use bevy::prelude::*;

use super::{PrimRoute, RouteCtx};
use crate::read::shade::{ReadPreviewMaterial, read_material_binding, read_preview_material};

/// Maps a bound Material → the entity's [`MeshMaterial3d`].
pub struct MaterialRoute;

/// The prim's decoded preview material, if it has a binding that resolves.
fn material_of(ctx: &RouteCtx) -> Option<ReadPreviewMaterial> {
    let binding = read_material_binding(ctx.stage, ctx.path).ok().flatten()?;
    read_preview_material(ctx.stage, &binding).ok().flatten()
}

fn to_standard_material(
    read: &ReadPreviewMaterial,
    assets: Option<&AssetServer>,
) -> StandardMaterial {
    let mut m = StandardMaterial::default();
    if let Some(c) = read.diffuse_color {
        let a = read.opacity.unwrap_or(1.0);
        m.base_color = Color::srgba(c[0], c[1], c[2], a);
    } else if let Some(a) = read.opacity {
        m.base_color.set_alpha(a);
    }
    if read.opacity.is_some_and(|a| a < 1.0) {
        m.alpha_mode = AlphaMode::Blend;
    }
    if let Some(r) = read.roughness {
        m.perceptual_roughness = r;
    }
    if let Some(mtl) = read.metallic {
        m.metallic = mtl;
    }
    if let Some(e) = read.emissive_color {
        m.emissive = LinearRgba::rgb(e[0], e[1], e[2]);
    }
    if let Some(ior) = read.ior {
        m.ior = ior;
    }
    // UsdTransform2d on the st chain → StandardMaterial UV transform. USD applies
    // `st' = rotate(scale·st) + translation`, matching glam's T·R·S composition.
    if let Some(uv) = &read.uv_transform {
        m.uv_transform = bevy::math::Affine2::from_scale_angle_translation(
            Vec2::from(uv.scale),
            uv.rotation_deg.to_radians(),
            Vec2::from(uv.translation),
        );
    }
    // Textures — loaded through the AssetServer when one is available. Path is
    // used as authored (layer-relative resolution is a P4 follow-up).
    if let Some(server) = assets {
        if let Some(p) = &read.diffuse_texture {
            m.base_color_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.normal_texture {
            m.normal_map_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.metallic_texture {
            m.metallic_roughness_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.emissive_texture {
            m.emissive_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.occlusion_texture {
            m.occlusion_texture = Some(server.load(p.clone()));
        }
    }
    m
}

impl PrimRoute for MaterialRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        read_material_binding(ctx.stage, ctx.path)
            .ok()
            .flatten()
            .is_some()
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        // Only meaningful once the mesh route has given us something to shade;
        // if there's no render Assets there's nothing to attach.
        if world.get_resource::<Assets<StandardMaterial>>().is_none() {
            return;
        }
        let Some(read) = material_of(ctx) else {
            return;
        };
        let assets = world.get_resource::<AssetServer>().cloned();
        let material = to_standard_material(&read, assets.as_ref());
        let handle =
            super::cache::intern_preview_material(world, &read, assets.is_some(), material);
        if let Some(mut mat) = world.get_mut::<MeshMaterial3d<StandardMaterial>>(entity) {
            mat.0 = handle;
        } else if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(MeshMaterial3d(handle));
        }
    }

    fn patch(&self, ctx: &RouteCtx, world: &mut World, entity: Entity, changed: &[&str]) {
        if changed.is_empty() || changed.iter().any(|name| material_property(name)) {
            self.project(ctx, world, entity);
        }
    }
}

fn material_property(name: &str) -> bool {
    name.starts_with("material:binding")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_patch_filter_ignores_purpose() {
        assert!(!material_property("purpose"));
        assert!(!material_property("points"));
        assert!(material_property("material:binding"));
        assert!(material_property("material:binding:preview"));
    }

    #[test]
    fn opaque_material_maps_channels() {
        let read = ReadPreviewMaterial {
            diffuse_color: Some([0.2, 0.4, 0.6]),
            roughness: Some(0.3),
            metallic: Some(0.8),
            emissive_color: Some([1.0, 0.0, 0.0]),
            ior: Some(1.4),
            ..Default::default()
        };
        let m = to_standard_material(&read, None);
        let c = m.base_color.to_srgba();
        assert!((c.red - 0.2).abs() < 1e-3 && (c.blue - 0.6).abs() < 1e-3);
        assert!((c.alpha - 1.0).abs() < 1e-6, "opaque by default");
        assert!((m.perceptual_roughness - 0.3).abs() < 1e-6);
        assert!((m.metallic - 0.8).abs() < 1e-6);
        assert!((m.ior - 1.4).abs() < 1e-6);
        assert_eq!(m.emissive, LinearRgba::rgb(1.0, 0.0, 0.0));
        assert!(matches!(m.alpha_mode, AlphaMode::Opaque));
    }

    #[test]
    fn translucent_material_sets_blend() {
        let read = ReadPreviewMaterial {
            diffuse_color: Some([1.0, 1.0, 1.0]),
            opacity: Some(0.5),
            ..Default::default()
        };
        let m = to_standard_material(&read, None);
        assert!((m.base_color.to_srgba().alpha - 0.5).abs() < 1e-3, "alpha from opacity");
        assert!(matches!(m.alpha_mode, AlphaMode::Blend), "opacity<1 → Blend");
    }

    #[test]
    fn empty_material_is_default_ish() {
        // An all-None read yields (essentially) the StandardMaterial default.
        let m = to_standard_material(&ReadPreviewMaterial::default(), None);
        let def = StandardMaterial::default();
        assert_eq!(m.base_color.to_srgba(), def.base_color.to_srgba());
        assert!(matches!(m.alpha_mode, AlphaMode::Opaque));
    }
}
