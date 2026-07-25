//! Camera route (PLAN P4): `UsdGeomCamera` → a [`Projection`] plus a
//! [`UsdCamera`] marker.
//!
//! Deliberately does **not** attach `Camera3d` / rendering components: a live
//! stage often has several cameras, and activating them would fight the app's
//! own viewport camera. The route projects the camera *parameters* (as a
//! `Projection`, positioned by the prim's `GlobalTransform`); the app decides
//! which — if any — to make active by querying [`UsdCamera`].

use bevy::prelude::*;
use std::f32::consts::PI;

use openusd::schemas::geom::Camera;
use openusd::sdf::Value;

use super::{PrimRoute, RouteCtx};

/// Marker on entities projected from a `UsdGeomCamera`. Carries no data — pair
/// it with the entity's [`Projection`] and `GlobalTransform`.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct UsdCamera;

/// Maps `UsdGeomCamera` prims to a [`Projection`] + [`UsdCamera`] marker.
pub struct CameraRoute;

fn projection_of(ctx: &RouteCtx) -> Projection {
    let Ok(Some(cam)) = Camera::get(ctx.stage, ctx.path.clone()) else {
        return Projection::default();
    };
    // USD aperture + focal length share units (tenths of a scene unit / mm);
    // only their ratio sets the field of view, so the units cancel.
    let focal = cam.focal_length_attr().get::<f32>().ok().flatten().unwrap_or(50.0);
    let v_aperture = cam
        .vertical_aperture_attr()
        .get::<f32>()
        .ok()
        .flatten()
        .unwrap_or(15.2955);
    let clip = match cam.clipping_range_attr().get::<Value>() {
        Ok(Some(Value::Vec2f(c))) => [c.x, c.y],
        Ok(Some(Value::Vec2d(c))) => [c.x as f32, c.y as f32],
        _ => [0.1, 1_000_000.0],
    };
    let is_ortho = matches!(
        cam.projection_attr().get::<Value>(),
        Ok(Some(Value::Token(t))) if t.as_str() == "orthographic"
    );

    if is_ortho {
        Projection::Orthographic(OrthographicProjection {
            near: clip[0],
            far: clip[1],
            ..OrthographicProjection::default_3d()
        })
    } else {
        // Vertical FOV from the aperture / focal-length ratio.
        let fov = 2.0 * (v_aperture / (2.0 * focal.max(1e-3))).atan();
        Projection::Perspective(PerspectiveProjection {
            fov: fov.clamp(1e-3, PI - 1e-3),
            near: clip[0].max(1e-4),
            far: clip[1],
            ..default()
        })
    }
}

impl PrimRoute for CameraRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        ctx.type_name.as_deref() == Some("Camera")
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        let projection = projection_of(ctx);
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert((UsdCamera, projection));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::sdf::Value;
    use openusd::usd::Stage;

    #[test]
    fn camera_projects_perspective() {
        let stage = Stage::builder().in_memory("cam.usda").unwrap();
        stage
            .define_prim("/Cam")
            .unwrap()
            .set_type_name("Camera")
            .unwrap();
        stage
            .create_attribute("/Cam.focalLength", "float")
            .unwrap()
            .set(Value::Float(50.0))
            .unwrap();
        stage
            .create_attribute("/Cam.verticalAperture", "float")
            .unwrap()
            .set(Value::Float(24.0))
            .unwrap();
        stage
            .create_attribute("/Cam.clippingRange", "float2")
            .unwrap()
            .set(Value::Vec2f([0.5, 500.0].into()))
            .unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let e = map.entity("/Cam").unwrap();
        assert!(world.get::<UsdCamera>(e).is_some(), "camera marker present");
        match world.get::<Projection>(e).expect("projection") {
            Projection::Perspective(p) => {
                let expected = 2.0 * (24.0f32 / (2.0 * 50.0)).atan();
                assert!((p.fov - expected).abs() < 1e-4, "fov from aperture/focal");
                assert!((p.near - 0.5).abs() < 1e-4);
                assert!((p.far - 500.0).abs() < 1e-2);
            }
            _ => panic!("expected a perspective projection"),
        }
    }

    #[test]
    fn does_not_attach_camera3d() {
        // The route must not create an active render camera.
        let stage = Stage::builder().in_memory("cam2.usda").unwrap();
        stage
            .define_prim("/Cam")
            .unwrap()
            .set_type_name("Camera")
            .unwrap();
        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        let e = map.entity("/Cam").unwrap();
        assert!(world.get::<Camera3d>(e).is_none(), "no Camera3d attached");
    }
}
