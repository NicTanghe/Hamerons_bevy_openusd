//! Discovery and compilation of standalone MaterialX documents referenced by USD.

use std::collections::BTreeSet;
use std::path::{Path as FsPath, PathBuf};

use bevy::prelude::Resource;
use materialx_wesl::{
    Compiler, CompilerLimits, Diagnostic as CoreDiagnostic, DiagnosticCode as CoreCode, Document,
    Registry, Severity as CoreSeverity, Value as CoreValue,
};
use openusd::sdf::{FieldKey, Path, Value};
use openusd::usd::Stage;

use super::asset::MaterialXAssetReader;
use super::compiler::{
    CompileFailure, CompiledMaterialX, CompiledTexture, MAX_TEXTURES, MAX_UNIFORMS,
};
use super::diagnostic::{DiagnosticCode, MaterialXDiagnostic, Severity};
use super::emit;

/// The renderer-neutral registry is parsed once and shared by material routes.
#[derive(Resource, Debug, Clone)]
pub struct MaterialXDocumentRegistry(pub Registry);

impl Default for MaterialXDocumentRegistry {
    fn default() -> Self {
        Self(Registry::default())
    }
}

/// An exact material inside one standalone MaterialX document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalMaterialXSource {
    pub document: PathBuf,
    pub material_name: String,
}

/// Locate the `.mtlx` reference that introduces a bound USD material.
///
/// OpenUSD-rs deliberately reports a MaterialX XML file as an unsupported USD
/// layer, but it retains the authoring USD prim spec and its reference list-op.
/// Walking the material and its ancestors therefore recovers both the source
/// layer (the asset-resolution anchor) and the authored `.mtlx` path without
/// teaching the USD core how to parse MaterialX.
pub fn find_external_materialx(
    stage: &Stage,
    material: &Path,
    asset_reader: Option<&MaterialXAssetReader>,
) -> Result<Option<ExternalMaterialXSource>, MaterialXDiagnostic> {
    let material_name = material.name().ok_or_else(|| {
        MaterialXDiagnostic::error(
            DiagnosticCode::MissingMaterial,
            material.as_str(),
            None,
            None,
            "bound MaterialX path has no material name",
        )
    })?;
    let mut cursor = Some(material.clone());
    while let Some(path) = cursor {
        let stack = stage.prim(path.clone()).prim_stack().map_err(|error| {
            MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                material.as_str(),
                Some(path.as_str().into()),
                None,
                format!("cannot inspect the USD prim stack: {error}"),
            )
        })?;
        let mut candidates = BTreeSet::new();
        for (layer_identifier, spec_path) in stack {
            let Some(layer) = stage.layer(&layer_identifier) else {
                continue;
            };
            let Some(spec) = layer.prim(spec_path) else {
                continue;
            };
            let references = spec.field(FieldKey::References.as_str()).map_err(|error| {
                MaterialXDiagnostic::error(
                    DiagnosticCode::InvalidConnection,
                    material.as_str(),
                    Some(path.as_str().into()),
                    Some("references".into()),
                    format!("cannot read the authored reference list: {error}"),
                )
            })?;
            let Some(Value::ReferenceListOp(references)) = references else {
                continue;
            };
            for reference in references.flatten() {
                if !is_materialx_path(&reference.asset_path) {
                    continue;
                }
                candidates.insert(resolve_reference(&layer_identifier, &reference.asset_path));
            }
        }
        match candidates.len() {
            0 => {}
            1 => {
                let document = candidates.into_iter().next().unwrap();
                let exists = document.is_file()
                    || asset_reader.is_some_and(|reader| reader.contains(&document));
                if !exists {
                    return Err(MaterialXDiagnostic::error(
                        DiagnosticCode::MissingAsset,
                        material.as_str(),
                        Some(path.as_str().into()),
                        Some(document.display().to_string()),
                        "referenced MaterialX document does not exist",
                    ));
                }
                return Ok(Some(ExternalMaterialXSource {
                    document,
                    material_name: material_name.to_owned(),
                }));
            }
            count => {
                return Err(MaterialXDiagnostic::error(
                    DiagnosticCode::AmbiguousMaterial,
                    material.as_str(),
                    Some(path.as_str().into()),
                    Some("references".into()),
                    format!("{count} MaterialX documents contribute at the same USD scope"),
                ));
            }
        }
        cursor = path.parent();
    }
    Ok(None)
}

/// Compile one discovered standalone MaterialX material for the Bevy adapter.
pub fn compile_external_materialx(
    source: &ExternalMaterialXSource,
    material_path: &Path,
    registry: &MaterialXDocumentRegistry,
    asset_reader: Option<&MaterialXAssetReader>,
) -> Result<CompiledMaterialX, CompileFailure> {
    let document = load_external_document(source, material_path, asset_reader)?;
    let limits = CompilerLimits {
        max_nodes: 1024,
        max_depth: 256,
        max_uniforms: MAX_UNIFORMS,
        max_textures: MAX_TEXTURES,
    };
    let asset_exists =
        |path: &FsPath| path.is_file() || asset_reader.is_some_and(|reader| reader.contains(path));
    let compiled = Compiler::with_limits(&registry.0, limits)
        .with_asset_exists(&asset_exists)
        .compile(&document, &source.material_name)
        .map_err(|failure| CompileFailure {
            diagnostics: failure
                .diagnostics
                .into_iter()
                .map(|diagnostic| adapt_diagnostic(diagnostic, material_path))
                .collect(),
        })?;

    let uniforms = compiled
        .uniforms
        .iter()
        .map(|uniform| {
            core_value_to_vec4(&uniform.value).map_err(|message| CompileFailure {
                diagnostics: vec![MaterialXDiagnostic::error(
                    DiagnosticCode::UnsupportedFeature,
                    material_path.as_str(),
                    uniform.provenance.scope.clone(),
                    Some(uniform.name.clone()),
                    message,
                )],
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let textures = compiled
        .textures
        .iter()
        .map(|texture| CompiledTexture {
            path: texture.resolved_path.display().to_string(),
            is_srgb: texture
                .authored_colorspace
                .as_deref()
                .is_some_and(is_srgb_colorspace),
            u_address_mode: texture.u_address_mode.clone(),
            v_address_mode: texture.v_address_mode.clone(),
            filter_type: texture.filter_type.clone(),
        })
        .collect();
    let uses_normal =
        compiled.context.geometric_normal || compiled.context.tangent || compiled.context.bitangent;
    let wesl = emit::external_root_module(&compiled, uses_normal);

    Ok(CompiledMaterialX {
        material: material_path.as_str().into(),
        graph_key: compiled.graph_key.0,
        wesl,
        uniforms,
        textures,
        alpha_blend: matches!(compiled.alpha_mode, materialx_wesl::AlphaMode::Blend),
        transmission: compiled.features.transmission,
        required_modules: compiled
            .required_modules
            .iter()
            .map(|module| module.module.clone())
            .collect(),
        diagnostics: compiled
            .diagnostics
            .into_iter()
            .map(|diagnostic| adapt_diagnostic(diagnostic, material_path))
            .collect(),
    })
}

fn load_external_document(
    source: &ExternalMaterialXSource,
    material_path: &Path,
    asset_reader: Option<&MaterialXAssetReader>,
) -> Result<Document, CompileFailure> {
    let document = if let Some(reader) = asset_reader {
        match reader.read(&source.document) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes).map_err(|error| CompileFailure {
                    diagnostics: vec![MaterialXDiagnostic::error(
                        DiagnosticCode::InvalidDocument,
                        material_path.as_str(),
                        None,
                        Some(source.document.display().to_string()),
                        format!("MaterialX document is not UTF-8: {error}"),
                    )],
                })?;
                Document::parse_with_source(&text, Some(source.document.clone()))
            }
            Err(error) if !source.document.is_file() => {
                return Err(CompileFailure {
                    diagnostics: vec![MaterialXDiagnostic::error(
                        DiagnosticCode::MissingAsset,
                        material_path.as_str(),
                        None,
                        Some(source.document.display().to_string()),
                        format!("cannot read MaterialX document: {error}"),
                    )],
                });
            }
            Err(_) => Document::load(&source.document),
        }
    } else {
        Document::load(&source.document)
    };

    document.map_err(|diagnostics| CompileFailure {
        diagnostics: diagnostics
            .into_vec()
            .into_iter()
            .map(|diagnostic| adapt_diagnostic(diagnostic, material_path))
            .collect(),
    })
}

fn is_materialx_path(path: &str) -> bool {
    FsPath::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mtlx"))
}

fn resolve_reference(layer_identifier: &str, authored_path: &str) -> PathBuf {
    let authored = PathBuf::from(authored_path);
    if authored.is_absolute() {
        authored
    } else {
        FsPath::new(layer_identifier)
            .parent()
            .unwrap_or_else(|| FsPath::new(""))
            .join(authored)
    }
}

fn is_srgb_colorspace(colorspace: &str) -> bool {
    matches!(
        colorspace.to_ascii_lowercase().as_str(),
        "srgb" | "srgb_texture" | "srgbtexture"
    )
}

fn core_value_to_vec4(value: &CoreValue) -> Result<[f32; 4], String> {
    Ok(match value {
        CoreValue::Boolean(value) => [u8::from(*value) as f32, 0.0, 0.0, 0.0],
        CoreValue::Integer(value) => [*value as f32, 0.0, 0.0, 0.0],
        CoreValue::Float(value) => [*value, 0.0, 0.0, 0.0],
        CoreValue::Vector2(value) => [value[0], value[1], 0.0, 0.0],
        CoreValue::Vector3(value) | CoreValue::Color3(value) => [value[0], value[1], value[2], 0.0],
        CoreValue::Vector4(value) | CoreValue::Color4(value) => *value,
        other => {
            return Err(format!(
                "MaterialX uniform type {} does not fit the Bevy adapter's vec4 value slots",
                other.value_type()
            ));
        }
    })
}

fn adapt_diagnostic(diagnostic: CoreDiagnostic, material_path: &Path) -> MaterialXDiagnostic {
    let message = match diagnostic.location.as_ref() {
        Some(location) => {
            let document = location
                .document
                .as_deref()
                .map_or_else(|| "<MaterialX>".into(), |path| path.display().to_string());
            format!(
                "{} ({}:{}:{})",
                diagnostic.message, document, location.line, location.column
            )
        }
        None => diagnostic.message,
    };
    MaterialXDiagnostic {
        severity: match diagnostic.severity {
            CoreSeverity::Warning => Severity::Warning,
            CoreSeverity::Error => Severity::Error,
        },
        code: adapt_code(diagnostic.code),
        material: diagnostic
            .material
            .unwrap_or_else(|| material_path.as_str().into()),
        node: diagnostic.node,
        property: diagnostic.property,
        message,
    }
}

fn adapt_code(code: CoreCode) -> DiagnosticCode {
    match code {
        CoreCode::Io
        | CoreCode::XmlSyntax
        | CoreCode::InvalidDocument
        | CoreCode::MissingName
        | CoreCode::DuplicateName
        | CoreCode::InvalidType
        | CoreCode::InvalidValue
        | CoreCode::ConflictingSource => DiagnosticCode::InvalidDocument,
        CoreCode::MissingMaterial => DiagnosticCode::MissingMaterial,
        CoreCode::AmbiguousMaterial => DiagnosticCode::AmbiguousMaterial,
        CoreCode::MissingNode | CoreCode::UnknownNode | CoreCode::UnsupportedNode => {
            DiagnosticCode::UnknownNodeDef
        }
        CoreCode::AmbiguousNode => DiagnosticCode::AmbiguousConnection,
        CoreCode::MissingOutput => DiagnosticCode::UnknownOutput,
        CoreCode::MissingInput => DiagnosticCode::MissingValue,
        CoreCode::InvalidConnection => DiagnosticCode::InvalidConnection,
        CoreCode::ConnectionCycle => DiagnosticCode::ConnectionCycle,
        CoreCode::TypeMismatch => DiagnosticCode::TypeMismatch,
        CoreCode::UnsupportedFeature | CoreCode::StandardSurfaceApproximation => {
            DiagnosticCode::UnsupportedFeature
        }
        CoreCode::MissingTexture | CoreCode::RelativeAssetWithoutSource => {
            DiagnosticCode::MissingAsset
        }
        CoreCode::ResourceLimit => DiagnosticCode::ResourceLimit,
        CoreCode::Registry => DiagnosticCode::UnsupportedImplementation,
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;
    use crate::read::shade::read_material_binding;

    struct PackageResolver<'a>(wesl::VirtualResolver<'a>);

    impl wesl::Resolver for PackageResolver<'_> {
        fn resolve_source<'a>(
            &'a self,
            path: &wesl::ModulePath,
        ) -> Result<Cow<'a, str>, wesl::ResolveError> {
            self.0.resolve_source(&self.canonical_path(path))
        }

        fn canonical_path(&self, path: &wesl::ModulePath) -> wesl::ModulePath {
            match &path.origin {
                wesl::syntax::PathOrigin::Package(package) if package.contains('/') => {
                    wesl::ModulePath {
                        origin: wesl::syntax::PathOrigin::Package(
                            package.rsplit('/').next().unwrap().into(),
                        ),
                        components: path.components.clone(),
                    }
                }
                _ => path.clone(),
            }
        }
    }

    const FORWARD_IO: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4f,
    @location(3) world_position: vec4f,
    @location(0) world_normal: vec3f,
    @location(1) uv: vec2f,
    @location(2) world_tangent: vec4f,
    @location(4) @interpolate(flat) instance_index: u32,
}
struct FragmentOutput { @location(0) color: vec4f, }
"#;
    const MESH_FUNCTIONS: &str = r#"
fn get_world_from_local(instance_index: u32) -> mat4x4f {
    return mat4x4f(
        vec4f(1.0, 0.0, 0.0, 0.0),
        vec4f(0.0, 1.0, 0.0, 0.0),
        vec4f(0.0, 0.0, 1.0, 0.0),
        vec4f(0.0, 0.0, 0.0, 1.0),
    );
}
"#;
    const PBR_TYPES: &str = r#"
const STANDARD_MATERIAL_FLAGS_ALPHA_MODE_BLEND: u32 = 2u << 29u;
const STANDARD_MATERIAL_FLAGS_ATTENUATION_ENABLED_BIT: u32 = 1u << 13u;
struct StandardMaterial {
    base_color: vec4f,
    metallic: f32,
    perceptual_roughness: f32,
    emissive: vec4f,
    flags: u32,
    specular_transmission: f32,
    thickness: f32,
    ior: f32,
    reflectance: vec3f,
    attenuation_distance: f32,
    attenuation_color: vec4f,
}
struct PbrInput {
    material: StandardMaterial,
    world_normal: vec3f,
    N: vec3f,
}
"#;
    const PBR_FRAGMENT: &str = r#"
import super::forward_io::VertexOutput;
import super::pbr_types::{PbrInput, StandardMaterial};
fn pbr_input_from_vertex_output(in: VertexOutput, is_front: bool, double_sided: bool) -> PbrInput {
    return PbrInput(
        StandardMaterial(
            vec4f(1.0),
            0.0,
            0.5,
            vec4f(0.0, 0.0, 0.0, 1.0),
            0u,
            0.0,
            0.0,
            1.5,
            vec3f(0.5),
            0.0,
            vec4f(1.0),
        ),
        normalize(in.world_normal),
        normalize(in.world_normal),
    );
}
"#;
    const PBR_FUNCTIONS: &str = r#"
import super::pbr_types::PbrInput;
fn apply_pbr_lighting(input: PbrInput) -> vec4f {
    return input.material.base_color + input.material.emissive;
}
fn main_pass_post_lighting_processing(input: PbrInput, color: vec4f) -> vec4f {
    return color;
}
"#;

    fn validate_wesl(compiled: &CompiledMaterialX, registry: &Registry) {
        let mut resolver = wesl::VirtualResolver::new();
        for module in registry.translator_modules() {
            resolver.add_module(
                format!("materialx::{}", module.module).parse().unwrap(),
                Cow::Borrowed(module.source),
            );
        }
        for (module, source) in [
            ("constants", "const MATERIAL_BIND_GROUP: u32 = 2u;"),
            ("bevy_pbr::render::forward_io", FORWARD_IO),
            ("bevy_pbr::render::pbr_types", PBR_TYPES),
            ("bevy_pbr::render::pbr_fragment", PBR_FRAGMENT),
            ("bevy_pbr::render::pbr_functions", PBR_FUNCTIONS),
            ("bevy_pbr::render::mesh_functions", MESH_FUNCTIONS),
        ] {
            resolver.add_module(module.parse().unwrap(), Cow::Borrowed(source));
        }
        resolver.add_module(
            "usd_bevy::external_graph".parse().unwrap(),
            Cow::Owned(compiled.wesl.clone()),
        );
        let mut compiler = wesl::Wesl::new("").set_custom_resolver(PackageResolver(resolver));
        compiler.set_feature("VERTEX_UVS_A", true);
        compiler.set_feature("VERTEX_TANGENTS", true);
        compiler
            .compile(&"usd_bevy::external_graph".parse().unwrap())
            .unwrap_or_else(|error| panic!("Bevy external MaterialX WESL failed: {error}"));
    }

    #[test]
    fn open_chessboard_binding_discovers_and_wraps_external_materialx() {
        let chess_set = FsPath::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets/full_assets/OpenChessSet/chess_set.usda");
        if !chess_set.is_file() {
            eprintln!("skipping sibling OpenChessSet integration test");
            return;
        }
        let stage = Stage::open(chess_set.to_str().unwrap()).unwrap();
        let render = Path::new("/ChessSet/Chessboard/Geom/Render").unwrap();
        let binding = read_material_binding(&stage, &render)
            .unwrap()
            .expect("chessboard render mesh has a material binding");
        let source = find_external_materialx(&stage, &binding, None)
            .unwrap()
            .expect("binding ancestry contains a .mtlx reference");
        assert_eq!(source.material_name, "M_Chessboard");
        assert!(source.document.ends_with("Chessboard_mat.mtlx"));

        let registry = MaterialXDocumentRegistry::default();
        let compiled = compile_external_materialx(&source, &binding, &registry, None).unwrap();
        assert_eq!(compiled.textures.len(), 4);
        assert!(compiled.textures[0].is_srgb);
        assert!(compiled.wesl.contains("@size(16) thickness: f32"));
        assert!(!compiled.wesl.contains("webgl_padding"));
        assert!(
            compiled.textures[1..]
                .iter()
                .all(|texture| !texture.is_srgb)
        );
        assert!(compiled.wesl.contains("fn fragment("));
        validate_wesl(&compiled, &registry.0);
    }

    #[test]
    fn open_chess_pawn_top_preserves_transmission_closure_for_bevy() {
        let document = FsPath::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets/full_assets/OpenChessSet/assets/Pawn/Pawn_mat.mtlx");
        if !document.is_file() {
            eprintln!("skipping sibling OpenChessSet integration test");
            return;
        }
        let source = ExternalMaterialXSource {
            document,
            material_name: "M_Pawn_Top_B".into(),
        };
        let material = Path::new("/ChessSet/Black/Pawns/Pawn/Looks/M_Pawn_Top_B").unwrap();
        let registry = MaterialXDocumentRegistry::default();
        let compiled = compile_external_materialx(&source, &material, &registry, None).unwrap();

        assert!(compiled.transmission);
        assert!(!compiled.alpha_blend, "transmission is not alpha opacity");
        assert!(compiled.wesl.contains("mx.transmission.weight"));
        assert!(compiled.wesl.contains("mx.transmission.color"));
        assert!(compiled.wesl.contains("specular_transmission"));
        assert!(compiled.wesl.contains("materialx_renderer.thickness"));
        assert!(compiled.wesl.contains("mx.thin_walled"));
        assert!(compiled.wesl.contains("get_world_from_local"));
        assert!(
            compiled
                .required_modules
                .iter()
                .any(|module| module == "pbrlib::mx_roughness_anisotropy")
        );
        validate_wesl(&compiled, &registry.0);
    }

    #[test]
    fn compiles_document_and_texture_from_host_asset_reader() {
        const DOCUMENT: &str = r#"<materialx>
  <image name="I" type="color3">
    <input name="file" type="filename" value="texture.png" />
  </image>
  <standard_surface name="S" type="surfaceshader">
    <input name="base_color" type="color3" nodename="I" />
  </standard_surface>
  <surfacematerial name="M" type="material">
    <input name="surfaceshader" type="surfaceshader" nodename="S" />
  </surfacematerial>
</materialx>"#;
        let reader = MaterialXAssetReader::new(|path| {
            if path.ends_with("material.mtlx") {
                Ok(DOCUMENT.as_bytes().to_vec())
            } else if path.ends_with("texture.png") {
                Ok(vec![0])
            } else {
                Err(format!("{} is unavailable", path.display()))
            }
        });
        let source = ExternalMaterialXSource {
            document: PathBuf::from("virtual/material.mtlx"),
            material_name: "M".into(),
        };
        let material = Path::new("/M").unwrap();
        let registry = MaterialXDocumentRegistry::default();

        let compiled =
            compile_external_materialx(&source, &material, &registry, Some(&reader)).unwrap();

        assert_eq!(compiled.textures.len(), 1);
        assert!(compiled.textures[0].path.ends_with("virtual/texture.png"));
    }
}
