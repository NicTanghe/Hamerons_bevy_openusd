//! Dome-light route (PLAN Phase B1): `UsdLuxDomeLight` → environment lighting.
//!
//! A `DomeLight` is an image-based environment (a lat-long HDR wrapping the
//! scene). Bevy's `EnvironmentMapLight` wants **pre-filtered cubemaps** (a
//! diffuse irradiance map + a mip-chained specular map), and stock Bevy has no
//! runtime convolution from an equirectangular HDR — so a faithful IBL bake is
//! out of scope here (that's the deferred B2).
//!
//! What this route does now:
//! * emits a [`UsdDomeLight`] marker carrying the texture, format, color and
//!   intensity, so an app can wire real IBL / a `Skybox` to its camera;
//! * sets the global [`GlobalAmbientLight`] to `color × intensity` as a cheap
//!   stand-in so a dome-lit scene isn't left black.

use bevy::prelude::*;

use openusd::schemas::lux::{DomeLight, Light};
use openusd::sdf::Value;

use super::{PrimRoute, RouteCtx};

/// USD dome `intensity` (default 1) → Bevy `GlobalAmbientLight::brightness` (lux-ish).
/// Approximate — the app can override the ambient after projection.
const AMBIENT_SCALE: f32 = 100.0;

/// A `UsdLuxDomeLight`: an image-based environment light. Bevy can't consume the
/// lat-long HDR directly (see the module docs), so this carries the authored
/// data for an app to build real IBL / a skybox from.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct UsdDomeLight {
    /// The environment texture (`inputs:texture:file`), unresolved path string.
    pub texture: String,
    /// `inputs:texture:format` (`latlong`, `mirroredBall`, `angular`,
    /// `cubeMapVerticalCross`, or `automatic`).
    pub format: String,
    /// Linear light color × exposure-scaled intensity, as authored.
    pub color: [f32; 3],
    /// The exposure-scaled `inputs:intensity`.
    pub intensity: f32,
}

fn asset_string(v: Option<Value>) -> String {
    match v {
        Some(Value::AssetPath(a)) => a.as_str().to_string(),
        Some(Value::String(s)) => s,
        Some(Value::Token(t)) => t.as_str().to_string(),
        _ => String::new(),
    }
}

fn token_string(v: Option<Value>) -> String {
    match v {
        Some(Value::Token(t)) => t.as_str().to_string(),
        Some(Value::String(s)) => s,
        _ => "automatic".to_string(),
    }
}

/// Projects `DomeLight` prims as [`UsdDomeLight`] markers + an ambient stand-in.
pub struct DomeLightRoute;

impl PrimRoute for DomeLightRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        // `DomeLight_1` is the schema-versioned typeName for the same prim.
        matches!(ctx.type_name.as_deref(), Some("DomeLight") | Some("DomeLight_1"))
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        let Ok(Some(dome)) = DomeLight::get(ctx.stage, ctx.path.clone()) else {
            return;
        };
        let color = match dome.color_attr().get::<Value>() {
            Ok(Some(Value::Vec3f(c))) => [c.x, c.y, c.z],
            Ok(Some(Value::Vec3d(c))) => [c.x as f32, c.y as f32, c.z as f32],
            _ => [1.0, 1.0, 1.0],
        };
        let base = dome.intensity_attr().get::<f32>().ok().flatten().unwrap_or(1.0);
        let exposure = dome.exposure_attr().get::<f32>().ok().flatten().unwrap_or(0.0);
        let intensity = base * 2f32.powf(exposure);

        let marker = UsdDomeLight {
            texture: asset_string(dome.texture_file_attr().get::<Value>().ok().flatten()),
            format: token_string(dome.texture_format_attr().get::<Value>().ok().flatten()),
            color,
            intensity,
        };

        // Cheap stand-in so the scene isn't unlit. A single dome is the common
        // case; multiple domes are last-write-wins (documented approximation).
        let ambient = GlobalAmbientLight {
            color: Color::linear_rgb(color[0], color[1], color[2]),
            brightness: intensity * AMBIENT_SCALE,
            ..default()
        };
        if let Some(mut a) = world.get_resource_mut::<GlobalAmbientLight>() {
            *a = ambient;
        } else {
            world.insert_resource(ambient);
        }

        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(marker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::usd::Stage;

    #[test]
    fn dome_light_projects_marker_and_ambient() {
        let stage = Stage::builder().in_memory("dome.usda").unwrap();
        let dome = DomeLight::define(&stage, "/Env").unwrap();
        dome.create_texture_file_attr()
            .unwrap()
            .set(Value::AssetPath("./studio.hdr".into()))
            .unwrap();
        dome.create_texture_format_attr()
            .unwrap()
            .set(Value::Token("latlong".into()))
            .unwrap();
        dome.create_intensity_attr().unwrap().set(Value::Float(2.0)).unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let e = map.entity("/Env").unwrap();
        let m = world.get::<UsdDomeLight>(e).expect("dome marker");
        assert_eq!(m.texture, "./studio.hdr");
        assert_eq!(m.format, "latlong");
        assert_eq!(m.intensity, 2.0);

        let ambient = world.resource::<GlobalAmbientLight>();
        assert!(
            (ambient.brightness - 2.0 * AMBIENT_SCALE).abs() < 1e-3,
            "ambient brightness follows dome intensity"
        );
    }
}
