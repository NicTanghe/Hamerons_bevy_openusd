//! Bevy material and deterministic WESL shader/module registration.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::Path;

use bevy::asset::RenderAssetUsages;
use bevy::asset::uuid::Uuid;
use bevy::image::{
    CompressedImageFormats, ImageAddressMode, ImageFilterMode, ImageSampler,
    ImageSamplerDescriptor, ImageType,
};
use bevy::pbr::{Material, MaterialPipeline, MaterialPipelineKey};
use bevy::platform::hash::FixedHasher;
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::shader::Shader;
#[cfg(not(target_arch = "wasm32"))]
use bevy::tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};

use super::asset::MaterialXAssetReader;
use super::compiler::{CompiledMaterialX, CompiledTexture, MAX_TEXTURES, MAX_UNIFORMS};
#[cfg(not(target_arch = "wasm32"))]
use super::diagnostic::MaterialXDiagnostics;
use super::diagnostic::{DiagnosticCode, MaterialXDiagnostic};
use super::registry::MaterialXRegistry;

const MODULE_UUID_PREFIX: u128 = 0x4d58_5745_534c_4d4f_0000_0000_0000_0000;
const GRAPH_UUID_PREFIX: u128 = 0x4d58_4752_4150_4853_0000_0000_0000_0000;

/// Strong handles for synchronously decoded USD-resolved texture files.
#[derive(Resource, Default)]
pub struct MaterialXTextureCache(HashMap<CompiledTexture, Handle<Image>>);

/// Native texture reads and decodes that are still running off the UI thread.
///
/// This is a separate opt-in resource so small bare test worlds retain the
/// deterministic synchronous path. [`crate::UsdPlugin`] installs it in the
/// real application.
#[derive(Resource, Default)]
pub struct MaterialXPendingTextures {
    #[cfg(not(target_arch = "wasm32"))]
    pending: HashMap<CompiledTexture, PendingTexture>,
}

#[cfg(not(target_arch = "wasm32"))]
struct PendingTexture {
    task: Task<Result<Image, String>>,
    handle: Handle<Image>,
    material: String,
}

/// Renderer-owned geometry parameters that MaterialX deliberately does not
/// author. `thickness` is a local-space closed-mesh estimate; the generated
/// shader applies the instance's world scale before Bevy traces refraction.
#[derive(Debug, Clone, Copy, ShaderType)]
pub struct MaterialXRendererParams {
    pub thickness: f32,
    // These fields make encase physically upload 16 bytes. They are deliberately
    // absent from the generated WESL, which represents the same trailing space
    // with `@size(16)` and therefore exposes no padding names to GLSL ES.
    padding_0: f32,
    padding_1: f32,
    padding_2: f32,
}

/// Fixed host resource layout for the first MaterialX slice.
#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
#[bind_group_data(MaterialXPipelineKey)]
pub struct MaterialXMaterial {
    #[uniform(0)]
    pub values: [Vec4; MAX_UNIFORMS],
    #[texture(1)]
    #[sampler(2)]
    pub texture_0: Option<Handle<Image>>,
    #[texture(3)]
    #[sampler(4)]
    pub texture_1: Option<Handle<Image>>,
    #[texture(5)]
    #[sampler(6)]
    pub texture_2: Option<Handle<Image>>,
    #[texture(7)]
    #[sampler(8)]
    pub texture_3: Option<Handle<Image>>,
    #[uniform(9)]
    pub renderer: MaterialXRendererParams,
    pub graph_key: u64,
    pub alpha_mode: AlphaMode,
    pub transmission: bool,
}

#[repr(C)]
#[derive(Eq, PartialEq, Hash, Copy, Clone)]
pub struct MaterialXPipelineKey {
    graph_key: u64,
    transmission: bool,
}

impl From<&MaterialXMaterial> for MaterialXPipelineKey {
    fn from(material: &MaterialXMaterial) -> Self {
        Self {
            graph_key: material.graph_key,
            transmission: material.transmission,
        }
    }
}

impl Material for MaterialXMaterial {
    fn alpha_mode(&self) -> AlphaMode {
        self.alpha_mode
    }

    fn enable_prepass() -> bool {
        false
    }

    fn reads_view_transmission_texture(&self) -> bool {
        self.transmission
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment.shader = graph_shader_handle(key.bind_group_data.graph_key);
            if key.bind_group_data.transmission {
                fragment
                    .shader_defs
                    .push("STANDARD_MATERIAL_SPECULAR_TRANSMISSION".into());
                fragment
                    .shader_defs
                    .push("STANDARD_MATERIAL_DIFFUSE_OR_SPECULAR_TRANSMISSION".into());
            }
        }
        Ok(())
    }
}

/// Register all required translator modules and this graph's generated root,
/// then construct the host-side material asset.
pub fn prepare_material(
    world: &mut World,
    compiled: &CompiledMaterialX,
    renderer_thickness: f32,
) -> Result<MaterialXMaterial, MaterialXDiagnostic> {
    register_shaders(world, compiled);

    let mut values = [Vec4::ZERO; MAX_UNIFORMS];
    for (target, source) in values.iter_mut().zip(&compiled.uniforms) {
        *target = Vec4::from(*source);
    }
    let mut textures: [Option<Handle<Image>>; MAX_TEXTURES] = Default::default();
    for (target, texture) in textures.iter_mut().zip(&compiled.textures) {
        *target = Some(load_texture(world, compiled, texture)?);
    }
    let [texture_0, texture_1, texture_2, texture_3] = textures;
    Ok(MaterialXMaterial {
        values,
        texture_0,
        texture_1,
        texture_2,
        texture_3,
        renderer: MaterialXRendererParams {
            thickness: renderer_thickness,
            padding_0: 0.0,
            padding_1: 0.0,
            padding_2: 0.0,
        },
        graph_key: compiled.graph_key,
        alpha_mode: if compiled.alpha_blend {
            AlphaMode::Blend
        } else {
            AlphaMode::Opaque
        },
        transmission: compiled.transmission,
    })
}

fn load_texture(
    world: &mut World,
    compiled: &CompiledMaterialX,
    texture: &CompiledTexture,
) -> Result<Handle<Image>, MaterialXDiagnostic> {
    if let Some(handle) = world
        .get_resource::<MaterialXTextureCache>()
        .and_then(|cache| cache.0.get(texture))
    {
        return Ok(handle.clone());
    }
    if world.get_resource::<Assets<Image>>().is_none() {
        return Err(texture_error(
            compiled,
            &texture.path,
            "Image assets are unavailable; add Bevy's image/render plugins before UsdPlugin",
        ));
    }
    let asset_reader = world.get_resource::<MaterialXAssetReader>().cloned();

    #[cfg(not(target_arch = "wasm32"))]
    if world.get_resource::<MaterialXPendingTextures>().is_some()
        && let Some(pool) = AsyncComputeTaskPool::try_get()
    {
        let handle = world.resource::<Assets<Image>>().reserve_handle();
        let owned_texture = texture.clone();
        let task_reader = asset_reader.clone();
        let task = pool.spawn(async move { decode_texture(&owned_texture, task_reader.as_ref()) });
        world
            .resource_mut::<MaterialXTextureCache>()
            .0
            .insert(texture.clone(), handle.clone());
        world
            .resource_mut::<MaterialXPendingTextures>()
            .pending
            .insert(
                texture.clone(),
                PendingTexture {
                    task,
                    handle: handle.clone(),
                    material: compiled.material.clone(),
                },
            );
        return Ok(handle);
    }

    let image = decode_texture(texture, asset_reader.as_ref())
        .map_err(|message| texture_error(compiled, &texture.path, message))?;
    let handle = world.resource_mut::<Assets<Image>>().add(image);
    world
        .resource_mut::<MaterialXTextureCache>()
        .0
        .insert(texture.clone(), handle.clone());
    Ok(handle)
}

fn decode_texture(
    texture: &CompiledTexture,
    asset_reader: Option<&MaterialXAssetReader>,
) -> Result<Image, String> {
    let bytes = if let Some(reader) = asset_reader {
        reader
            .read(Path::new(&texture.path))
            .map_err(|error| format!("cannot read texture: {error}"))?
    } else {
        std::fs::read(&texture.path).map_err(|error| format!("cannot read texture: {error}"))?
    };
    let extension = Path::new(&texture.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| "texture has no usable file extension".to_owned())?;
    Image::from_buffer(
        &bytes,
        ImageType::Extension(extension),
        CompressedImageFormats::NONE,
        texture.is_srgb,
        texture_sampler(texture),
        RenderAssetUsages::default(),
    )
    .map_err(|error| format!("cannot decode texture: {error}"))
}

/// Publish a bounded number of completed MaterialX images each frame. Decode
/// work stays off-thread; limiting uploads avoids replacing one long load stall
/// with a single large GPU-upload frame.
#[cfg(not(target_arch = "wasm32"))]
pub fn apply_completed_materialx_textures(world: &mut World) {
    const MAX_COMPLETIONS_PER_FRAME: usize = 2;
    if world.get_resource::<MaterialXPendingTextures>().is_none()
        || world.get_resource::<Assets<Image>>().is_none()
    {
        return;
    }

    let keys = world
        .resource::<MaterialXPendingTextures>()
        .pending
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut completed = Vec::new();
    for texture in keys {
        if completed.len() >= MAX_COMPLETIONS_PER_FRAME {
            break;
        }
        let ready = {
            let mut pending = world.resource_mut::<MaterialXPendingTextures>();
            let Some(entry) = pending.pending.get_mut(&texture) else {
                continue;
            };
            block_on(poll_once(&mut entry.task))
        };
        if let Some(result) = ready {
            let entry = world
                .resource_mut::<MaterialXPendingTextures>()
                .pending
                .remove(&texture)
                .expect("completed MaterialX texture remains registered");
            completed.push((texture, entry.handle, entry.material, result));
        }
    }

    for (texture, handle, material, result) in completed {
        let image = match result {
            Ok(image) => image,
            Err(message) => {
                let diagnostic = MaterialXDiagnostic::error(
                    DiagnosticCode::MissingAsset,
                    &material,
                    None,
                    Some(texture.path.clone()),
                    message,
                );
                bevy::log::error!(target: "usd_bevy::materialx", "{diagnostic}");
                if let Some(mut diagnostics) = world.get_resource_mut::<MaterialXDiagnostics>() {
                    diagnostics.push(diagnostic);
                }
                let mut fallback = Image::default();
                fallback.sampler = texture_sampler(&texture);
                fallback
            }
        };
        world
            .resource_mut::<Assets<Image>>()
            .insert(handle.id(), image)
            .expect("reserved MaterialX image handle remains valid");
        bevy::log::trace!(
            target: "usd_bevy::materialx",
            "{}: async texture ready",
            texture.path
        );
    }
}

/// Web builds currently retain the synchronous image decode path.
#[cfg(target_arch = "wasm32")]
pub fn apply_completed_materialx_textures(_world: &mut World) {}

fn texture_sampler(texture: &CompiledTexture) -> ImageSampler {
    let mut descriptor = ImageSamplerDescriptor::linear();
    descriptor.address_mode_u = address_mode(&texture.u_address_mode);
    descriptor.address_mode_v = address_mode(&texture.v_address_mode);
    let filter = if texture.filter_type.eq_ignore_ascii_case("closest") {
        ImageFilterMode::Nearest
    } else {
        ImageFilterMode::Linear
    };
    descriptor.set_filter(filter);
    ImageSampler::Descriptor(descriptor)
}

fn address_mode(mode: &str) -> ImageAddressMode {
    match mode.to_ascii_lowercase().as_str() {
        "periodic" | "repeat" => ImageAddressMode::Repeat,
        "mirror" | "mirror_repeat" => ImageAddressMode::MirrorRepeat,
        _ => ImageAddressMode::ClampToEdge,
    }
}

fn texture_error(
    compiled: &CompiledMaterialX,
    path: &str,
    message: impl std::fmt::Display,
) -> MaterialXDiagnostic {
    MaterialXDiagnostic::error(
        DiagnosticCode::MissingAsset,
        &compiled.material,
        None,
        Some(path.into()),
        message.to_string(),
    )
}

fn register_shaders(world: &mut World, compiled: &CompiledMaterialX) {
    let Some(registry) = world.get_resource::<MaterialXRegistry>() else {
        return;
    };
    let modules = registry.translator_modules();
    if world.get_resource::<Assets<Shader>>().is_none() {
        return;
    }

    let mut shaders = world.resource_mut::<Assets<Shader>>();
    for (module, path, source) in modules {
        let uuid = module_uuid(module);
        if !shaders.contains(bevy::asset::AssetId::Uuid { uuid }) {
            shaders
                .insert(uuid, Shader::from_wesl(*source, *path))
                .expect("UUID MaterialX module insertion cannot fail");
        }
    }
    let uuid = graph_uuid(compiled.graph_key);
    if !shaders.contains(bevy::asset::AssetId::Uuid { uuid }) {
        let path = format!(
            "embedded://usd_bevy/materialx/graph_{:016x}.wesl",
            compiled.graph_key
        );
        shaders
            .insert(uuid, Shader::from_wesl(compiled.wesl.clone(), path))
            .expect("UUID MaterialX root insertion cannot fail");
    }
}

fn module_uuid(module: &str) -> Uuid {
    let mut hasher = FixedHasher.build_hasher();
    module.hash(&mut hasher);
    Uuid::from_u128(MODULE_UUID_PREFIX | hasher.finish() as u128)
}

fn graph_uuid(graph_key: u64) -> Uuid {
    Uuid::from_u128(GRAPH_UUID_PREFIX | graph_key as u128)
}

fn graph_shader_handle(graph_key: u64) -> Handle<Shader> {
    graph_uuid(graph_key).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_handles_are_deterministic_and_namespaced() {
        assert_eq!(graph_shader_handle(7).id(), graph_shader_handle(7).id());
        assert_ne!(graph_shader_handle(7).id(), graph_shader_handle(8).id());
        assert_ne!(module_uuid("generated::inline"), graph_uuid(7));
    }

    #[test]
    fn renderer_uniform_is_webgl_aligned() {
        assert_eq!(MaterialXRendererParams::min_size().get(), 16);

        let params = MaterialXRendererParams {
            thickness: 1.0,
            padding_0: 0.0,
            padding_1: 0.0,
            padding_2: 0.0,
        };
        let mut buffer =
            bevy::render::render_resource::encase::UniformBuffer::new(Vec::<u8>::new());
        buffer.write(&params).unwrap();
        assert_eq!(buffer.into_inner().len(), 16);
    }

    #[test]
    fn decodes_texture_from_host_asset_reader() {
        let texture_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../../assets/full_assets/OpenChessSet/assets/Chessboard/tex/chessboard_base_color.jpg",
        );
        if !texture_path.is_file() {
            eprintln!("skipping sibling OpenChessSet texture test");
            return;
        }
        let bytes = std::fs::read(texture_path).unwrap();
        let reader = MaterialXAssetReader::new(move |path| {
            (path == Path::new("virtual.jpg"))
                .then(|| bytes.clone())
                .ok_or_else(|| "missing".to_owned())
        });
        let texture = CompiledTexture {
            path: "virtual.jpg".into(),
            is_srgb: true,
            u_address_mode: "periodic".into(),
            v_address_mode: "periodic".into(),
            filter_type: "linear".into(),
        };

        decode_texture(&texture, Some(&reader)).expect("host-provided JPEG should decode");
    }
}
