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
#[cfg(not(target_arch = "wasm32"))]
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};

use crate::read::geom::{Interpolation, MeshPrimvar, Orientation, ReadMesh};
use crate::read::shade::ReadPreviewMaterial;

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
    /// Content signatures for already-built Bevy meshes produced by shapes,
    /// curves, and points routes.
    meshes: HashMap<u64, Handle<Mesh>>,
    /// Signatures over raw USD mesh inputs. Looking here before baking avoids
    /// repeating triangulation and tangent generation for composed references.
    usd_meshes: HashMap<u64, Handle<Mesh>>,
    materials: HashMap<u64, Handle<StandardMaterial>>,
    #[cfg(not(target_arch = "wasm32"))]
    pending_usd_meshes: HashMap<u64, PendingUsdMesh>,
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingUsdMesh {
    task: Task<Mesh>,
    entities: Vec<Entity>,
    label: String,
}

/// Marks which asynchronous bake result an entity is currently waiting for.
/// A later edit replaces the signature, preventing stale jobs from attaching.
#[derive(Component)]
struct RequestedUsdMesh(u64);

/// Result of requesting a USD mesh bake.
pub enum MeshAvailability {
    Ready(Handle<Mesh>),
    Pending,
}

impl ProjectionCache {
    /// Number of distinct meshes currently interned.
    pub fn len(&self) -> usize {
        self.meshes.len() + self.usd_meshes.len()
    }

    /// Whether the cache holds no interned meshes.
    pub fn is_empty(&self) -> bool {
        self.meshes.is_empty() && self.usd_meshes.is_empty()
    }
}

/// Return an existing converted mesh or start one conversion for this exact
/// USD geometry. On native builds the expensive bake runs on Bevy's async
/// compute pool when it is available. Web and bare test worlds fall back to a
/// synchronous bake.
pub fn request_usd_mesh(
    world: &mut World,
    entity: Entity,
    label: &str,
    read: ReadMesh,
) -> MeshAvailability {
    let signature = usd_mesh_signature(&read);

    if world.get_resource::<ProjectionCache>().is_none() {
        let mesh = crate::mesh::mesh_from_usd(&read);
        return MeshAvailability::Ready(intern_mesh(world, mesh));
    }

    let existing = world
        .resource::<ProjectionCache>()
        .usd_meshes
        .get(&signature)
        .cloned();
    if let Some(handle) = existing
        && world.resource::<Assets<Mesh>>().contains(&handle)
    {
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.remove::<RequestedUsdMesh>();
        }
        return MeshAvailability::Ready(handle);
    }

    #[cfg(not(target_arch = "wasm32"))]
    if let Some(pool) = AsyncComputeTaskPool::try_get() {
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert(RequestedUsdMesh(signature));
        }
        let mut cache = world.resource_mut::<ProjectionCache>();
        if let Some(pending) = cache.pending_usd_meshes.get_mut(&signature) {
            if !pending.entities.contains(&entity) {
                pending.entities.push(entity);
            }
        } else {
            let task = pool.spawn(async move { crate::mesh::mesh_from_usd(&read) });
            cache.pending_usd_meshes.insert(
                signature,
                PendingUsdMesh {
                    task,
                    entities: vec![entity],
                    label: label.to_string(),
                },
            );
        }
        return MeshAvailability::Pending;
    }

    let mesh = crate::mesh::mesh_from_usd(&read);
    let handle = add_usd_mesh(world, signature, mesh);
    MeshAvailability::Ready(handle)
}

/// Poll native conversion jobs without blocking. A small completion budget
/// prevents a frame spike when many workers finish together.
#[cfg(not(target_arch = "wasm32"))]
pub fn apply_completed_usd_meshes(world: &mut World) {
    const MAX_COMPLETIONS_PER_FRAME: usize = 4;

    if world.get_resource::<ProjectionCache>().is_none()
        || world.get_resource::<Assets<Mesh>>().is_none()
    {
        return;
    }

    // Despawns, reloads, or newer edits invalidate old waiters. Dropping a
    // task with no remaining waiter cancels needless work where possible.
    let live_requests: HashMap<Entity, u64> = {
        let mut query = world.query::<(Entity, &RequestedUsdMesh)>();
        query
            .iter(world)
            .map(|(entity, request)| (entity, request.0))
            .collect()
    };
    world
        .resource_mut::<ProjectionCache>()
        .pending_usd_meshes
        .retain(|signature, pending| {
            pending
                .entities
                .retain(|entity| live_requests.get(entity).copied() == Some(*signature));
            !pending.entities.is_empty()
        });

    let keys: Vec<u64> = world
        .resource::<ProjectionCache>()
        .pending_usd_meshes
        .keys()
        .copied()
        .collect();
    let mut completed = Vec::new();
    for signature in keys {
        if completed.len() >= MAX_COMPLETIONS_PER_FRAME {
            break;
        }
        let ready = {
            let mut cache = world.resource_mut::<ProjectionCache>();
            let Some(pending) = cache.pending_usd_meshes.get_mut(&signature) else {
                continue;
            };
            block_on(poll_once(&mut pending.task))
        };
        if let Some(mesh) = ready {
            let pending = world
                .resource_mut::<ProjectionCache>()
                .pending_usd_meshes
                .remove(&signature)
                .expect("completed USD mesh task remains registered");
            completed.push((signature, mesh, pending.entities, pending.label));
        }
    }

    for (signature, mesh, entities, label) in completed {
        let handle = add_usd_mesh(world, signature, mesh);
        let mut attached = 0usize;
        for entity in entities {
            let current = world
                .get::<RequestedUsdMesh>(entity)
                .is_some_and(|request| request.0 == signature);
            if !current {
                continue;
            }
            if let Ok(mut e) = world.get_entity_mut(entity) {
                e.insert(Mesh3d(handle.clone()));
                e.remove::<RequestedUsdMesh>();
                attached += 1;
            }
        }
        bevy::log::trace!(
            target: "usd_bevy::route::geom",
            "{label}: async mesh ready -> {attached} instance(s)"
        );
    }
}

/// Web builds intentionally retain synchronous conversion.
#[cfg(target_arch = "wasm32")]
pub fn apply_completed_usd_meshes(_world: &mut World) {}

fn add_usd_mesh(world: &mut World, signature: u64, mesh: Mesh) -> Handle<Mesh> {
    let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
    let mut cache = world.resource_mut::<ProjectionCache>();
    if cache.meshes.len() + cache.usd_meshes.len() >= MAX_INTERNED {
        cache.meshes.clear();
        cache.usd_meshes.clear();
    }
    cache.usd_meshes.insert(signature, handle.clone());
    handle
}

/// Intern a decoded preview material so repeated USD references retain the
/// same Bevy material handle and remain eligible for renderer instancing.
pub fn intern_preview_material(
    world: &mut World,
    read: &ReadPreviewMaterial,
    has_asset_server: bool,
    material: StandardMaterial,
) -> Handle<StandardMaterial> {
    if world.get_resource::<ProjectionCache>().is_none() {
        return world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(material);
    }
    let signature = preview_material_signature(read, has_asset_server);
    let existing = world
        .resource::<ProjectionCache>()
        .materials
        .get(&signature)
        .cloned();
    if let Some(handle) = existing
        && world
            .resource::<Assets<StandardMaterial>>()
            .contains(&handle)
    {
        return handle;
    }
    let handle = world
        .resource_mut::<Assets<StandardMaterial>>()
        .add(material);
    let mut cache = world.resource_mut::<ProjectionCache>();
    if cache.materials.len() >= MAX_INTERNED {
        cache.materials.clear();
    }
    cache.materials.insert(signature, handle.clone());
    handle
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

/// Signature the inputs that affect [`crate::mesh::mesh_from_usd`]. This is
/// deliberately computed before mesh construction so identical composed USD
/// references can share one pending or completed bake.
fn usd_mesh_signature(read: &ReadMesh) -> u64 {
    let mut h = FixedHasher.build_hasher();
    hash_f32x3_slice(&read.points, &mut h);
    read.face_vertex_counts.hash(&mut h);
    read.face_vertex_indices.hash(&mut h);
    hash_vec3_primvar(read.normals.as_ref(), &mut h);
    hash_vec2_primvar(read.uvs.as_ref(), &mut h);
    hash_orientation(read.orientation, &mut h);
    hash_vec3_primvar(read.display_color.as_ref(), &mut h);
    hash_float_primvar(read.display_opacity.as_ref(), &mut h);
    h.finish()
}

fn hash_interpolation(value: Interpolation, h: &mut impl Hasher) {
    let tag = match value {
        Interpolation::Constant => 0u8,
        Interpolation::Uniform => 1,
        Interpolation::Varying => 2,
        Interpolation::Vertex => 3,
        Interpolation::FaceVarying => 4,
    };
    tag.hash(h);
}

fn hash_orientation(value: Orientation, h: &mut impl Hasher) {
    match value {
        Orientation::RightHanded => 0u8,
        Orientation::LeftHanded => 1u8,
    }
    .hash(h);
}

fn hash_f32_slice(values: &[f32], h: &mut impl Hasher) {
    values.len().hash(h);
    for value in values {
        value.to_bits().hash(h);
    }
}

fn hash_f32x2_slice(values: &[[f32; 2]], h: &mut impl Hasher) {
    values.len().hash(h);
    for value in values {
        for lane in value {
            lane.to_bits().hash(h);
        }
    }
}

fn hash_f32x3_slice(values: &[[f32; 3]], h: &mut impl Hasher) {
    values.len().hash(h);
    for value in values {
        for lane in value {
            lane.to_bits().hash(h);
        }
    }
}

fn hash_vec2_primvar(value: Option<&MeshPrimvar<[f32; 2]>>, h: &mut impl Hasher) {
    match value {
        None => 0u8.hash(h),
        Some(value) => {
            1u8.hash(h);
            hash_interpolation(value.interpolation, h);
            hash_f32x2_slice(&value.values, h);
            value.indices.hash(h);
        }
    }
}

fn hash_vec3_primvar(value: Option<&MeshPrimvar<[f32; 3]>>, h: &mut impl Hasher) {
    match value {
        None => 0u8.hash(h),
        Some(value) => {
            1u8.hash(h);
            hash_interpolation(value.interpolation, h);
            hash_f32x3_slice(&value.values, h);
            value.indices.hash(h);
        }
    }
}

fn hash_float_primvar(value: Option<&MeshPrimvar<f32>>, h: &mut impl Hasher) {
    match value {
        None => 0u8.hash(h),
        Some(value) => {
            1u8.hash(h);
            hash_interpolation(value.interpolation, h);
            hash_f32_slice(&value.values, h);
            value.indices.hash(h);
        }
    }
}

fn preview_material_signature(read: &ReadPreviewMaterial, has_asset_server: bool) -> u64 {
    let mut h = FixedHasher.build_hasher();
    has_asset_server.hash(&mut h);
    hash_optional_f32x3(read.diffuse_color, &mut h);
    hash_optional_f32(read.opacity, &mut h);
    hash_optional_f32(read.opacity_threshold, &mut h);
    hash_optional_f32(read.roughness, &mut h);
    hash_optional_f32(read.metallic, &mut h);
    hash_optional_f32x3(read.emissive_color, &mut h);
    hash_optional_f32(read.ior, &mut h);
    read.diffuse_texture.hash(&mut h);
    read.normal_texture.hash(&mut h);
    read.roughness_texture.hash(&mut h);
    read.metallic_texture.hash(&mut h);
    read.opacity_texture.hash(&mut h);
    read.emissive_texture.hash(&mut h);
    read.occlusion_texture.hash(&mut h);
    match read.uv_transform {
        None => 0u8.hash(&mut h),
        Some(uv) => {
            1u8.hash(&mut h);
            hash_f32x2_slice(&[uv.translation, uv.scale], &mut h);
            uv.rotation_deg.to_bits().hash(&mut h);
        }
    }
    h.finish()
}

fn hash_optional_f32(value: Option<f32>, h: &mut impl Hasher) {
    match value {
        None => 0u8.hash(h),
        Some(value) => {
            1u8.hash(h);
            value.to_bits().hash(h);
        }
    }
}

fn hash_optional_f32x3(value: Option<[f32; 3]>, h: &mut impl Hasher) {
    match value {
        None => 0u8.hash(h),
        Some(value) => {
            1u8.hash(h);
            for lane in value {
                lane.to_bits().hash(h);
            }
        }
    }
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
    use crate::read::geom::SubdivScheme;
    use crate::route::SchemaRegistry;
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::PrimitiveTopology;
    use openusd::usd::Stage;

    fn triangle(offset: f32) -> ReadMesh {
        ReadMesh {
            points: vec![
                [offset, 0.0, 0.0],
                [offset + 1.0, 0.0, 0.0],
                [offset, 1.0, 0.0],
            ],
            face_vertex_counts: vec![3],
            face_vertex_indices: vec![0, 1, 2],
            normals: Some(MeshPrimvar {
                values: vec![[0.0, 0.0, 1.0]; 3],
                interpolation: Interpolation::Vertex,
                indices: Vec::new(),
            }),
            uvs: Some(MeshPrimvar {
                values: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                interpolation: Interpolation::Vertex,
                indices: Vec::new(),
            }),
            orientation: Orientation::RightHanded,
            display_color: None,
            display_opacity: None,
            subsets: Vec::new(),
            double_sided: false,
            extent: None,
            subdivision_scheme: SubdivScheme::None,
        }
    }

    #[test]
    fn raw_usd_signature_deduplicates_before_baking() {
        let a = triangle(0.0);
        let b = a.clone();
        let c = triangle(2.0);
        assert_eq!(usd_mesh_signature(&a), usd_mesh_signature(&b));
        assert_ne!(usd_mesh_signature(&a), usd_mesh_signature(&c));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn duplicate_async_requests_share_one_bake_and_handle() {
        use bevy::tasks::TaskPoolBuilder;

        AsyncComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(2).build());
        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(ProjectionCache::default());
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();

        assert!(matches!(
            request_usd_mesh(&mut world, a, "/A", triangle(0.0)),
            MeshAvailability::Pending
        ));
        assert!(matches!(
            request_usd_mesh(&mut world, b, "/B", triangle(0.0)),
            MeshAvailability::Pending
        ));
        let pending = &world.resource::<ProjectionCache>().pending_usd_meshes;
        assert_eq!(pending.len(), 1, "one conversion for identical inputs");
        assert_eq!(pending.values().next().unwrap().entities.len(), 2);

        for _ in 0..10_000 {
            apply_completed_usd_meshes(&mut world);
            if world.get::<Mesh3d>(a).is_some() && world.get::<Mesh3d>(b).is_some() {
                break;
            }
            std::thread::yield_now();
        }
        let a_handle = world.get::<Mesh3d>(a).expect("first mesh completed");
        let b_handle = world.get::<Mesh3d>(b).expect("second mesh completed");
        assert_eq!(a_handle.0, b_handle.0);
        assert_eq!(world.resource::<Assets<Mesh>>().len(), 1);
        assert_eq!(world.resource::<ProjectionCache>().len(), 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn stale_async_result_does_not_replace_newer_geometry() {
        use bevy::tasks::TaskPoolBuilder;

        AsyncComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(2).build());
        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(ProjectionCache::default());
        let entity = world.spawn_empty().id();

        let _ = request_usd_mesh(&mut world, entity, "/Old", triangle(0.0));
        let _ = request_usd_mesh(&mut world, entity, "/New", triangle(10.0));

        for _ in 0..10_000 {
            apply_completed_usd_meshes(&mut world);
            if world
                .resource::<ProjectionCache>()
                .pending_usd_meshes
                .is_empty()
            {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            world
                .resource::<ProjectionCache>()
                .pending_usd_meshes
                .is_empty(),
            "both conversion jobs completed"
        );
        let handle = &world.get::<Mesh3d>(entity).expect("new mesh attached").0;
        let mesh = world.resource::<Assets<Mesh>>().get(handle).unwrap();
        let Some(VertexAttributeValues::Float32x3(positions)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        else {
            panic!("positions");
        };
        assert_eq!(positions[0], [10.0, 0.0, 0.0]);
    }

    #[test]
    fn identical_preview_materials_share_one_handle() {
        let mut world = World::new();
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(ProjectionCache::default());
        let read = ReadPreviewMaterial {
            diffuse_color: Some([0.2, 0.4, 0.6]),
            roughness: Some(0.3),
            ..Default::default()
        };
        let a = intern_preview_material(&mut world, &read, false, StandardMaterial::default());
        let b = intern_preview_material(&mut world, &read, false, StandardMaterial::default());
        assert_eq!(a, b);
        assert_eq!(world.resource::<Assets<StandardMaterial>>().len(), 1);
    }

    /// The signature must fold in vertex color and be deterministic: same
    /// content → same hash (guards against attribute-iteration-order flakiness),
    /// different color → different hash (so recoloured geometry isn't aliased).
    #[test]
    fn signature_folds_color_and_is_deterministic() {
        let mk = |color: [f32; 4]| {
            let mut m = Mesh::new(
                PrimitiveTopology::TriangleList,
                RenderAssetUsages::default(),
            );
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
            stage
                .define_prim(name)
                .unwrap()
                .set_type_name("Cube")
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

        let a = world
            .get::<Mesh3d>(map.entity("/A").unwrap())
            .unwrap()
            .0
            .clone();
        let b = world
            .get::<Mesh3d>(map.entity("/B").unwrap())
            .unwrap()
            .0
            .clone();
        assert_eq!(a, b, "identical cubes share one interned mesh handle");
        assert_eq!(
            world.resource::<ProjectionCache>().len(),
            1,
            "one distinct mesh"
        );
    }

    #[test]
    fn differing_geometry_is_not_aliased() {
        // Two cubes of *different* size must get distinct handles — the
        // signature must not collide across genuinely different geometry.
        let stage = Stage::builder().in_memory("cache2.usda").unwrap();
        for (name, size) in [("/Small", 1.0f32), ("/Big", 4.0f32)] {
            stage
                .define_prim(name)
                .unwrap()
                .set_type_name("Cube")
                .unwrap();
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

        let s = world
            .get::<Mesh3d>(map.entity("/Small").unwrap())
            .unwrap()
            .0
            .clone();
        let b = world
            .get::<Mesh3d>(map.entity("/Big").unwrap())
            .unwrap()
            .0
            .clone();
        assert_ne!(s, b, "different-sized cubes must not share a mesh");
        assert_eq!(
            world.resource::<ProjectionCache>().len(),
            2,
            "two distinct meshes"
        );
    }

    #[test]
    fn no_cache_resource_falls_back_to_plain_add() {
        // Without the resource, interning must still produce working meshes.
        let stage = Stage::builder().in_memory("cache3.usda").unwrap();
        stage
            .define_prim("/A")
            .unwrap()
            .set_type_name("Cube")
            .unwrap();
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
