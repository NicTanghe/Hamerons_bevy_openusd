//! Skinning route (PLAN in-repo): a skinned `Mesh` → its CPU-deformed geometry
//! at the current [`StageTime`](super::StageTime).
//!
//! Runs after the mesh route (which bakes the rest mesh) and replaces the
//! entity's `Mesh3d` with the skinned points computed by [`read::skel`]. Being
//! a normal route, it re-runs on reproject and — because a skinned mesh is
//! flagged animated (see `crate::live::prim_is_animated`) — it resamples as
//! `StageTime` moves.

use bevy::prelude::*;

use super::{PrimRoute, RouteCtx};
use crate::read::skel::{blend_shaped_points_at, has_blend_shapes, is_skinned, skinned_points_at};

/// Replaces a skinned / blend-shaped mesh's geometry with its deformed points.
pub struct SkinRoute;

impl PrimRoute for SkinRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        matches!(ctx.type_name.as_deref(), Some("Mesh"))
            && (is_skinned(ctx.stage, ctx.path) || has_blend_shapes(ctx.stage, ctx.path))
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        if world.get_resource::<Assets<Mesh>>().is_none() {
            return;
        }
        // Skinned meshes morph (blend shapes) then skin; a mesh with only blend
        // shapes just morphs.
        let deformed = if is_skinned(ctx.stage, ctx.path) {
            skinned_points_at(ctx.stage, ctx.path, ctx.time)
        } else {
            blend_shaped_points_at(ctx.stage, ctx.path, ctx.time)
        };
        let Ok(Some(points)) = deformed else {
            return;
        };
        // Rebuild with the mesh's own topology/normals/uvs but deformed points.
        let Ok(Some(mut read)) = crate::read::geom::read_mesh(ctx.stage, ctx.path) else {
            return;
        };
        read.points = points;
        // Skinning invalidates authored normals; drop them so the mesh builder
        // recomputes flat normals from the deformed positions.
        read.normals = None;
        let mesh = crate::mesh::mesh_from_usd(&read);
        // Skinned geometry re-deforms every time code, so each result is unique;
        // interning it would only bloat the cache and pin dead meshes alive. Add
        // it directly and let the old deformed mesh be reclaimed on replacement.
        let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
        if let Some(mut m) = world.get_mut::<Mesh3d>(entity) {
            m.0 = handle;
        } else if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(Mesh3d(handle));
        }
    }
}
