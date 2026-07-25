//! UsdShade read side: resolve a `Material`'s surface shader and harvest its
//! `UsdPreviewSurface` (and MDL / MaterialX equivalents) inputs into a
//! [`ReadPreviewMaterial`], following connections through `UsdUVTexture` and
//! the MaterialX node graph. Reads through openusd only.

use openusd::sdf::{Path, Value};
use openusd::usd::Stage;

use super::util::{
    connections_at, default_at, read_asset_path, read_token_or_string, read_vec2f, targets_at,
};

/// Decoded UsdPreviewSurface material. Each channel is `None` (unauthored),
/// a scalar, or a texture asset path (caller resolves via the AssetServer).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadPreviewMaterial {
    pub diffuse_color: Option<[f32; 3]>,
    pub opacity: Option<f32>,
    pub opacity_threshold: Option<f32>,
    pub roughness: Option<f32>,
    pub metallic: Option<f32>,
    pub emissive_color: Option<[f32; 3]>,
    pub ior: Option<f32>,

    pub diffuse_texture: Option<String>,
    pub normal_texture: Option<String>,
    pub roughness_texture: Option<String>,
    pub metallic_texture: Option<String>,
    pub opacity_texture: Option<String>,
    pub emissive_texture: Option<String>,
    pub occlusion_texture: Option<String>,

    /// `UsdTransform2d` on the texture-coordinate chain (scale/rotate/translate
    /// of `st`), if the network has one. Applied to `StandardMaterial::uv_transform`.
    pub uv_transform: Option<UvTransform>,
}

/// A 2D texture-coordinate transform read from a `UsdTransform2d` node:
/// USD applies it as `st' = rotate(scale * st) + translation`, with rotation in
/// degrees, counter-clockwise about the origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UvTransform {
    pub translation: [f32; 2],
    pub rotation_deg: f32,
    pub scale: [f32; 2],
}

impl Default for UvTransform {
    fn default() -> Self {
        Self {
            translation: [0.0, 0.0],
            rotation_deg: 0.0,
            scale: [1.0, 1.0],
        }
    }
}

/// Read `material:binding` on a geom prim and return the bound Material prim
/// path. `None` if no binding is authored.
pub fn read_material_binding(stage: &Stage, prim: &Path) -> anyhow::Result<Option<Path>> {
    let rel_path = prim.append_property("material:binding")?;
    Ok(targets_at(stage, &rel_path)?.into_iter().next())
}

/// Read a `Material` prim and return its decoded surface inputs.
pub fn read_preview_material(
    stage: &Stage,
    material: &Path,
) -> anyhow::Result<Option<ReadPreviewMaterial>> {
    let Some((shader, dialect)) = resolve_surface_shader(stage, material)? else {
        return Ok(None);
    };

    let shader_id = read_token_or_string(stage, &shader, "info:id")?;
    let mdl_subid = read_token_or_string(stage, &shader, "info:mdl:sourceAsset:subIdentifier")?;
    let mdl_source = read_asset_path(stage, &shader, "info:mdl:sourceAsset")?;
    let mdl_basename = mdl_source.as_deref().and_then(|p| {
        std::path::Path::new(p)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
    });
    let mdl_id = mdl_subid.as_deref().or(mdl_basename.as_deref());

    let channels: &[(&str, ColourSetter, ScalarSetter, TextureSetter)] = match dialect {
        SurfaceDialect::Mdl => match mdl_id {
            Some("OmniSurface") | Some("OmniSurfaceLite") | Some("OmniSurfaceBase") => {
                OMNISURFACE_CHANNELS
            }
            _ => OMNIPBR_CHANNELS,
        },
        SurfaceDialect::MaterialX => match shader_id.as_deref() {
            Some("ND_standard_surface_surfaceshader") => MATERIALX_STD_SURFACE_CHANNELS,
            _ => PREVIEW_CHANNELS,
        },
        SurfaceDialect::Preview => match shader_id.as_deref() {
            Some("UsdPreviewSurface") | Some("ND_UsdPreviewSurface_surfaceshader") | None => {
                PREVIEW_CHANNELS
            }
            Some("OmniPBR") | Some("OmniPBR_Opacity") | Some("OmniPBR_ClearCoat") => {
                OMNIPBR_CHANNELS
            }
            Some("OmniSurface") | Some("OmniSurfaceLite") | Some("OmniSurfaceBase") => {
                OMNISURFACE_CHANNELS
            }
            Some("ND_standard_surface_surfaceshader") => MATERIALX_STD_SURFACE_CHANNELS,
            _ => return Ok(None),
        },
    };

    let mut out = ReadPreviewMaterial::default();
    for (channel, bind_colour, bind_scalar, bind_texture) in channels {
        let (value, texture) = resolve_channel(stage, material, &shader, channel)?;
        if let Some(tex) = texture {
            bind_texture(&mut out, tex);
        }
        match value {
            Some(ResolvedValue::Color3(c)) => bind_colour(&mut out, c),
            Some(ResolvedValue::Scalar(s)) => bind_scalar(&mut out, s),
            None => {}
        }
    }
    out.uv_transform = read_uv_transform(stage, material)?;
    Ok(Some(out))
}

/// Find a `UsdTransform2d` node in the material's shader network and read its
/// `scale` / `rotation` / `translation` inputs. USD materials generally share a
/// single st transform across textures, so the first one found is applied
/// material-wide. `None` when the network has no transform (identity `st`).
fn read_uv_transform(stage: &Stage, material: &Path) -> anyhow::Result<Option<UvTransform>> {
    for child in stage
        .prim(material.clone())
        .child_names()
        .unwrap_or_default()
    {
        let node = material.append_path(child.as_str())?;
        if read_token_or_string(stage, &node, "info:id")?.as_deref() != Some("UsdTransform2d") {
            continue;
        }
        let mut t = UvTransform::default();
        if let Some(s) = read_vec2f(stage, &node, "inputs:scale")? {
            t.scale = s;
        }
        if let Some(tr) = read_vec2f(stage, &node, "inputs:translation")? {
            t.translation = tr;
        }
        if let Some(Value::Float(r)) = default_at(stage, &node.append_property("inputs:rotation")?)?
        {
            t.rotation_deg = r;
        } else if let Some(Value::Double(r)) =
            default_at(stage, &node.append_property("inputs:rotation")?)?
        {
            t.rotation_deg = r as f32;
        }
        return Ok(Some(t));
    }
    Ok(None)
}

#[derive(Copy, Clone, Debug)]
enum SurfaceDialect {
    Preview,
    MaterialX,
    Mdl,
}

fn resolve_surface_shader(
    stage: &Stage,
    material: &Path,
) -> anyhow::Result<Option<(Path, SurfaceDialect)>> {
    let outputs = [
        ("outputs:surface", SurfaceDialect::Preview),
        ("outputs:mtlx:surface", SurfaceDialect::MaterialX),
        ("outputs:mdl:surface", SurfaceDialect::Mdl),
    ];
    for (attr_name, dialect) in outputs {
        let attr_path = material.append_property(attr_name)?;
        if let Some(t) = connections_at(stage, &attr_path)?.into_iter().next() {
            return Ok(Some((t.prim_path(), dialect)));
        }
    }
    // Fallback: scan child Shader prims and infer the dialect.
    for child in stage
        .prim(material.clone())
        .child_names()
        .unwrap_or_default()
    {
        let shader = material.append_path(child.as_str())?;
        if stage.prim(shader.clone()).type_name()?.as_deref() != Some("Shader") {
            continue;
        }
        let shader_id = read_token_or_string(stage, &shader, "info:id")?;
        let mdl_subid = read_token_or_string(stage, &shader, "info:mdl:sourceAsset:subIdentifier")?;
        let mdl_source = read_asset_path(stage, &shader, "info:mdl:sourceAsset")?;
        if mdl_subid.is_some() || mdl_source.is_some() {
            return Ok(Some((shader, SurfaceDialect::Mdl)));
        }
        if matches!(
            shader_id.as_deref(),
            Some("UsdPreviewSurface")
                | Some("ND_UsdPreviewSurface_surfaceshader")
                | Some("OmniPBR")
                | Some("OmniPBR_Opacity")
                | Some("OmniPBR_ClearCoat")
        ) {
            return Ok(Some((shader, SurfaceDialect::Preview)));
        }
        if matches!(
            shader_id.as_deref(),
            Some("ND_standard_surface_surfaceshader")
        ) {
            return Ok(Some((shader, SurfaceDialect::MaterialX)));
        }
    }
    Ok(None)
}

type ColourSetter = fn(&mut ReadPreviewMaterial, [f32; 3]);
type ScalarSetter = fn(&mut ReadPreviewMaterial, f32);
type TextureSetter = fn(&mut ReadPreviewMaterial, String);

fn set_diffuse_c(o: &mut ReadPreviewMaterial, c: [f32; 3]) {
    o.diffuse_color = Some(c);
}
fn set_diffuse_s(_: &mut ReadPreviewMaterial, _: f32) {}
fn set_diffuse_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.diffuse_texture = Some(s);
}
fn set_opacity_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_opacity_s(o: &mut ReadPreviewMaterial, s: f32) {
    o.opacity = Some(s);
}
fn set_opacity_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.opacity_texture = Some(s);
}
fn set_opacity_threshold_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_opacity_threshold_s(o: &mut ReadPreviewMaterial, s: f32) {
    o.opacity_threshold = Some(s);
}
fn set_opacity_threshold_tex(_: &mut ReadPreviewMaterial, _: String) {}
fn set_rough_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_rough_s(o: &mut ReadPreviewMaterial, s: f32) {
    o.roughness = Some(s);
}
fn set_rough_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.roughness_texture = Some(s);
}
fn set_metal_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_metal_s(o: &mut ReadPreviewMaterial, s: f32) {
    o.metallic = Some(s);
}
fn set_metal_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.metallic_texture = Some(s);
}
fn set_emissive_c(o: &mut ReadPreviewMaterial, c: [f32; 3]) {
    o.emissive_color = Some(c);
}
fn set_emissive_s(_: &mut ReadPreviewMaterial, _: f32) {}
fn set_emissive_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.emissive_texture = Some(s);
}
fn set_ior_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_ior_s(o: &mut ReadPreviewMaterial, s: f32) {
    o.ior = Some(s);
}
fn set_ior_tex(_: &mut ReadPreviewMaterial, _: String) {}
fn set_normal_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_normal_s(_: &mut ReadPreviewMaterial, _: f32) {}
fn set_normal_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.normal_texture = Some(s);
}
fn set_occlusion_c(_: &mut ReadPreviewMaterial, _: [f32; 3]) {}
fn set_occlusion_s(_: &mut ReadPreviewMaterial, _: f32) {}
fn set_occlusion_tex(o: &mut ReadPreviewMaterial, s: String) {
    o.occlusion_texture = Some(s);
}

/// MaterialX `opacity` is a `color3`; fold to a luminance scalar.
fn set_opacity_mtlx_c(o: &mut ReadPreviewMaterial, c: [f32; 3]) {
    o.opacity = Some(0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]);
}

const PREVIEW_CHANNELS: &[(&str, ColourSetter, ScalarSetter, TextureSetter)] = &[
    (
        "diffuseColor",
        set_diffuse_c,
        set_diffuse_s,
        set_diffuse_tex,
    ),
    ("opacity", set_opacity_c, set_opacity_s, set_opacity_tex),
    (
        "opacityThreshold",
        set_opacity_threshold_c,
        set_opacity_threshold_s,
        set_opacity_threshold_tex,
    ),
    ("roughness", set_rough_c, set_rough_s, set_rough_tex),
    ("metallic", set_metal_c, set_metal_s, set_metal_tex),
    (
        "emissiveColor",
        set_emissive_c,
        set_emissive_s,
        set_emissive_tex,
    ),
    ("ior", set_ior_c, set_ior_s, set_ior_tex),
    ("normal", set_normal_c, set_normal_s, set_normal_tex),
    (
        "occlusion",
        set_occlusion_c,
        set_occlusion_s,
        set_occlusion_tex,
    ),
];

const MATERIALX_STD_SURFACE_CHANNELS: &[(&str, ColourSetter, ScalarSetter, TextureSetter)] = &[
    ("base_color", set_diffuse_c, set_diffuse_s, set_diffuse_tex),
    ("metalness", set_metal_c, set_metal_s, set_metal_tex),
    (
        "specular_roughness",
        set_rough_c,
        set_rough_s,
        set_rough_tex,
    ),
    (
        "emission_color",
        set_emissive_c,
        set_emissive_s,
        set_emissive_tex,
    ),
    (
        "opacity",
        set_opacity_mtlx_c,
        set_opacity_s,
        set_opacity_tex,
    ),
    ("normal", set_normal_c, set_normal_s, set_normal_tex),
];

const OMNIPBR_CHANNELS: &[(&str, ColourSetter, ScalarSetter, TextureSetter)] = &[
    (
        "diffuse_color_constant",
        set_diffuse_c,
        set_diffuse_s,
        set_diffuse_tex,
    ),
    (
        "diffuse_texture",
        set_diffuse_c,
        set_diffuse_s,
        set_diffuse_tex,
    ),
    (
        "reflection_roughness_constant",
        set_rough_c,
        set_rough_s,
        set_rough_tex,
    ),
    (
        "reflectionroughness_texture",
        set_rough_c,
        set_rough_s,
        set_rough_tex,
    ),
    ("metallic_constant", set_metal_c, set_metal_s, set_metal_tex),
    ("metallic_texture", set_metal_c, set_metal_s, set_metal_tex),
    (
        "emissive_color",
        set_emissive_c,
        set_emissive_s,
        set_emissive_tex,
    ),
    (
        "emissive_color_texture",
        set_emissive_c,
        set_emissive_s,
        set_emissive_tex,
    ),
    (
        "opacity_constant",
        set_opacity_c,
        set_opacity_s,
        set_opacity_tex,
    ),
    (
        "opacity_texture",
        set_opacity_c,
        set_opacity_s,
        set_opacity_tex,
    ),
    (
        "normalmap_texture",
        set_normal_c,
        set_normal_s,
        set_normal_tex,
    ),
];

const OMNISURFACE_CHANNELS: &[(&str, ColourSetter, ScalarSetter, TextureSetter)] = &[
    (
        "diffuse_reflection_color",
        set_diffuse_c,
        set_diffuse_s,
        set_diffuse_tex,
    ),
    (
        "diffuse_reflection_color_image",
        set_diffuse_c,
        set_diffuse_s,
        set_diffuse_tex,
    ),
    (
        "geometry_normal_image",
        set_normal_c,
        set_normal_s,
        set_normal_tex,
    ),
    (
        "geometry_opacity_image",
        set_opacity_c,
        set_opacity_s,
        set_opacity_tex,
    ),
    (
        "geometry_opacity",
        set_opacity_c,
        set_opacity_s,
        set_opacity_tex,
    ),
    ("roughness", set_rough_c, set_rough_s, set_rough_tex),
    ("metalness", set_metal_c, set_metal_s, set_metal_tex),
    (
        "emission_color",
        set_emissive_c,
        set_emissive_s,
        set_emissive_tex,
    ),
    (
        "emission_color_image",
        set_emissive_c,
        set_emissive_s,
        set_emissive_tex,
    ),
];

#[derive(Debug)]
enum ResolvedValue {
    Color3([f32; 3]),
    Scalar(f32),
}

fn resolve_channel(
    stage: &Stage,
    material: &Path,
    shader: &Path,
    channel: &str,
) -> anyhow::Result<(Option<ResolvedValue>, Option<String>)> {
    let mat_attr = format!("inputs:{channel}");
    let mat_path = material.append_property(&mat_attr)?;
    let (v, t) = resolve_attr_chain(stage, &mat_path)?;
    if v.is_some() || t.is_some() {
        return Ok((v, t));
    }
    let sh_path = shader.append_property(&mat_attr)?;
    resolve_attr_chain(stage, &sh_path)
}

fn resolve_attr_chain(
    stage: &Stage,
    attr_path: &Path,
) -> anyhow::Result<(Option<ResolvedValue>, Option<String>)> {
    let mut cur = attr_path.clone();
    for _ in 0..16 {
        if let Some(next) = connections_at(stage, &cur)?.into_iter().next() {
            let prim = next.prim_path();
            match shader_kind(stage, &prim)? {
                ShaderKind::Texture => return Ok((None, read_texture_file(stage, &prim)?)),
                ShaderKind::NormalMap => {
                    cur = prim.append_property("inputs:in")?;
                    continue;
                }
                ShaderKind::Constant => {
                    let v_path = prim.append_property("inputs:value")?;
                    return Ok((default_at(stage, &v_path)?.and_then(value_to_preview), None));
                }
                ShaderKind::Multiply | ShaderKind::AddOrSubtract => {
                    cur = prim.append_property("inputs:in1")?;
                    continue;
                }
                ShaderKind::Mix => {
                    cur = prim.append_property("inputs:fg")?;
                    continue;
                }
                ShaderKind::Unknown => {
                    cur = next;
                    continue;
                }
            }
        }
        let default = default_at(stage, &cur)?;
        match default.clone() {
            Some(Value::AssetPath(s)) => return Ok((None, Some(s.as_str().to_string()))),
            Some(Value::String(s)) => return Ok((None, Some(s))),
            _ => {}
        }
        return Ok((default.and_then(value_to_preview), None));
    }
    Ok((None, None))
}

enum ShaderKind {
    Texture,
    NormalMap,
    Constant,
    Multiply,
    AddOrSubtract,
    Mix,
    Unknown,
}

fn shader_kind(stage: &Stage, prim: &Path) -> anyhow::Result<ShaderKind> {
    let id = read_token_or_string(stage, prim, "info:id")?;
    Ok(match id.as_deref() {
        Some("UsdUVTexture") => ShaderKind::Texture,
        Some(s) if s.starts_with("ND_image_") => ShaderKind::Texture,
        Some("ND_normalmap") => ShaderKind::NormalMap,
        Some(s) if s.starts_with("ND_constant_") => ShaderKind::Constant,
        Some(s) if s.starts_with("ND_multiply_") => ShaderKind::Multiply,
        Some(s) if s.starts_with("ND_add_") || s.starts_with("ND_subtract_") => {
            ShaderKind::AddOrSubtract
        }
        Some(s) if s.starts_with("ND_mix_") => ShaderKind::Mix,
        _ => ShaderKind::Unknown,
    })
}

fn read_texture_file(stage: &Stage, tex_prim: &Path) -> anyhow::Result<Option<String>> {
    read_asset_path(stage, tex_prim, "inputs:file")
}

fn value_to_preview(v: Value) -> Option<ResolvedValue> {
    match v {
        Value::Float(f) => Some(ResolvedValue::Scalar(f)),
        Value::Double(d) => Some(ResolvedValue::Scalar(d as f32)),
        Value::Vec3f(c) => Some(ResolvedValue::Color3([c.x, c.y, c.z])),
        Value::Vec3d(c) => Some(ResolvedValue::Color3([c.x as f32, c.y as f32, c.z as f32])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openusd::usd::Stage;

    #[test]
    fn reads_uv_transform_from_transform2d() {
        let stage = Stage::builder().in_memory("uv.usda").unwrap();
        // Material with a preview surface + a UsdTransform2d node on the st chain.
        stage.define_prim("/Mat").unwrap().set_type_name("Material").unwrap();
        stage
            .create_attribute("/Mat.outputs:surface", "token")
            .unwrap()
            .set_connections([Path::new("/Mat/Surface.outputs:surface").unwrap()])
            .unwrap();
        let surf = stage.define_prim("/Mat/Surface").unwrap();
        surf.set_type_name("Shader").unwrap();
        stage
            .create_attribute("/Mat/Surface.info:id", "token")
            .unwrap()
            .set(Value::Token("UsdPreviewSurface".into()))
            .unwrap();
        stage.define_prim("/Mat/Xf").unwrap().set_type_name("Shader").unwrap();
        stage
            .create_attribute("/Mat/Xf.info:id", "token")
            .unwrap()
            .set(Value::Token("UsdTransform2d".into()))
            .unwrap();
        stage
            .create_attribute("/Mat/Xf.inputs:scale", "float2")
            .unwrap()
            .set(Value::Vec2f([2.0f32, 3.0f32].into()))
            .unwrap();
        stage
            .create_attribute("/Mat/Xf.inputs:translation", "float2")
            .unwrap()
            .set(Value::Vec2f([0.5f32, 0.25f32].into()))
            .unwrap();
        stage
            .create_attribute("/Mat/Xf.inputs:rotation", "float")
            .unwrap()
            .set(Value::Float(90.0))
            .unwrap();

        let read = read_preview_material(&stage, &Path::new("/Mat").unwrap())
            .unwrap()
            .expect("material");
        let uv = read.uv_transform.expect("uv transform");
        assert_eq!(uv.scale, [2.0, 3.0]);
        assert_eq!(uv.translation, [0.5, 0.25]);
        assert_eq!(uv.rotation_deg, 90.0);
    }

    #[test]
    fn no_transform2d_yields_none() {
        let stage = Stage::builder().in_memory("uv2.usda").unwrap();
        stage.define_prim("/Mat").unwrap().set_type_name("Material").unwrap();
        stage
            .create_attribute("/Mat.outputs:surface", "token")
            .unwrap()
            .set_connections([Path::new("/Mat/Surface.outputs:surface").unwrap()])
            .unwrap();
        stage.define_prim("/Mat/Surface").unwrap().set_type_name("Shader").unwrap();
        stage
            .create_attribute("/Mat/Surface.info:id", "token")
            .unwrap()
            .set(Value::Token("UsdPreviewSurface".into()))
            .unwrap();

        let read = read_preview_material(&stage, &Path::new("/Mat").unwrap())
            .unwrap()
            .expect("material");
        assert!(read.uv_transform.is_none());
    }
}
