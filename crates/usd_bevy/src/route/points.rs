//! Points route (SCHEMA_INTEGRATION Phase C): `UsdGeomPoints` → a Bevy
//! `PointList` mesh (one vertex per point). Read through the geom `Points` /
//! `PointBased` schema.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;

use openusd::schemas::geom::{PointBased, Points};
use openusd::sdf::Value;

use super::{PrimRoute, RouteCtx};

/// Maps `UsdGeomPoints` to a point-cloud mesh.
pub struct PointsRoute;

fn positions(ctx: &RouteCtx) -> Option<Vec<[f32; 3]>> {
    let points = Points::get(ctx.stage, ctx.path.clone()).ok()??;
    match points.points_attr().get::<Value>() {
        Ok(Some(Value::Vec3fVec(v))) => Some(v.iter().map(|p| [p.x, p.y, p.z]).collect()),
        Ok(Some(Value::Vec3dVec(v))) => {
            Some(v.iter().map(|p| [p.x as f32, p.y as f32, p.z as f32]).collect())
        }
        _ => None,
    }
}

impl PrimRoute for PointsRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        ctx.type_name.as_deref() == Some("Points")
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        if world.get_resource::<Assets<Mesh>>().is_none()
            || world.get_resource::<Assets<StandardMaterial>>().is_none()
        {
            return;
        }
        let Some(pos) = positions(ctx) else {
            return;
        };
        if pos.is_empty() {
            return;
        }
        let mut mesh = Mesh::new(PrimitiveTopology::PointList, RenderAssetUsages::default());
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, pos);
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

    #[test]
    fn points_project_pointlist_mesh() {
        let stage = Stage::builder().in_memory("pts.usda").unwrap();
        stage
            .define_prim("/Pts")
            .unwrap()
            .set_type_name("Points")
            .unwrap();
        stage
            .create_attribute("/Pts.points", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 1.0, 1.0].into(),
            ]))
            .unwrap();

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let e = map.entity("/Pts").unwrap();
        let handle = world.get::<Mesh3d>(e).expect("points mesh").0.clone();
        let mesh = world.resource::<Assets<Mesh>>().get(&handle).unwrap();
        assert_eq!(mesh.primitive_topology(), PrimitiveTopology::PointList);
        assert_eq!(mesh.count_vertices(), 2);
    }
}
