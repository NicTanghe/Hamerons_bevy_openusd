//! PointInstancer route (PLAN P4, in-repo): `UsdGeomPointInstancer` → one
//! child entity per instance, sharing the prototype's baked mesh.
//!
//! This is *point* instancing (an explicit position/orientation/scale table),
//! distinct from USD native scenegraph instancing (which is openusd-blocked).
//! Instances are spawned as children of the instancer entity, each carrying a
//! [`UsdInstance`] marker so a reproject can clear the previous batch. All
//! instances of one prototype share a single `Mesh`/`StandardMaterial` handle
//! (baked once per project), so N instances cost one mesh in memory.

use bevy::prelude::*;
use bevy::platform::collections::HashMap;

use super::{PrimRoute, RouteCtx};
use crate::read::geom::{ReadPointInstancer, read_point_instancer};
use openusd::schemas::geom::PointInstancer;
use openusd::sdf::Value;

/// Instance indices marked invisible via the schema's `invisibleIds`.
fn invisible_ids(ctx: &RouteCtx) -> bevy::platform::collections::HashSet<i64> {
    let mut set = bevy::platform::collections::HashSet::default();
    if let Ok(Some(pi)) = PointInstancer::get(ctx.stage, ctx.path.clone()) {
        match pi.invisible_ids_attr().get::<Value>() {
            Ok(Some(Value::Int64Vec(v))) => set.extend(v),
            Ok(Some(Value::IntVec(v))) => set.extend(v.into_iter().map(i64::from)),
            _ => {}
        }
    }
    set
}

/// A baked prototype's shared render handles.
type ProtoHandles = (Handle<Mesh>, Handle<StandardMaterial>);

/// Marker on entities spawned for a PointInstancer instance.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct UsdInstance;

/// Maps a `PointInstancer` prim to per-instance child entities.
pub struct PointInstancerRoute;

fn instance_transform(read: &ReadPointInstancer, i: usize) -> Transform {
    let t = read.positions[i];
    let mut xf = Transform::from_translation(Vec3::from_array(t));
    if let Some(o) = read.orientations.get(i) {
        // read_quat_array yields [w, x, y, z]; bevy is xyzw.
        xf.rotation = Quat::from_xyzw(o[1], o[2], o[3], o[0]);
    }
    if let Some(s) = read.scales.get(i) {
        xf.scale = Vec3::from_array(*s);
    }
    xf
}

impl PointInstancerRoute {
    /// Despawn instance children this route spawned on a previous project, so a
    /// reproject doesn't stack duplicate batches.
    fn clear_instances(world: &mut World, entity: Entity) {
        let existing: Vec<Entity> = world
            .get::<Children>(entity)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        for child in existing {
            if world.get::<UsdInstance>(child).is_some() {
                world.entity_mut(child).despawn();
            }
        }
    }
}

impl PrimRoute for PointInstancerRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        ctx.type_name.as_deref() == Some("PointInstancer")
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        let Ok(Some(read)) = read_point_instancer(ctx.stage, ctx.path) else {
            return;
        };
        Self::clear_instances(world, entity);

        // Honor `invisibleIds` (read through the geom schema): those instances
        // are culled entirely.
        let invisible = invisible_ids(ctx);

        let have_assets = world.get_resource::<Assets<Mesh>>().is_some()
            && world.get_resource::<Assets<StandardMaterial>>().is_some();

        // Bake each referenced prototype's mesh once; share the handles.
        let mut proto_cache: HashMap<usize, Option<ProtoHandles>> =
            HashMap::default();

        for i in 0..read.positions.len() {
            if invisible.contains(&(i as i64)) {
                continue;
            }
            let xf = instance_transform(&read, i);
            let proto_idx = read.proto_indices.get(i).copied().unwrap_or(0) as usize;

            let handles = if have_assets {
                proto_cache
                    .entry(proto_idx)
                    .or_insert_with(|| bake_prototype(ctx, world, &read, proto_idx))
                    .clone()
            } else {
                None
            };

            let mut e = world.spawn((UsdInstance, xf, Visibility::default(), ChildOf(entity)));
            if let Some((mesh, material)) = handles {
                e.insert((Mesh3d(mesh), MeshMaterial3d(material)));
            }
        }
    }
}

/// Bake the prototype at `proto_idx` into a shared `(Mesh, Material)`. Returns
/// `None` if the prototype path doesn't resolve to a readable mesh.
fn bake_prototype(
    ctx: &RouteCtx,
    world: &mut World,
    read: &ReadPointInstancer,
    proto_idx: usize,
) -> Option<ProtoHandles> {
    let proto_path = read.prototypes.get(proto_idx)?;
    let mesh_read = crate::read::geom::read_mesh(ctx.stage, proto_path).ok().flatten()?;
    let mesh = crate::mesh::mesh_from_usd(&mesh_read);
    let mesh_handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
    let material = world
        .resource_mut::<Assets<StandardMaterial>>()
        .add(StandardMaterial::default());
    Some((mesh_handle, material))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::sdf::Value;
    use openusd::usd::Stage;

    #[test]
    fn point_instancer_spawns_instance_entities() {
        let stage = Stage::builder().in_memory("pi.usda").unwrap();
        stage
            .define_prim("/PI")
            .unwrap()
            .set_type_name("PointInstancer")
            .unwrap();
        stage
            .create_attribute("/PI.positions", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [5.0, 0.0, 0.0].into(),
                [0.0, 5.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/PI.protoIndices", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![0, 0, 0]))
            .unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let pi = map.entity("/PI").unwrap();
        let children: Vec<Entity> = world
            .get::<Children>(pi)
            .map(|c| c.iter().collect())
            .unwrap_or_default();
        let instances: Vec<Entity> = children
            .into_iter()
            .filter(|e| world.get::<UsdInstance>(*e).is_some())
            .collect();
        assert_eq!(instances.len(), 3, "one child entity per instance");

        // Second instance sits at (5,0,0).
        let at_5 = instances.iter().any(|e| {
            world
                .get::<Transform>(*e)
                .map(|t| (t.translation.x - 5.0).abs() < 1e-4)
                .unwrap_or(false)
        });
        assert!(at_5, "instance transform placed from positions");
    }

    #[test]
    fn reproject_clears_previous_instances() {
        let stage = Stage::builder().in_memory("pi2.usda").unwrap();
        stage
            .define_prim("/PI")
            .unwrap()
            .set_type_name("PointInstancer")
            .unwrap();
        stage
            .create_attribute("/PI.positions", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![[0.0, 0.0, 0.0].into()]))
            .unwrap();
        stage
            .create_attribute("/PI.protoIndices", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![0]))
            .unwrap();
        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        let pi = map.entity("/PI").unwrap();

        // Re-run the route (as a resync would) and confirm no duplication.
        let registry = SchemaRegistry::builtin();
        let p = openusd::sdf::path("/PI").unwrap();
        registry.patch_prim(&live.stage, &p, &mut world, pi, &[]);

        let instances = world
            .get::<Children>(pi)
            .map(|c| {
                c.iter()
                    .filter(|e| world.get::<UsdInstance>(*e).is_some())
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(instances, 1, "reproject cleared and rebuilt, no stacking");
    }

    #[test]
    fn invisible_ids_are_culled() {
        let stage = Stage::builder().in_memory("pi3.usda").unwrap();
        stage
            .define_prim("/PI")
            .unwrap()
            .set_type_name("PointInstancer")
            .unwrap();
        stage
            .create_attribute("/PI.positions", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 0.0, 0.0].into(),
                [2.0, 0.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/PI.protoIndices", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![0, 0, 0]))
            .unwrap();
        // Hide instance index 1.
        stage
            .create_attribute("/PI.invisibleIds", "int64[]")
            .unwrap()
            .set(Value::Int64Vec(vec![1]))
            .unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        let pi = map.entity("/PI").unwrap();
        let count = world
            .get::<Children>(pi)
            .map(|c| {
                c.iter()
                    .filter(|e| world.get::<UsdInstance>(*e).is_some())
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(count, 2, "the invisible instance was culled (3 → 2)");
    }
}
