//! PointInstancer route (PLAN P4, in-repo): `UsdGeomPointInstancer` → one
//! child entity per instance, sharing the prototype's baked geometry.
//!
//! This is *point* instancing (an explicit position/orientation/scale table),
//! distinct from USD scenegraph instancing via instanceable composition arcs.
//! Instances are spawned as children of the instancer entity, each carrying a
//! [`UsdInstance`] marker so a reproject can clear the previous batch. A USD
//! prototype may be a `Mesh` directly or an `Xform`/`Scope` subtree containing
//! one or more meshes. The subtree's local transforms are retained, while all
//! instances share the same mesh/material handles (baked once per project).

use bevy::platform::collections::HashMap;
use bevy::prelude::*;

use super::{DisplayPurposes, PrimRoute, RouteCtx};
use crate::read::geom::{
    ReadPointInstancer, VisibilityState, read_effective_purpose, read_point_instancer,
    read_visibility,
};
use crate::read::xform::read_transform_at;
use openusd::schemas::geom::PointInstancer;
use openusd::sdf::Value;

/// Stable instance IDs marked invisible via the schema's `invisibleIds`.
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

/// One prim in a baked prototype subtree. `parent` indexes another entry in
/// the same vector; entries are stored parent-before-child so they can be
/// materialized in one pass for every point instance.
#[derive(Clone)]
struct PrototypeNode {
    parent: Option<usize>,
    transform: Transform,
    visibility: Visibility,
    render: Option<ProtoHandles>,
}

/// A prototype can contain multiple meshes and transform-only grouping prims.
type BakedPrototype = Vec<PrototypeNode>;

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

fn local_transform(ctx: &RouteCtx, path: &openusd::sdf::Path) -> Transform {
    let Ok(Some(t)) = read_transform_at(ctx.stage, path, ctx.time) else {
        return Transform::IDENTITY;
    };
    Transform {
        translation: Vec3::from_array(t.translate),
        rotation: Quat::from_array(t.rotate),
        scale: Vec3::from_array(t.scale),
    }
}

fn prototype_visibility(ctx: &RouteCtx, world: &World, path: &openusd::sdf::Path) -> Visibility {
    let purpose = read_effective_purpose(ctx.stage, path).unwrap_or_else(|_| "default".to_string());
    let purposes = world
        .get_resource::<DisplayPurposes>()
        .copied()
        .unwrap_or_default();
    let invisible = matches!(
        read_visibility(ctx.stage, path),
        Ok(VisibilityState::Invisible)
    );
    if invisible || !purposes.shows(&purpose) {
        Visibility::Hidden
    } else {
        Visibility::default()
    }
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

        bevy::log::trace!(
            target: "usd_bevy::route::instancer",
            "{}: PointInstancer {} positions, {} protoIndices, {} prototypes",
            ctx.prim_str(),
            read.positions.len(),
            read.proto_indices.len(),
            read.prototypes.len(),
        );

        // Bake each referenced prototype subtree once; share its handles.
        let mut proto_cache: HashMap<usize, Option<BakedPrototype>> = HashMap::default();

        for i in 0..read.positions.len() {
            // `invisibleIds` addresses stable `ids`, falling back to the array
            // index only when `ids` is unauthored (USD's implicit-ID rule).
            let id = read.ids.get(i).copied().unwrap_or(i as i64);
            if invisible.contains(&id) {
                continue;
            }
            let xf = instance_transform(&read, i);
            let proto_idx = read.proto_indices.get(i).copied().unwrap_or(0) as usize;

            let prototype = if have_assets {
                proto_cache
                    .entry(proto_idx)
                    .or_insert_with(|| bake_prototype(ctx, world, &read, proto_idx))
                    .clone()
            } else {
                None
            };

            let instance = world
                .spawn((UsdInstance, xf, Visibility::default(), ChildOf(entity)))
                .id();

            if let Some(nodes) = prototype {
                let mut spawned = Vec::with_capacity(nodes.len());
                for node in nodes {
                    let parent = node.parent.map(|p| spawned[p]).unwrap_or(instance);
                    let mut e = world.spawn((node.transform, node.visibility, ChildOf(parent)));
                    if let Some((mesh, material)) = node.render {
                        e.insert((Mesh3d(mesh), MeshMaterial3d(material)));
                    }
                    spawned.push(e.id());
                }
            }
        }
    }
}

/// Bake the prototype at `proto_idx`, including descendant meshes and every
/// local transform needed to place them. USD explicitly permits prototype
/// relationship targets to be arbitrary prim subtrees, not only `Mesh` prims.
fn bake_prototype(
    ctx: &RouteCtx,
    world: &mut World,
    read: &ReadPointInstancer,
    proto_idx: usize,
) -> Option<BakedPrototype> {
    let proto_path = read.prototypes.get(proto_idx)?;
    let mut nodes = Vec::new();
    bake_prototype_node(ctx, world, proto_path, None, &mut nodes);
    if !nodes.iter().any(|node| node.render.is_some()) {
        bevy::log::warn!(
            target: "usd_bevy::route::instancer",
            "{}: prototype {} contains no supported Mesh geometry",
            ctx.prim_str(),
            proto_path.as_str(),
        );
        return None;
    }
    Some(nodes)
}

fn bake_prototype_node(
    ctx: &RouteCtx,
    world: &mut World,
    path: &openusd::sdf::Path,
    parent: Option<usize>,
    nodes: &mut BakedPrototype,
) {
    let render = crate::read::geom::read_mesh(ctx.stage, path)
        .ok()
        .flatten()
        .map(|read| {
            bevy::log::trace!(
                target: "usd_bevy::route::instancer",
                "{}: baking prototype mesh {} ({} points)",
                ctx.prim_str(),
                path.as_str(),
                read.points.len(),
            );
            let mesh = crate::mesh::mesh_from_usd(&read);
            let mesh_handle = super::cache::intern_mesh(world, mesh);
            let material = world
                .resource_mut::<Assets<StandardMaterial>>()
                .add(StandardMaterial::default());
            (mesh_handle, material)
        });

    let node_index = nodes.len();
    nodes.push(PrototypeNode {
        parent,
        transform: local_transform(ctx, path),
        visibility: prototype_visibility(ctx, world, path),
        render,
    });

    let child_names = ctx
        .stage
        .prim(path.clone())
        .child_names()
        .unwrap_or_default();
    for child_name in child_names {
        let Ok(child) = path.append_path(child_name.as_str()) else {
            continue;
        };
        bake_prototype_node(ctx, world, &child, Some(node_index), nodes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::schemas::geom::PointInstancer;
    use openusd::sdf::Value;
    use openusd::usd::Stage;

    fn descendants_with_mesh(world: &World, root: Entity) -> Vec<Entity> {
        fn visit(world: &World, entity: Entity, found: &mut Vec<Entity>) {
            if world.get::<Mesh3d>(entity).is_some() {
                found.push(entity);
            }
            if let Some(children) = world.get::<Children>(entity) {
                for child in children.iter() {
                    visit(world, child, found);
                }
            }
        }
        let mut found = Vec::new();
        visit(world, root, &mut found);
        found
    }

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
    fn point_instancer_projects_mesh_below_xform_prototype() {
        let stage = Stage::builder().in_memory("pi_nested.usda").unwrap();
        let pi = PointInstancer::define(&stage, "/PI").unwrap();
        stage
            .define_prim("/PI/Prototypes")
            .unwrap()
            .set_type_name("Scope")
            .unwrap();
        stage
            .define_prim("/PI/Prototypes/Chair")
            .unwrap()
            .set_type_name("Xform")
            .unwrap();
        stage
            .define_prim("/PI/Prototypes/Chair/Geo")
            .unwrap()
            .set_type_name("Mesh")
            .unwrap();
        stage
            .create_attribute("/PI/Prototypes/Chair.xformOpOrder", "token[]")
            .unwrap()
            .set(Value::token_vec(["xformOp:translate"]))
            .unwrap();
        stage
            .create_attribute("/PI/Prototypes/Chair.xformOp:translate", "double3")
            .unwrap()
            .set(Value::Vec3d([1.0, 2.0, 3.0].into()))
            .unwrap();
        stage
            .create_attribute("/PI/Prototypes/Chair/Geo.points", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 0.0, 0.0].into(),
                [0.0, 1.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/PI/Prototypes/Chair/Geo.faceVertexCounts", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![3]))
            .unwrap();
        stage
            .create_attribute("/PI/Prototypes/Chair/Geo.faceVertexIndices", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![0, 1, 2]))
            .unwrap();
        pi.create_prototypes_rel()
            .unwrap()
            .set_targets([openusd::sdf::path("/PI/Prototypes/Chair").unwrap()])
            .unwrap();
        pi.create_positions_attr()
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [5.0, 0.0, 0.0].into(),
            ]))
            .unwrap();
        pi.create_proto_indices_attr()
            .unwrap()
            .set(Value::IntVec(vec![0, 0]))
            .unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let instancer = map.entity("/PI").unwrap();
        let instances: Vec<Entity> = world
            .get::<Children>(instancer)
            .unwrap()
            .iter()
            .filter(|entity| world.get::<UsdInstance>(*entity).is_some())
            .collect();
        assert_eq!(instances.len(), 2);

        let first_meshes = descendants_with_mesh(&world, instances[0]);
        let second_meshes = descendants_with_mesh(&world, instances[1]);
        assert_eq!(first_meshes.len(), 1, "nested prototype mesh is rendered");
        assert_eq!(second_meshes.len(), 1, "every instance gets the subtree");
        assert_eq!(
            world.get::<Mesh3d>(first_meshes[0]).unwrap().0,
            world.get::<Mesh3d>(second_meshes[0]).unwrap().0,
            "instances share the baked mesh handle"
        );

        let prototype_root = world
            .get::<Children>(instances[0])
            .unwrap()
            .iter()
            .next()
            .unwrap();
        assert_eq!(
            world.get::<Transform>(prototype_root).unwrap().translation,
            Vec3::new(1.0, 2.0, 3.0),
            "prototype-root transform is preserved"
        );
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
        stage
            .create_attribute("/PI.ids", "int64[]")
            .unwrap()
            .set(Value::Int64Vec(vec![10, 20, 30]))
            .unwrap();
        // Hide the instance carrying stable id 20 (array index 1).
        stage
            .create_attribute("/PI.invisibleIds", "int64[]")
            .unwrap()
            .set(Value::Int64Vec(vec![20]))
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
