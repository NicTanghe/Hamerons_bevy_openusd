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
    let hidden = invisible || !purposes.shows(&purpose);
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
        let availability = super::cache::request_usd_mesh(world, entity, ctx.prim_str(), read);
        let needs_material = world
            .get::<MeshMaterial3d<StandardMaterial>>(entity)
            .is_none();
        let has_asset_server = world.get_resource::<AssetServer>().is_some();
        let material = needs_material.then(|| {
            let read = crate::read::shade::ReadPreviewMaterial::default();
            super::cache::intern_preview_material(
                world,
                &read,
                has_asset_server,
                StandardMaterial::default(),
            )
        });
        if let Ok(mut e) = world.get_entity_mut(entity) {
            if let super::cache::MeshAvailability::Ready(mesh_handle) = availability {
                e.insert(Mesh3d(mesh_handle));
            }
            if let Some(material) = material {
                e.insert(MeshMaterial3d(material));
            }
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
        if changed.is_empty() || changed.iter().any(|name| mesh_property(name)) {
            self.attach(ctx, world, entity);
        }
    }
}

fn mesh_property(name: &str) -> bool {
    matches!(
        name,
        "points"
            | "faceVertexCounts"
            | "faceVertexIndices"
            | "orientation"
            | "normals"
            | "subdivisionScheme"
    ) || name.starts_with("primvars:normals")
        || name.starts_with("primvars:st")
        || name.starts_with("primvars:displayColor")
        || name.starts_with("primvars:displayOpacity")
}

#[cfg(test)]
mod purpose_tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::{DisplayPurposes, SchemaRegistry};
    use openusd::usd::Stage;

    #[test]
    fn mesh_patch_ignores_purpose_and_accepts_geometry_properties() {
        assert!(!mesh_property("purpose"));
        assert!(!mesh_property("visibility"));
        assert!(mesh_property("points"));
        assert!(mesh_property("faceVertexIndices"));
        assert!(mesh_property("primvars:st:indices"));
        assert!(mesh_property("primvars:displayColor"));
    }

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
}
