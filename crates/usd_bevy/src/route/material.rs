//! Material route: a gprim's bound `UsdShade` Material → Bevy material.
//!
//! Runs after the mesh route, replacing the placeholder material the mesh route
//! attaches. Reads the `material:binding` and decodes the bound
//! Preview and MDL approximations use [`StandardMaterial`]. MaterialX graphs
//! compile to generated WESL and [`MaterialXMaterial`].
//! Scalar/colour channels apply headless. MaterialX texture assets use their
//! OpenUSD-resolved file paths; Preview/MDL textures still use [`AssetServer`].

use bevy::prelude::*;
use openusd::sdf::Path;
use std::collections::HashMap;

use super::{PrimRoute, RouteCtx};
use crate::materialx::compiler::{CompileFailure, compile_materialx, has_materialx_terminal};
use crate::materialx::diagnostic::{MaterialXDiagnostics, MaterialXFailure, Severity};
use crate::materialx::external::{
    MaterialXDocumentRegistry, compile_external_materialx, find_external_materialx,
};
use crate::materialx::material::{MaterialXMaterial, prepare_material};
use crate::materialx::registry::MaterialXRegistry;
use crate::read::shade::{ReadPreviewMaterial, read_material_binding, read_preview_material};

/// Maps a bound Material → the entity's [`MeshMaterial3d`].
pub struct MaterialRoute;

/// The renderer material selected for one shaded mesh.
///
/// PointInstancers bake prototype meshes before their source prim entities are
/// projected, so they use this same resolution path and share the resulting
/// handle across every visible instance.
#[derive(Clone)]
pub(crate) enum RoutedMaterial {
    Standard(Handle<StandardMaterial>),
    MaterialX(Handle<MaterialXMaterial>),
}

/// A resolved renderer material plus any compiler failure that should remain
/// inspectable on the entity using its visible magenta fallback.
#[derive(Clone)]
pub(crate) struct ResolvedMaterial {
    material: RoutedMaterial,
    failure: Option<MaterialXFailure>,
}

impl ResolvedMaterial {
    pub(crate) fn default_standard(world: &mut World) -> Self {
        let handle = world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        Self {
            material: RoutedMaterial::Standard(handle),
            failure: None,
        }
    }

    pub(crate) fn attach(self, entity: &mut EntityWorldMut<'_>) {
        match self.material {
            RoutedMaterial::Standard(handle) => {
                entity.remove::<MeshMaterial3d<MaterialXMaterial>>();
                entity.insert(MeshMaterial3d(handle));
            }
            RoutedMaterial::MaterialX(handle) => {
                entity.remove::<MeshMaterial3d<StandardMaterial>>();
                entity.insert(MeshMaterial3d(handle));
            }
        }
        if let Some(failure) = self.failure {
            entity.insert(failure);
        } else {
            entity.remove::<MaterialXFailure>();
        }
    }
}

/// The prim's decoded preview material, if it has a binding that resolves.
fn material_of(ctx: &RouteCtx, binding: &Path) -> Option<ReadPreviewMaterial> {
    read_preview_material(ctx.stage, binding).ok().flatten()
}

fn to_standard_material(
    read: &ReadPreviewMaterial,
    assets: Option<&AssetServer>,
) -> StandardMaterial {
    let mut m = StandardMaterial::default();
    if let Some(c) = read.diffuse_color {
        let a = read.opacity.unwrap_or(1.0);
        m.base_color = Color::srgba(c[0], c[1], c[2], a);
    } else if let Some(a) = read.opacity {
        m.base_color.set_alpha(a);
    }
    if read.opacity.is_some_and(|a| a < 1.0) {
        m.alpha_mode = AlphaMode::Blend;
    }
    if let Some(r) = read.roughness {
        m.perceptual_roughness = r;
    }
    if let Some(mtl) = read.metallic {
        m.metallic = mtl;
    }
    if let Some(e) = read.emissive_color {
        m.emissive = LinearRgba::rgb(e[0], e[1], e[2]);
    }
    if let Some(ior) = read.ior {
        m.ior = ior;
    }
    // UsdTransform2d on the st chain → StandardMaterial UV transform. USD applies
    // `st' = rotate(scale·st) + translation`, matching glam's T·R·S composition.
    if let Some(uv) = &read.uv_transform {
        m.uv_transform = bevy::math::Affine2::from_scale_angle_translation(
            Vec2::from(uv.scale),
            uv.rotation_deg.to_radians(),
            Vec2::from(uv.translation),
        );
    }
    // Textures — loaded through the AssetServer when one is available. Path is
    // used as authored (layer-relative resolution is a P4 follow-up).
    if let Some(server) = assets {
        if let Some(p) = &read.diffuse_texture {
            m.base_color_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.normal_texture {
            m.normal_map_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.metallic_texture {
            m.metallic_roughness_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.emissive_texture {
            m.emissive_texture = Some(server.load(p.clone()));
        }
        if let Some(p) = &read.occlusion_texture {
            m.occlusion_texture = Some(server.load(p.clone()));
        }
    }
    m
}

impl PrimRoute for MaterialRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        read_material_binding(ctx.stage, ctx.path)
            .ok()
            .flatten()
            .is_some()
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        // Prototype source meshes are intentionally not rendered directly. The
        // PointInstancer resolves their materials while baking the shared
        // visible subtree, so compiling again on the hidden source would only
        // duplicate assets and diagnostics.
        if world
            .get_resource::<super::instancer::PointInstancerPrototypeSources>()
            .is_some_and(|sources| sources.contains(ctx.prim_str()))
            && world.get::<Mesh3d>(entity).is_none()
        {
            return;
        }
        let Some(material) = resolve_material(ctx, world) else {
            return;
        };
        if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
            material.attach(&mut entity_mut);
        }
    }

    fn patch(&self, ctx: &RouteCtx, world: &mut World, entity: Entity, changed: &[&str]) {
        if changed.is_empty()
            || changed
                .iter()
                .any(|name| material_property(name) || super::geom::mesh_property(name))
        {
            self.project(ctx, world, entity);
        }
    }
}

/// Resolve a USD material binding into the exact renderer handle used by a
/// normal mesh. Kept separate from entity attachment so PointInstancer
/// prototypes follow the same Preview/MaterialX/fallback behavior.
pub(crate) fn resolve_material(ctx: &RouteCtx, world: &mut World) -> Option<ResolvedMaterial> {
    // If there are no render assets there is nothing to create or attach.
    if world.get_resource::<Assets<StandardMaterial>>().is_none() {
        return None;
    }
    let Some(binding) = read_material_binding(ctx.stage, ctx.path).ok().flatten() else {
        return None;
    };
    let registry = world
        .get_resource::<MaterialXRegistry>()
        .cloned()
        .unwrap_or_default();
    let materialx_result = if has_materialx_terminal(ctx.stage, &binding, &registry) {
        Some(compile_materialx(ctx.stage, &binding, ctx.time, &registry))
    } else {
        match find_external_materialx(ctx.stage, &binding) {
            Ok(Some(source)) => {
                let document_registry = world
                    .get_resource::<MaterialXDocumentRegistry>()
                    .cloned()
                    .unwrap_or_default();
                Some(compile_external_materialx(
                    &source,
                    &binding,
                    &document_registry,
                ))
            }
            Ok(None) => None,
            Err(diagnostic) => Some(Err(CompileFailure {
                diagnostics: vec![diagnostic],
            })),
        }
    };
    if let Some(result) = materialx_result {
        match result {
            Ok(compiled) => {
                for diagnostic in &compiled.diagnostics {
                    record_materialx_diagnostic(world, diagnostic.clone());
                }
                if world.get_resource::<Assets<MaterialXMaterial>>().is_none() {
                    bevy::log::error!(
                        target: "usd_bevy::materialx",
                        "{}: MaterialXMaterial assets are unavailable; add UsdPlugin after Bevy's render plugins",
                        binding.as_str()
                    );
                    return None;
                }
                let thickness = if compiled.transmission {
                    read_materialx_mesh_thickness(ctx)
                } else {
                    0.0
                };
                if compiled.transmission {
                    bevy::log::trace!(
                        target: "usd_bevy::materialx",
                        "{}: closed-mesh refraction thickness {thickness}",
                        binding.as_str()
                    );
                }
                let material = match prepare_material(world, &compiled, thickness) {
                    Ok(material) => material,
                    Err(diagnostic) => {
                        record_materialx_diagnostic(world, diagnostic.clone());
                        return Some(materialx_fallback(world, vec![diagnostic]));
                    }
                };
                let handle = world
                    .resource_mut::<Assets<MaterialXMaterial>>()
                    .add(material);
                return Some(ResolvedMaterial {
                    material: RoutedMaterial::MaterialX(handle),
                    failure: None,
                });
            }
            Err(failure) => {
                for diagnostic in &failure.diagnostics {
                    record_materialx_diagnostic(world, diagnostic.clone());
                }
                return Some(materialx_fallback(world, failure.diagnostics));
            }
        }
    }

    let Some(read) = material_of(ctx, &binding) else {
        return None;
    };
    let assets = world.get_resource::<AssetServer>().cloned();
    let material = to_standard_material(&read, assets.as_ref());
    let handle = super::cache::intern_preview_material(world, &read, assets.is_some(), material);
    Some(ResolvedMaterial {
        material: RoutedMaterial::Standard(handle),
        failure: None,
    })
}

fn record_materialx_diagnostic(
    world: &mut World,
    diagnostic: crate::materialx::diagnostic::MaterialXDiagnostic,
) {
    match diagnostic.severity {
        Severity::Warning => bevy::log::warn!(target: "usd_bevy::materialx", "{diagnostic}"),
        Severity::Error => bevy::log::error!(target: "usd_bevy::materialx", "{diagnostic}"),
    }
    if let Some(mut diagnostics) = world.get_resource_mut::<MaterialXDiagnostics>() {
        diagnostics.push(diagnostic);
    }
}

fn materialx_fallback(
    world: &mut World,
    diagnostics: Vec<crate::materialx::diagnostic::MaterialXDiagnostic>,
) -> ResolvedMaterial {
    let read = ReadPreviewMaterial {
        diffuse_color: Some([1.0, 0.0, 1.0]),
        roughness: Some(0.35),
        emissive_color: Some([0.25, 0.0, 0.25]),
        ..Default::default()
    };
    let material = to_standard_material(&read, None);
    let handle = super::cache::intern_preview_material(world, &read, false, material);
    ResolvedMaterial {
        material: RoutedMaterial::Standard(handle),
        failure: Some(MaterialXFailure(diagnostics)),
    }
}

fn material_property(name: &str) -> bool {
    name.starts_with("material:binding")
}

/// Bevy's screen-space refraction needs a geometric travel distance, while
/// MaterialX correctly keeps that renderer concern out of the surface graph.
/// Use the same convention as Bevy's transmission example: for a closed mesh,
/// the smallest full AABB extent is its characteristic local-space thickness.
/// Open/non-manifold meshes remain zero; authored `thin_walled` also forces
/// zero later in the generated shader.
fn read_materialx_mesh_thickness(ctx: &RouteCtx) -> f32 {
    crate::read::geom::read_mesh(ctx.stage, ctx.path)
        .ok()
        .flatten()
        .as_ref()
        .map_or(0.0, materialx_mesh_thickness)
}

fn materialx_mesh_thickness(read: &crate::read::geom::ReadMesh) -> f32 {
    if !mesh_is_closed(read) || read.points.is_empty() {
        return 0.0;
    }

    let mut minimum = Vec3::splat(f32::INFINITY);
    let mut maximum = Vec3::splat(f32::NEG_INFINITY);
    for point in &read.points {
        let point = Vec3::from_array(*point);
        if !point.is_finite() {
            return 0.0;
        }
        minimum = minimum.min(point);
        maximum = maximum.max(point);
    }
    let extent = maximum - minimum;
    let thickness = extent.min_element();
    if thickness.is_finite() && thickness > f32::EPSILON {
        thickness
    } else {
        0.0
    }
}

fn mesh_is_closed(read: &crate::read::geom::ReadMesh) -> bool {
    let mut edges: HashMap<(i32, i32), u32> = HashMap::new();
    let mut cursor = 0usize;
    for &count in &read.face_vertex_counts {
        let Ok(count) = usize::try_from(count) else {
            return false;
        };
        if count < 3 || cursor.saturating_add(count) > read.face_vertex_indices.len() {
            return false;
        }
        let face = &read.face_vertex_indices[cursor..cursor + count];
        for index in face {
            if *index < 0 || *index as usize >= read.points.len() {
                return false;
            }
        }
        for index in 0..count {
            let a = face[index];
            let b = face[(index + 1) % count];
            if a == b {
                return false;
            }
            let edge = if a < b { (a, b) } else { (b, a) };
            *edges.entry(edge).or_default() += 1;
        }
        cursor += count;
    }
    cursor == read.face_vertex_indices.len()
        && !edges.is_empty()
        && edges.values().all(|count| *count == 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::geom::{Orientation, ReadMesh, SubdivScheme};
    use bevy::shader::Shader;

    fn test_mesh(
        points: Vec<[f32; 3]>,
        face_vertex_counts: Vec<i32>,
        face_vertex_indices: Vec<i32>,
    ) -> ReadMesh {
        ReadMesh {
            points,
            face_vertex_counts,
            face_vertex_indices,
            normals: None,
            uvs: None,
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
    fn closed_mesh_uses_smallest_full_extent_as_thickness() {
        let cube = test_mesh(
            vec![
                [-1.0, -1.0, -1.0],
                [1.0, -1.0, -1.0],
                [1.0, 1.0, -1.0],
                [-1.0, 1.0, -1.0],
                [-1.0, -1.0, 1.0],
                [1.0, -1.0, 1.0],
                [1.0, 1.0, 1.0],
                [-1.0, 1.0, 1.0],
            ],
            vec![4; 6],
            vec![
                0, 1, 2, 3, 4, 7, 6, 5, 0, 4, 5, 1, 1, 5, 6, 2, 2, 6, 7, 3, 3, 7, 4, 0,
            ],
        );
        assert!(mesh_is_closed(&cube));
        assert_eq!(materialx_mesh_thickness(&cube), 2.0);
    }

    #[test]
    fn open_mesh_has_no_solid_refraction_thickness() {
        let triangle = test_mesh(
            vec![[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0]],
            vec![3],
            vec![0, 1, 2],
        );
        assert!(!mesh_is_closed(&triangle));
        assert_eq!(materialx_mesh_thickness(&triangle), 0.0);
    }

    fn fixture_stage(name: &str) -> openusd::usd::Stage {
        let path = format!(
            "{}/../../assets/tests/materialx/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let stage = openusd::usd::Stage::open(&path).unwrap();
        stage
            .define_prim("/Mesh")
            .unwrap()
            .set_type_name("Mesh")
            .unwrap();
        stage
            .create_relationship("/Mesh.material:binding")
            .unwrap()
            .add_target(Path::new("/Mat").unwrap())
            .unwrap();
        stage
    }

    fn material_world() -> World {
        let mut world = World::new();
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(Assets::<MaterialXMaterial>::default());
        world.insert_resource(Assets::<Shader>::default());
        world.insert_resource(MaterialXRegistry::default());
        world.insert_resource(MaterialXDocumentRegistry::default());
        world.insert_resource(MaterialXDiagnostics::default());
        world.insert_resource(crate::materialx::material::MaterialXTextureCache::default());
        world
    }

    #[test]
    fn material_patch_filter_ignores_purpose() {
        assert!(!material_property("purpose"));
        assert!(!material_property("points"));
        assert!(material_property("material:binding"));
        assert!(material_property("material:binding:preview"));
    }

    #[test]
    fn opaque_material_maps_channels() {
        let read = ReadPreviewMaterial {
            diffuse_color: Some([0.2, 0.4, 0.6]),
            roughness: Some(0.3),
            metallic: Some(0.8),
            emissive_color: Some([1.0, 0.0, 0.0]),
            ior: Some(1.4),
            ..Default::default()
        };
        let m = to_standard_material(&read, None);
        let c = m.base_color.to_srgba();
        assert!((c.red - 0.2).abs() < 1e-3 && (c.blue - 0.6).abs() < 1e-3);
        assert!((c.alpha - 1.0).abs() < 1e-6, "opaque by default");
        assert!((m.perceptual_roughness - 0.3).abs() < 1e-6);
        assert!((m.metallic - 0.8).abs() < 1e-6);
        assert!((m.ior - 1.4).abs() < 1e-6);
        assert_eq!(m.emissive, LinearRgba::rgb(1.0, 0.0, 0.0));
        assert!(matches!(m.alpha_mode, AlphaMode::Opaque));
    }

    #[test]
    fn translucent_material_sets_blend() {
        let read = ReadPreviewMaterial {
            diffuse_color: Some([1.0, 1.0, 1.0]),
            opacity: Some(0.5),
            ..Default::default()
        };
        let m = to_standard_material(&read, None);
        assert!(
            (m.base_color.to_srgba().alpha - 0.5).abs() < 1e-3,
            "alpha from opacity"
        );
        assert!(
            matches!(m.alpha_mode, AlphaMode::Blend),
            "opacity<1 → Blend"
        );
    }

    #[test]
    fn empty_material_is_default_ish() {
        // An all-None read yields (essentially) the StandardMaterial default.
        let m = to_standard_material(&ReadPreviewMaterial::default(), None);
        let def = StandardMaterial::default();
        assert_eq!(m.base_color.to_srgba(), def.base_color.to_srgba());
        assert!(matches!(m.alpha_mode, AlphaMode::Opaque));
    }

    #[test]
    fn materialx_route_attaches_custom_material_and_registers_wesl() {
        let stage = fixture_stage("arithmetic.usda");
        let path = Path::new("/Mesh").unwrap();
        let ctx = RouteCtx::new(&stage, &path);
        let mut world = material_world();
        let entity = world.spawn_empty().id();

        MaterialRoute.project(&ctx, &mut world, entity);

        assert!(
            world
                .get::<MeshMaterial3d<MaterialXMaterial>>(entity)
                .is_some()
        );
        assert!(
            world
                .get::<MeshMaterial3d<StandardMaterial>>(entity)
                .is_none()
        );
        assert!(world.get::<MaterialXFailure>(entity).is_none());
        assert!(
            world.resource::<Assets<Shader>>().len() > 1,
            "translated modules and generated root were registered"
        );
    }

    #[test]
    fn materialx_route_decodes_resolved_texture() {
        let stage = fixture_stage("image_uv0.usda");
        let path = Path::new("/Mesh").unwrap();
        let ctx = RouteCtx::new(&stage, &path);
        let mut world = material_world();
        world.insert_resource(Assets::<Image>::default());
        let entity = world.spawn_empty().id();

        MaterialRoute.project(&ctx, &mut world, entity);

        let material_handle = &world
            .get::<MeshMaterial3d<MaterialXMaterial>>(entity)
            .expect("valid image graph uses the MaterialX material")
            .0;
        let material = world
            .resource::<Assets<MaterialXMaterial>>()
            .get(material_handle)
            .unwrap();
        assert!(material.texture_0.is_some());
        assert_eq!(world.resource::<Assets<Image>>().len(), 1);
        assert!(world.get::<MaterialXFailure>(entity).is_none());
    }

    #[test]
    fn materialx_route_attaches_visible_fallback_and_diagnostic() {
        let stage = fixture_stage("unknown_node.usda");
        let path = Path::new("/Mesh").unwrap();
        let ctx = RouteCtx::new(&stage, &path);
        let mut world = material_world();
        let entity = world.spawn_empty().id();

        MaterialRoute.project(&ctx, &mut world, entity);

        assert!(
            world
                .get::<MeshMaterial3d<StandardMaterial>>(entity)
                .is_some()
        );
        assert!(
            world
                .get::<MeshMaterial3d<MaterialXMaterial>>(entity)
                .is_none()
        );
        let failure = world.get::<MaterialXFailure>(entity).unwrap();
        assert!(failure.0.iter().any(|diagnostic| diagnostic.code
            == crate::materialx::diagnostic::DiagnosticCode::UnknownNodeDef));
    }
}
