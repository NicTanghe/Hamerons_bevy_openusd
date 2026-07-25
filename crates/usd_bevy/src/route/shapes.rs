//! Shapes route (SCHEMA_INTEGRATION Phase B): USD geom shape prims
//! (`Cube`/`Sphere`/`Cylinder`/`Capsule`/`Cone`/`Plane`) → Bevy primitive
//! meshes. Reads through openusd's `geom` shape schemas.
//!
//! USD's `Cylinder`/`Cone`/`Capsule`/`Plane` carry an `axis` (default `Z`);
//! Bevy primitives are `Y`-aligned, so the generated mesh is rotated to match.
//! Registered before the material route, which then binds a real material.

use bevy::prelude::*;
use std::f32::consts::FRAC_PI_2;

use openusd::schemas::geom::{Capsule, Cone, Cube, Cylinder, Plane, Sphere};
use openusd::sdf::Value;

use super::{PrimRoute, RouteCtx};

/// Maps USD geometric shapes to Bevy primitive meshes.
pub struct ShapesRoute;

fn f32_attr(attr: openusd::usd::Attribute, default: f32) -> f32 {
    match attr.get::<Value>() {
        Ok(Some(Value::Double(d))) => d as f32,
        Ok(Some(Value::Float(f))) => f,
        _ => default,
    }
}

/// Rotation taking a `Y`-aligned Bevy primitive onto the USD `axis` token.
fn axis_rotation(attr: openusd::usd::Attribute) -> Quat {
    let axis = match attr.get::<Value>() {
        Ok(Some(Value::Token(t))) => t.as_str().to_string(),
        _ => "Z".to_string(), // USD default axis
    };
    match axis.as_str() {
        "X" => Quat::from_rotation_z(-FRAC_PI_2), // +Y → +X
        "Z" => Quat::from_rotation_x(FRAC_PI_2),  // +Y → +Z
        _ => Quat::IDENTITY,                       // "Y"
    }
}

fn shape_mesh(ctx: &RouteCtx) -> Option<Mesh> {
    let stage = ctx.stage;
    let p = ctx.path.clone();
    match ctx.type_name.as_deref()? {
        "Cube" => {
            let cube = Cube::get(stage, p).ok()??;
            let size = f32_attr(cube.size_attr(), 2.0);
            Some(Mesh::from(Cuboid::from_length(size)))
        }
        "Sphere" => {
            let sphere = Sphere::get(stage, p).ok()??;
            let r = f32_attr(sphere.radius_attr(), 1.0);
            Some(Mesh::from(bevy::math::primitives::Sphere::new(r)))
        }
        "Cylinder" => {
            let cyl = Cylinder::get(stage, p).ok()??;
            let r = f32_attr(cyl.radius_attr(), 1.0);
            let h = f32_attr(cyl.height_attr(), 2.0);
            let mesh = Mesh::from(bevy::math::primitives::Cylinder::new(r, h));
            Some(mesh.rotated_by(axis_rotation(cyl.axis_attr())))
        }
        "Capsule" => {
            let cap = Capsule::get(stage, p).ok()??;
            let r = f32_attr(cap.radius_attr(), 0.5);
            let h = f32_attr(cap.height_attr(), 1.0);
            let mesh = Mesh::from(Capsule3d::new(r, h));
            Some(mesh.rotated_by(axis_rotation(cap.axis_attr())))
        }
        "Cone" => {
            let cone = Cone::get(stage, p).ok()??;
            let r = f32_attr(cone.radius_attr(), 1.0);
            let h = f32_attr(cone.height_attr(), 2.0);
            let mesh = Mesh::from(bevy::math::primitives::Cone::new(r, h));
            Some(mesh.rotated_by(axis_rotation(cone.axis_attr())))
        }
        "Plane" => {
            let plane = Plane::get(stage, p).ok()??;
            let w = f32_attr(plane.width_attr(), 1.0);
            let l = f32_attr(plane.length_attr(), 1.0);
            let mesh = Mesh::from(Rectangle::new(w, l));
            // Rectangle lies in the XY plane (normal +Z); USD plane's normal is
            // its `axis`. Rotate XY→ the axis plane.
            Some(mesh.rotated_by(axis_rotation(plane.axis_attr())))
        }
        _ => None,
    }
}

impl PrimRoute for ShapesRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        matches!(
            ctx.type_name.as_deref(),
            Some("Cube" | "Sphere" | "Cylinder" | "Capsule" | "Cone" | "Plane")
        )
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        if world.get_resource::<Assets<Mesh>>().is_none()
            || world.get_resource::<Assets<StandardMaterial>>().is_none()
        {
            return;
        }
        let Some(mesh) = shape_mesh(ctx) else {
            return;
        };
        let mesh_handle = super::cache::intern_mesh(world, mesh);
        let material = world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert((Mesh3d(mesh_handle), MeshMaterial3d(material)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::usd::Stage;

    fn world() -> World {
        let mut w = World::new();
        w.insert_resource(Assets::<Mesh>::default());
        w.insert_resource(Assets::<StandardMaterial>::default());
        w.insert_resource(SchemaRegistry::builtin());
        w
    }

    #[test]
    fn shapes_project_meshes() {
        let stage = Stage::builder().in_memory("shapes.usda").unwrap();
        for ty in ["Cube", "Sphere", "Cylinder", "Capsule", "Cone", "Plane"] {
            stage
                .define_prim(format!("/{ty}").as_str())
                .unwrap()
                .set_type_name(ty)
                .unwrap();
        }
        let live = LiveStage::new(stage);
        let mut world = world();
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        for ty in ["Cube", "Sphere", "Cylinder", "Capsule", "Cone", "Plane"] {
            let e = map.entity(&format!("/{ty}")).unwrap();
            assert!(
                world.get::<Mesh3d>(e).is_some(),
                "{ty} projected a primitive mesh"
            );
        }
    }

    #[test]
    fn cube_size_respected() {
        let stage = Stage::builder().in_memory("cube.usda").unwrap();
        stage
            .define_prim("/C")
            .unwrap()
            .set_type_name("Cube")
            .unwrap();
        stage
            .create_attribute("/C.size", "double")
            .unwrap()
            .set(Value::Double(4.0))
            .unwrap();
        let live = LiveStage::new(stage);
        let mut world = world();
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        let e = map.entity("/C").unwrap();
        let handle = world.get::<Mesh3d>(e).unwrap().0.clone();
        let mesh = world.resource::<Assets<Mesh>>().get(&handle).unwrap();
        // A size-4 cube spans [-2, 2] on each axis.
        let pos = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap();
        let bevy::mesh::VertexAttributeValues::Float32x3(v) = pos else {
            panic!("positions");
        };
        let max_x = v.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
        assert!((max_x - 2.0).abs() < 1e-4, "cube half-extent from size, got {max_x}");
    }
}
