//! Projected-mesh cache (PLAN Phase 6d): BSN's copy-on-write analog for USD.
//!
//! Every mesh-producing route builds a fresh [`Mesh`] and would otherwise
//! `Assets::add` it on each projection — so N identical prototype prims (a
//! kitbashed scene) allocate N identical GPU meshes, and re-projecting the same
//! prim mints a new handle each time. This resource interns meshes by a hash of
//! their geometry: identical content resolves to one shared [`Handle<Mesh>`].
//!
//! The cache is **opt-in**: when the resource is absent (a bare test `World`)
//! [`intern_mesh`] falls back to a plain `add`, so routes work either way.
//! [`crate::UsdPlugin`] inserts it.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};

use bevy::mesh::VertexAttributeValues;
use bevy::platform::hash::FixedHasher;
use bevy::prelude::*;

/// Upper bound on distinct interned meshes. The cache holds *strong* handles
/// (that's what keeps a shared mesh alive), so it must be bounded or a scene
/// that keeps minting distinct geometry — e.g. a time-varying `Points` prim —
/// would pin every past version alive. On overflow the map is cleared, which
/// drops those strong refs so `Assets<Mesh>` can reclaim anything unreferenced;
/// still-referenced meshes simply get re-interned on their next projection.
const MAX_INTERNED: usize = 8192;

/// Interns projected meshes by geometry signature so identical prims share one
/// [`Handle<Mesh>`]. Insert via [`crate::UsdPlugin`]; absent ⇒ no interning.
///
/// Interning is only worthwhile for geometry that repeats or persists (static
/// prototypes). Per-frame-unique geometry — notably CPU-skinned meshes, which
/// re-deform every time code — deliberately bypasses the cache (see
/// [`intern_mesh`]'s callers) so it neither bloats the map nor pins dead meshes.
#[derive(Resource, Default)]
pub struct ProjectionCache {
    meshes: HashMap<u64, Handle<Mesh>>,
}

impl ProjectionCache {
    /// Number of distinct meshes currently interned.
    pub fn len(&self) -> usize {
        self.meshes.len()
    }

    /// Whether the cache holds no interned meshes.
    pub fn is_empty(&self) -> bool {
        self.meshes.is_empty()
    }
}

/// Add `mesh` to `Assets<Mesh>`, reusing an existing handle when a mesh with
/// identical geometry was already interned this session. Falls back to a plain
/// `add` when there is no [`ProjectionCache`] resource.
pub fn intern_mesh(world: &mut World, mesh: Mesh) -> Handle<Mesh> {
    // No cache resource → behave exactly like `Assets::add`.
    if world.get_resource::<ProjectionCache>().is_none() {
        return world.resource_mut::<Assets<Mesh>>().add(mesh);
    }
    let sig = mesh_signature(&mesh);
    if let Some(existing) = world
        .resource::<ProjectionCache>()
        .meshes
        .get(&sig)
        .cloned()
    {
        // Only reuse if the asset is still alive (not unloaded out from under us).
        if world.resource::<Assets<Mesh>>().contains(&existing) {
            return existing;
        }
    }
    let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
    let mut cache = world.resource_mut::<ProjectionCache>();
    // Bound memory: clearing drops the strong handles so unreferenced meshes are
    // reclaimable. A stale (dead-handle) entry we passed over above also gets
    // swept here rather than lingering.
    if cache.meshes.len() >= MAX_INTERNED {
        cache.meshes.clear();
    }
    cache.meshes.insert(sig, handle.clone());
    handle
}

/// A 64-bit signature over the geometry that defines a mesh's appearance:
/// topology, indices, and every attribute the mesh builder emits
/// (position / normal / uv / vertex color). Two meshes with equal signatures
/// render identically, so they can share one handle. Float lanes are hashed by
/// bit pattern (exact-equality — no fuzzy matching). Any attribute added here
/// MUST be one the builder actually produces, else the signature is stable but
/// blind to a difference and two visually distinct meshes could alias.
fn mesh_signature(mesh: &Mesh) -> u64 {
    let mut h = FixedHasher.build_hasher();
    std::mem::discriminant(&mesh.primitive_topology()).hash(&mut h);
    match mesh.indices() {
        Some(bevy::mesh::Indices::U16(v)) => {
            0u8.hash(&mut h);
            v.hash(&mut h);
        }
        Some(bevy::mesh::Indices::U32(v)) => {
            1u8.hash(&mut h);
            v.hash(&mut h);
        }
        None => 2u8.hash(&mut h),
    }
    for id in [
        Mesh::ATTRIBUTE_POSITION.id,
        Mesh::ATTRIBUTE_NORMAL.id,
        Mesh::ATTRIBUTE_UV_0.id,
        Mesh::ATTRIBUTE_COLOR.id,
    ] {
        hash_attribute(mesh, id, &mut h);
    }
    h.finish()
}

fn hash_attribute(mesh: &Mesh, id: bevy::mesh::MeshVertexAttributeId, h: &mut impl Hasher) {
    let Some(values) = mesh
        .attributes()
        .find(|(attr, _)| attr.id == id)
        .map(|(_, v)| v)
    else {
        0u8.hash(h);
        return;
    };
    match values {
        VertexAttributeValues::Float32x3(v) => {
            for a in v {
                for f in a {
                    f.to_bits().hash(h);
                }
            }
        }
        VertexAttributeValues::Float32x2(v) => {
            for a in v {
                for f in a {
                    f.to_bits().hash(h);
                }
            }
        }
        VertexAttributeValues::Float32x4(v) => {
            for a in v {
                for f in a {
                    f.to_bits().hash(h);
                }
            }
        }
        _ => {
            // Attribute present but a variant we don't fold in: fold its length
            // so meshes differing only there still get distinct signatures.
            values.len().hash(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::PrimitiveTopology;
    use openusd::usd::Stage;

    /// The signature must fold in vertex color and be deterministic: same
    /// content → same hash (guards against attribute-iteration-order flakiness),
    /// different color → different hash (so recoloured geometry isn't aliased).
    #[test]
    fn signature_folds_color_and_is_deterministic() {
        let mk = |color: [f32; 4]| {
            let mut m = Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default());
            m.insert_attribute(
                Mesh::ATTRIBUTE_POSITION,
                vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            );
            m.insert_attribute(Mesh::ATTRIBUTE_COLOR, vec![color; 3]);
            m
        };
        let red = mesh_signature(&mk([1.0, 0.0, 0.0, 1.0]));
        let blue = mesh_signature(&mk([0.0, 0.0, 1.0, 1.0]));
        assert_ne!(red, blue, "vertex color must affect the signature");
        assert_eq!(
            red,
            mesh_signature(&mk([1.0, 0.0, 0.0, 1.0])),
            "identical meshes must hash identically"
        );
    }

    #[test]
    fn identical_prims_share_one_mesh() {
        // Two Cube prims of the same size → one interned mesh handle.
        let stage = Stage::builder().in_memory("cache.usda").unwrap();
        for name in ["/A", "/B"] {
            stage.define_prim(name).unwrap().set_type_name("Cube").unwrap();
        }

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(ProjectionCache::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let a = world.get::<Mesh3d>(map.entity("/A").unwrap()).unwrap().0.clone();
        let b = world.get::<Mesh3d>(map.entity("/B").unwrap()).unwrap().0.clone();
        assert_eq!(a, b, "identical cubes share one interned mesh handle");
        assert_eq!(world.resource::<ProjectionCache>().len(), 1, "one distinct mesh");
    }

    #[test]
    fn differing_geometry_is_not_aliased() {
        // Two cubes of *different* size must get distinct handles — the
        // signature must not collide across genuinely different geometry.
        let stage = Stage::builder().in_memory("cache2.usda").unwrap();
        for (name, size) in [("/Small", 1.0f32), ("/Big", 4.0f32)] {
            stage.define_prim(name).unwrap().set_type_name("Cube").unwrap();
            stage
                .create_attribute(format!("{name}.size").as_str(), "double")
                .unwrap()
                .set(openusd::sdf::Value::Double(size as f64))
                .unwrap();
        }

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(ProjectionCache::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let s = world.get::<Mesh3d>(map.entity("/Small").unwrap()).unwrap().0.clone();
        let b = world.get::<Mesh3d>(map.entity("/Big").unwrap()).unwrap().0.clone();
        assert_ne!(s, b, "different-sized cubes must not share a mesh");
        assert_eq!(world.resource::<ProjectionCache>().len(), 2, "two distinct meshes");
    }

    #[test]
    fn no_cache_resource_falls_back_to_plain_add() {
        // Without the resource, interning must still produce working meshes.
        let stage = Stage::builder().in_memory("cache3.usda").unwrap();
        stage.define_prim("/A").unwrap().set_type_name("Cube").unwrap();
        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        assert!(
            world.get::<Mesh3d>(map.entity("/A").unwrap()).is_some(),
            "mesh still attaches with no ProjectionCache present"
        );
    }
}
