//! Geometry routes: `visibility` → Bevy [`Visibility`], and mesh prims →
//! [`Mesh3d`] + a placeholder [`MeshMaterial3d`]. Real material binding (from
//! `read::shade`) layers on as its own route later (PLAN P4).

use bevy::prelude::*;

use super::{DisplayPurposes, PrimRoute, RouteCtx};
use crate::read::geom::{
    VisibilityState, read_effective_purpose, read_mesh, read_visibility,
};

/// The prim's effective (inherited) USD `purpose`: `"default"`, `"render"`,
/// `"proxy"`, or `"guide"`. Carried so gameplay/UI can query or re-filter it.
#[derive(Component, Debug, Clone)]
pub struct UsdPurpose(pub String);

/// Maps `visibility` (+ `purpose`) → [`Visibility`]. Applies to every prim
/// (imageable or not); an unauthored `visibility` reads as inherited/visible.
///
/// A prim is hidden if it is authored `invisible` **or** its effective purpose
/// isn't in the world's [`DisplayPurposes`] (PLAN Phase A) — so `guide`
/// annotations and, by default, the `render` twin of a `proxy`/`render` pair
/// don't draw. `purpose` is inherited down namespace, so this resolves the
/// effective purpose from the nearest ancestor with an authored opinion.
pub struct VisibilityRoute;

/// Combined visibility + effective purpose for `entity`'s prim, honoring the
/// world's [`DisplayPurposes`] (defaults when the resource is absent).
fn resolve(ctx: &RouteCtx, world: &World) -> (Visibility, String) {
    let purpose = read_effective_purpose(ctx.stage, ctx.path)
        .unwrap_or_else(|_| "default".to_string());
    let purposes = world
        .get_resource::<DisplayPurposes>()
        .copied()
        .unwrap_or_default();
    let invisible = matches!(
        read_visibility(ctx.stage, ctx.path),
        Ok(VisibilityState::Invisible)
    );
    let prototype_source = world
        .get_resource::<super::instancer::PointInstancerPrototypeSources>()
        .is_some_and(|sources| sources.contains(ctx.prim_str()));
    let hidden = prototype_source || invisible || !purposes.shows(&purpose);
    let vis = if hidden {
        Visibility::Hidden
    } else {
        Visibility::default()
    };
    (vis, purpose)
}

fn apply(ctx: &RouteCtx, world: &mut World, entity: Entity) {
    let (vis, purpose) = resolve(ctx, world);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert((vis, UsdPurpose(purpose)));
    }
}

impl PrimRoute for VisibilityRoute {
    fn matches(&self, _ctx: &RouteCtx) -> bool {
        true
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        apply(ctx, world, entity);
    }

    fn patch(&self, ctx: &RouteCtx, world: &mut World, entity: Entity, changed: &[&str]) {
        let touches = changed.is_empty()
            || changed.contains(&"visibility")
            || changed.contains(&"purpose");
        if !touches {
            return;
        }
        apply(ctx, world, entity);
    }
}

/// Bakes a UsdGeomMesh's points/topology into a Bevy [`Mesh`] and attaches
/// [`Mesh3d`] + a default [`StandardMaterial`]. No-op (with a warning) when the
/// render `Assets` are absent (headless) — the prim still projects, it just
/// carries no renderable geometry.
pub struct MeshRoute;

impl MeshRoute {
    /// Bake + attach; returns whether a `Mesh3d` was inserted.
    fn attach(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) -> bool {
        let prototype_source = world
            .get_resource::<super::instancer::PointInstancerPrototypeSources>()
            .is_some_and(|sources| sources.contains(ctx.prim_str()));
        if prototype_source {
            // This mesh is source data for a PointInstancer. The instancer
            // route bakes it once and shares that handle across instances;
            // attaching it here would draw an extra copy at the source path.
            if let Ok(mut e) = world.get_entity_mut(entity) {
                e.remove::<Mesh3d>();
                e.remove::<MeshMaterial3d<StandardMaterial>>();
            }
            return false;
        }
        let Ok(Some(read)) = read_mesh(ctx.stage, ctx.path) else {
            return false;
        };
        if world.get_resource::<Assets<Mesh>>().is_none()
            || world.get_resource::<Assets<StandardMaterial>>().is_none()
        {
            bevy::log::warn!(
                target: "usd_bevy::route::geom",
                "{}: has a mesh but render Assets are absent — not attached",
                ctx.prim_str()
            );
            return false;
        }
        bevy::log::trace!(
            target: "usd_bevy::route::geom",
            "{}: mesh {} points -> Mesh3d",
            ctx.prim_str(),
            read.points.len()
        );
        let mesh = crate::mesh::mesh_from_usd(&read);
        let mesh_handle = super::cache::intern_mesh(world, mesh);
        let material = world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert((Mesh3d(mesh_handle), MeshMaterial3d(material)));
            return true;
        }
        false
    }
}

impl PrimRoute for MeshRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        // Fast path on typeName; fall back to probing for `points` so meshes
        // authored without an explicit type (rare, but valid) still route.
        matches!(ctx.type_name.as_deref(), Some("Mesh"))
            || read_mesh(ctx.stage, ctx.path).ok().flatten().is_some()
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        self.attach(ctx, world, entity);
    }

    fn patch(&self, ctx: &RouteCtx, world: &mut World, entity: Entity, changed: &[&str]) {
        let touches_geometry = changed.is_empty()
            || changed.iter().any(|property| {
                matches!(
                    *property,
                    "points"
                        | "faceVertexCounts"
                        | "faceVertexIndices"
                        | "normals"
                        | "orientation"
                        | "subdivisionScheme"
                        | "doubleSided"
                ) || property.starts_with("primvars:")
            });
        if touches_geometry {
            self.attach(ctx, world, entity);
        }
    }
}

#[cfg(test)]
mod purpose_tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::{DisplayPurposes, SchemaRegistry};
    use openusd::usd::Stage;

    fn purpose_stage() -> Stage {
        let stage = Stage::builder().in_memory("purpose.usda").unwrap();
        let def = |path: &str, purpose: Option<&str>| {
            stage.define_prim(path).unwrap().set_type_name("Xform").unwrap();
            if let Some(p) = purpose {
                stage
                    .create_attribute(format!("{path}.purpose").as_str(), "token")
                    .unwrap()
                    .set(openusd::sdf::Value::Token(p.into()))
                    .unwrap();
            }
        };
        def("/Plain", None);
        def("/Proxy", Some("proxy"));
        def("/Render", Some("render"));
        def("/Guide", Some("guide"));
        // A Scope authored `proxy` — its child inherits proxy (pruning).
        stage.define_prim("/Grp").unwrap().set_type_name("Scope").unwrap();
        stage
            .create_attribute("/Grp.purpose", "token")
            .unwrap()
            .set(openusd::sdf::Value::Token("proxy".into()))
            .unwrap();
        stage.define_prim("/Grp/Child").unwrap().set_type_name("Xform").unwrap();
        stage
    }

    fn hidden(world: &World, map: &PrimEntities, path: &str) -> bool {
        let e = map.entity(path).unwrap();
        matches!(world.get::<Visibility>(e), Some(Visibility::Hidden))
    }

    #[test]
    fn default_shows_proxy_hides_render_and_guide() {
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        world.insert_resource(DisplayPurposes::default());
        let live = LiveStage::new(purpose_stage());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        assert!(!hidden(&world, &map, "/Plain"), "default purpose shown");
        assert!(!hidden(&world, &map, "/Proxy"), "proxy shown by default");
        assert!(hidden(&world, &map, "/Render"), "render hidden by default");
        assert!(hidden(&world, &map, "/Guide"), "guide hidden by default");
        // Inherited: child of a proxy Scope resolves to proxy → shown.
        assert!(!hidden(&world, &map, "/Grp/Child"), "inherits proxy → shown");
        // The effective purpose is carried on the entity.
        let child = map.entity("/Grp/Child").unwrap();
        assert_eq!(world.get::<UsdPurpose>(child).unwrap().0, "proxy");
    }

    #[test]
    fn render_toggle_swaps_proxy_and_render() {
        let mut world = World::new();
        world.insert_resource(SchemaRegistry::builtin());
        // Render-quality viewport: show render, hide proxy.
        world.insert_resource(DisplayPurposes {
            render: true,
            proxy: false,
            guide: false,
        });
        let live = LiveStage::new(purpose_stage());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        assert!(!hidden(&world, &map, "/Render"), "render shown when toggled on");
        assert!(hidden(&world, &map, "/Proxy"), "proxy hidden when toggled off");
        assert!(!hidden(&world, &map, "/Plain"), "default always shown");
    }

    #[test]
    fn purpose_patch_does_not_rebuild_mesh_assets() {
        let stage = Stage::builder().in_memory("mesh-purpose.usda").unwrap();
        stage
            .define_prim("/Mesh")
            .unwrap()
            .set_type_name("Mesh")
            .unwrap();
        stage
            .create_attribute("/Mesh.points", "point3f[]")
            .unwrap()
            .set(openusd::sdf::Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 0.0, 0.0].into(),
                [0.0, 1.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/Mesh.faceVertexCounts", "int[]")
            .unwrap()
            .set(openusd::sdf::Value::IntVec(vec![3]))
            .unwrap();
        stage
            .create_attribute("/Mesh.faceVertexIndices", "int[]")
            .unwrap()
            .set(openusd::sdf::Value::IntVec(vec![0, 1, 2]))
            .unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let entity = map.entity("/Mesh").unwrap();
        let before_handle = world.get::<Mesh3d>(entity).unwrap().0.clone();
        let before_count = world.resource::<Assets<Mesh>>().len();
        let registry = SchemaRegistry::builtin();
        registry.patch_prim(
            &live.stage,
            &openusd::sdf::path("/Mesh").unwrap(),
            &mut world,
            entity,
            &["purpose"],
        );
        assert_eq!(world.get::<Mesh3d>(entity).unwrap().0, before_handle);
        assert_eq!(world.resource::<Assets<Mesh>>().len(), before_count);
    }
}
