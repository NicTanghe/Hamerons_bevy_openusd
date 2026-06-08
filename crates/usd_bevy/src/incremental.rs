//! Incremental, prim-keyed scene updates.
//!
//! Instead of rebuilding the whole scene when something changes, apply the
//! change to the *live entities* keyed by prim path (`UsdPrimRef`). The first
//! path implemented here is material/texture **variant switching**: recompose
//! the stage with the new selection, rebuild only the affected meshes'
//! materials, and reassign `MeshMaterial3d` in place — no re-tessellation, no
//! respawn, no metadata passes. Results are cached per `(prim, set, option)`
//! so re-selecting an option is instant.
//!
//! This is the seed of a general retained-mode projection: the same
//! "find affected entities by path, update in place" shape extends to
//! transforms / meshes, and pairs with a future change-notification layer
//! (which would supply the affected-path set directly).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use bevy::mesh::Mesh3d;
use bevy::pbr::{MeshMaterial3d, StandardMaterial};
use bevy::prelude::*;

use crate::asset::{VariantSelection, author_variant_session_layer};
use crate::material::standard_material_from_usd;
use crate::prim_ref::UsdPrimRef;
use crate::read::{geom as ugeom, shade as ushade};
use crate::texture::AssetServerTextures;

/// The loaded stage's source layer + current variant selections, so the
/// incremental updater can recompose with a new selection. The viewer sets
/// this on every (re)load.
#[derive(Resource, Default, Clone)]
pub struct LoadedStageSource {
    /// Absolute path to the root layer.
    pub source: PathBuf,
    /// Search root (the source layer's directory).
    pub root: PathBuf,
    /// All currently-applied variant selections.
    pub base_variants: Vec<VariantSelection>,
}

/// Variant switches to apply incrementally — `(prim_path, set_name, option)`.
/// The viewer pushes here instead of triggering a full reload.
#[derive(Resource, Default)]
pub struct PendingVariantSwitch {
    pub queue: Vec<(String, String, String)>,
}

/// Set when an incremental switch isn't possible (geometry changed, or the
/// recompose/read failed). The viewer watches this and falls back to a full
/// reload, then resets it.
#[derive(Resource, Default)]
pub struct VariantReloadFallback(pub bool);

#[derive(Resource, Default)]
struct MaterialVariantCache(HashMap<(String, String, String), Vec<(String, Handle<StandardMaterial>)>>);

/// Registers the incremental-update resources + system.
pub struct IncrementalPlugin;

impl Plugin for IncrementalPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LoadedStageSource>()
            .init_resource::<PendingVariantSwitch>()
            .init_resource::<VariantReloadFallback>()
            .init_resource::<MaterialVariantCache>()
            .add_systems(Update, apply_variant_switch);
    }
}

type MeshMatQuery<'w, 's> =
    Query<'w, 's, (&'static UsdPrimRef, &'static Mesh3d, &'static mut MeshMaterial3d<StandardMaterial>)>;

fn apply_variant_switch(
    mut pending: ResMut<PendingVariantSwitch>,
    source: Res<LoadedStageSource>,
    mut fallback: ResMut<VariantReloadFallback>,
    mut cache: ResMut<MaterialVariantCache>,
    asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    meshes: Res<Assets<bevy::mesh::Mesh>>,
    mut q: MeshMatQuery,
) {
    if pending.queue.is_empty() {
        return;
    }
    for (prim_path, set_name, option) in std::mem::take(&mut pending.queue) {
        let key = (prim_path.clone(), set_name.clone(), option.clone());

        // Cached → instant reassign, no recompose.
        if let Some(per_mesh) = cache.0.get(&key).cloned() {
            reassign(&mut q, &per_mesh);
            info!("variant: cached live-swap {prim_path} {set_name}={option}");
            continue;
        }

        let Some(stage) = open_with_variant(&source) else {
            warn!("variant: recompose failed for {set_name}={option}; falling back to reload");
            fallback.0 = true;
            continue;
        };

        // Read each live mesh's bound material in the recomposed stage, detect
        // geometry change, and build the new material.
        let search = [source.root.clone()];
        let mut tex = AssetServerTextures {
            asset_server: &asset_server,
            search_paths: &search,
        };
        let mut per_mesh: Vec<(String, Handle<StandardMaterial>)> = Vec::new();
        let mut geometry_changed = false;
        for (pref, mesh3d, _) in q.iter() {
            let Ok(ppath) = openusd::sdf::path(&pref.path) else {
                continue;
            };
            // Geometry guard: if the mesh's vertex count changed, this isn't a
            // pure material variant — bail to a full reload.
            if let (Some(live), Ok(Some(rm))) =
                (meshes.get(&mesh3d.0), ugeom::read_mesh(&stage, &ppath))
                && rm.points.len() != live.count_vertices()
            {
                geometry_changed = true;
                break;
            }
            let Ok(Some(mat_prim)) = ushade::read_material_binding(&stage, &ppath) else {
                continue;
            };
            let Ok(Some(read)) = ushade::read_preview_material(&stage, &mat_prim) else {
                continue;
            };
            let handle = materials.add(standard_material_from_usd(&mut tex, &read));
            per_mesh.push((pref.path.clone(), handle));
        }

        if geometry_changed {
            warn!("variant: geometry changed for {set_name}={option}; falling back to reload");
            fallback.0 = true;
            continue;
        }

        reassign(&mut q, &per_mesh);
        info!("variant: live-applied {prim_path} {set_name}={option} ({} meshes)", per_mesh.len());
        cache.0.insert(key, per_mesh);
    }
}

fn reassign(q: &mut MeshMatQuery, per_mesh: &[(String, Handle<StandardMaterial>)]) {
    for (path, handle) in per_mesh {
        for (pref, _, mut mat) in q.iter_mut() {
            if &pref.path == path {
                mat.0 = handle.clone();
            }
        }
    }
}

/// Recompose the source layer with the current variant selections via a
/// session layer (openusd directly — no Bevy AssetServer needed for the stage).
fn open_with_variant(source: &LoadedStageSource) -> Option<openusd::usd::Stage> {
    let text = author_variant_session_layer(&source.base_variants);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    source.source.hash(&mut hasher);
    let tmp = std::env::temp_dir().join(format!("usd_variant_session_{:016x}.usda", hasher.finish()));
    std::fs::write(&tmp, &text).ok()?;
    openusd::usd::Stage::builder()
        .resolver(openusd::ar::DefaultResolver::with_search_paths(vec![source.root.clone()]))
        .session_layer(tmp.to_str()?.to_string())
        .open(source.source.to_str()?)
        .ok()
}
