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
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::Shader;

use super::compiler::{CompiledMaterialX, CompiledTexture, MAX_TEXTURES, MAX_UNIFORMS};
use super::diagnostic::{DiagnosticCode, MaterialXDiagnostic};
use super::registry::MaterialXRegistry;

const MODULE_UUID_PREFIX: u128 = 0x4d58_5745_534c_4d4f_0000_0000_0000_0000;
const GRAPH_UUID_PREFIX: u128 = 0x4d58_4752_4150_4853_0000_0000_0000_0000;

/// Strong handles for synchronously decoded USD-resolved texture files.
#[derive(Resource, Default)]
pub struct MaterialXTextureCache(HashMap<CompiledTexture, Handle<Image>>);

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
    pub graph_key: u64,
    pub alpha_mode: AlphaMode,
}

#[repr(C)]
#[derive(Eq, PartialEq, Hash, Copy, Clone)]
pub struct MaterialXPipelineKey {
    graph_key: u64,
}

impl From<&MaterialXMaterial> for MaterialXPipelineKey {
    fn from(material: &MaterialXMaterial) -> Self {
        Self {
            graph_key: material.graph_key,
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

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &bevy::mesh::MeshVertexBufferLayoutRef,
        key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment.shader = graph_shader_handle(key.bind_group_data.graph_key);
        }
        Ok(())
    }
}

/// Register all required translator modules and this graph's generated root,
/// then construct the host-side material asset.
pub fn prepare_material(
    world: &mut World,
    compiled: &CompiledMaterialX,
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
        graph_key: compiled.graph_key,
        alpha_mode: if compiled.alpha_blend {
            AlphaMode::Blend
        } else {
            AlphaMode::Opaque
        },
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

    let bytes = std::fs::read(&texture.path).map_err(|error| {
        texture_error(
            compiled,
            &texture.path,
            format!("cannot read texture: {error}"),
        )
    })?;
    let extension = Path::new(&texture.path)
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| {
            texture_error(
                compiled,
                &texture.path,
                "texture has no usable file extension",
            )
        })?;
    let image = Image::from_buffer(
        &bytes,
        ImageType::Extension(extension),
        CompressedImageFormats::NONE,
        texture.is_srgb,
        texture_sampler(texture),
        RenderAssetUsages::default(),
    )
    .map_err(|error| {
        texture_error(
            compiled,
            &texture.path,
            format!("cannot decode texture: {error}"),
        )
    })?;
    let handle = world.resource_mut::<Assets<Image>>().add(image);
    world
        .resource_mut::<MaterialXTextureCache>()
        .0
        .insert(texture.clone(), handle.clone());
    Ok(handle)
}

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
}
