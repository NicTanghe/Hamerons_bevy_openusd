//! Composed UsdShade-to-MaterialX graph compiler.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};

use bevy::platform::hash::FixedHasher;
use openusd::sdf::{Path, Value};
use openusd::usd::{Stage, TimeCode};

use super::diagnostic::{DiagnosticCode, MaterialXDiagnostic};
use super::emit;
use super::registry::{Coverage, MaterialXRegistry, Port};

pub const MAX_UNIFORMS: usize = 64;
pub const MAX_TEXTURES: usize = 4;

/// One texture and its MaterialX sampling metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CompiledTexture {
    pub path: String,
    pub is_srgb: bool,
    pub u_address_mode: String,
    pub v_address_mode: String,
    pub filter_type: String,
}

/// A compiled graph plus its runtime values and resource dependencies.
#[derive(Debug, Clone)]
pub struct CompiledMaterialX {
    pub material: String,
    /// Stable across changes to authored uniform values and texture paths.
    pub graph_key: u64,
    pub wesl: String,
    pub uniforms: Vec<[f32; 4]>,
    pub textures: Vec<CompiledTexture>,
    pub alpha_blend: bool,
    pub required_modules: Vec<String>,
    pub diagnostics: Vec<MaterialXDiagnostic>,
}

/// A failed compilation. All failures are structured and location-aware.
#[derive(Debug, Clone)]
pub struct CompileFailure {
    pub diagnostics: Vec<MaterialXDiagnostic>,
}

impl std::fmt::Display for CompileFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(first) = self.diagnostics.first() {
            first.fmt(formatter)
        } else {
            formatter.write_str("MaterialX compilation failed")
        }
    }
}

impl std::error::Error for CompileFailure {}

/// Compile the MaterialX surface selected from `material` at `time`.
///
/// `outputs:mtlx:surface` is selected first. Only when it is unconnected is
/// the universal `outputs:surface` considered.
pub fn compile_materialx(
    stage: &Stage,
    material: &Path,
    time: Option<f64>,
    registry: &MaterialXRegistry,
) -> Result<CompiledMaterialX, CompileFailure> {
    Compiler::new(stage, material, time, registry).compile()
}

/// Cheap routing predicate. An explicit MaterialX terminal always opts in;
/// the universal fallback opts in when it directly names a registered
/// MaterialX shader or a NodeGraph interface.
pub fn has_materialx_terminal(
    stage: &Stage,
    material: &Path,
    registry: &MaterialXRegistry,
) -> bool {
    let connections = |name: &str| {
        let Ok(path) = material.append_property(name) else {
            return Vec::new();
        };
        let Some((prim, property)) = path.split_property() else {
            return Vec::new();
        };
        stage
            .prim(prim)
            .attribute(property)
            .connections()
            .unwrap_or_default()
    };
    if !connections("outputs:mtlx:surface").is_empty() {
        return true;
    }
    let universal = connections("outputs:surface");
    let [source] = universal.as_slice() else {
        return false;
    };
    let prim = source.prim_path();
    if stage
        .prim(prim.clone())
        .type_name()
        .ok()
        .flatten()
        .is_some_and(|name| name.as_str() == "NodeGraph")
    {
        return true;
    }
    let id = stage
        .prim(prim)
        .attribute("info:id")
        .get::<Value>()
        .ok()
        .flatten()
        .and_then(|value| match value {
            Value::Token(id) => Some(id.as_str().to_string()),
            Value::String(id) => Some(id),
            _ => None,
        });
    id.is_some_and(|id| registry.node(&id).is_some())
}

#[derive(Debug, Clone)]
pub(super) struct Expression {
    pub(super) code: String,
    materialx_type: String,
}

#[derive(Debug)]
pub(super) struct SurfaceExpressions {
    pub(super) base_color: Expression,
    pub(super) metalness: Expression,
    pub(super) roughness: Expression,
    pub(super) emission: Expression,
    pub(super) opacity: Expression,
    pub(super) normal: Option<Expression>,
    pub(super) alpha_blend: bool,
}

struct Compiler<'a> {
    stage: &'a Stage,
    material: &'a Path,
    time: Option<TimeCode>,
    registry: &'a MaterialXRegistry,
    uniforms: Vec<[f32; 4]>,
    textures: Vec<CompiledTexture>,
    lines: Vec<String>,
    modules: BTreeSet<String>,
    imports: BTreeSet<(String, String)>,
    memo: HashMap<String, Expression>,
    visiting: HashSet<String>,
    diagnostics: Vec<MaterialXDiagnostic>,
    next_symbol: usize,
}

impl<'a> Compiler<'a> {
    fn new(
        stage: &'a Stage,
        material: &'a Path,
        time: Option<f64>,
        registry: &'a MaterialXRegistry,
    ) -> Self {
        Self {
            stage,
            material,
            time: time.map(TimeCode::new),
            registry,
            uniforms: Vec::new(),
            textures: Vec::new(),
            lines: Vec::new(),
            modules: BTreeSet::new(),
            imports: BTreeSet::new(),
            memo: HashMap::new(),
            visiting: HashSet::new(),
            diagnostics: Vec::new(),
            next_symbol: 0,
        }
    }

    fn compile(mut self) -> Result<CompiledMaterialX, CompileFailure> {
        let terminal = self.select_terminal()?;
        let surface_node = self.resolve_surface_node(&terminal)?;
        let surface = self.lower_standard_surface(&surface_node)?;
        let required_modules: Vec<String> = self.modules.into_iter().collect();
        let wesl = emit::root_module(&self.imports, &self.lines, &surface);
        let mut hasher = FixedHasher.build_hasher();
        wesl.hash(&mut hasher);
        self.textures.len().hash(&mut hasher);
        let graph_key = hasher.finish();

        Ok(CompiledMaterialX {
            material: self.material.as_str().into(),
            graph_key,
            wesl,
            uniforms: self.uniforms,
            textures: self.textures,
            alpha_blend: surface.alpha_blend,
            required_modules,
            diagnostics: self.diagnostics,
        })
    }

    fn select_terminal(&mut self) -> Result<Path, CompileFailure> {
        for name in ["outputs:mtlx:surface", "outputs:surface"] {
            let path = self
                .material
                .append_property(name)
                .map_err(|error| self.internal_error(name, error))?;
            let connections = self.connections(&path)?;
            if connections.len() > 1 {
                return self.fail(MaterialXDiagnostic::error(
                    DiagnosticCode::AmbiguousConnection,
                    self.material.as_str(),
                    None,
                    Some(path.as_str().into()),
                    format!(
                        "surface terminal has {} composed sources; exactly one is required",
                        connections.len()
                    ),
                ));
            }
            if let Some(connection) = connections.into_iter().next() {
                return Ok(connection);
            }
        }
        self.fail(MaterialXDiagnostic::error(
            DiagnosticCode::MissingTerminal,
            self.material.as_str(),
            None,
            None,
            "neither outputs:mtlx:surface nor universal outputs:surface is connected",
        ))
    }

    fn resolve_surface_node(&mut self, terminal: &Path) -> Result<Path, CompileFailure> {
        let (prim, property) = terminal.split_property().ok_or_else(|| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                self.material.as_str(),
                None,
                Some(terminal.as_str().into()),
                "surface connection target is not a property path",
            ))
        })?;
        if !property.starts_with("outputs:") {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(terminal.as_str().into()),
                "surface terminal must connect to a shader output",
            ));
        }
        let id = self.shader_id(&prim)?;
        if id == "ND_surfacematerial" {
            let input = prim
                .append_property("inputs:surfaceshader")
                .map_err(|error| self.internal_error(prim.as_str(), error))?;
            let sources = self.exactly_one_connection(&input)?;
            return Ok(sources);
        }
        Ok(terminal.clone())
    }

    fn lower_standard_surface(
        &mut self,
        output: &Path,
    ) -> Result<SurfaceExpressions, CompileFailure> {
        let prim = output.prim_path();
        let id = self.shader_id(&prim)?;
        if id != "ND_standard_surface_surfaceshader" {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::UnknownNodeDef,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(output.as_str().into()),
                format!("expected ND_standard_surface_surfaceshader, found {id}"),
            ));
        }
        self.verify_id_implementation(&prim)?;
        let descriptor = self.registry.node(&id).cloned().ok_or_else(|| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::UnknownNodeDef,
                self.material.as_str(),
                Some(prim.as_str().into()),
                None,
                format!("NodeDef {id} is absent from the MaterialX registry"),
            ))
        })?;

        let (base, _) = self.lower_input(&prim, port(&descriptor, "base")?)?;
        let (base_color, _) = self.lower_input(&prim, port(&descriptor, "base_color")?)?;
        let (metalness, _) = self.lower_input(&prim, port(&descriptor, "metalness")?)?;
        let (roughness, _) = self.lower_input(&prim, port(&descriptor, "specular_roughness")?)?;
        let (emission_weight, _) = self.lower_input(&prim, port(&descriptor, "emission")?)?;
        let (emission_color, _) = self.lower_input(&prim, port(&descriptor, "emission_color")?)?;
        let (opacity_color, opacity_active) =
            self.lower_input(&prim, port(&descriptor, "opacity")?)?;
        let (normal, normal_active) = self.lower_input(&prim, port(&descriptor, "normal")?)?;

        self.diagnostics.push(MaterialXDiagnostic::warning(
            DiagnosticCode::UnsupportedFeature,
            self.material.as_str(),
            Some(prim.as_str().into()),
            None,
            "MaterialX Standard Surface is approximated through Bevy PBR; unsupported lobes use Bevy defaults",
        ));
        if normal_active {
            self.diagnostics.push(MaterialXDiagnostic::warning(
                DiagnosticCode::UnsupportedFeature,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some("inputs:normal".into()),
                "the first slice treats the compiled normal as world-space",
            ));
        }

        Ok(SurfaceExpressions {
            base_color: Expression {
                code: format!("({} * vec3f({}))", base_color.code, base.code),
                materialx_type: "color3".into(),
            },
            metalness,
            roughness,
            emission: Expression {
                code: format!(
                    "({} * vec3f({}))",
                    emission_color.code, emission_weight.code
                ),
                materialx_type: "color3".into(),
            },
            opacity: Expression {
                code: format!("dot({}, vec3f(0.299, 0.587, 0.114))", opacity_color.code),
                materialx_type: "float".into(),
            },
            normal: normal_active.then_some(normal),
            alpha_blend: opacity_active,
        })
    }

    fn lower_input(
        &mut self,
        prim: &Path,
        port: &Port,
    ) -> Result<(Expression, bool), CompileFailure> {
        let attribute = prim
            .append_property(format!("inputs:{}", port.name))
            .map_err(|error| self.internal_error(prim.as_str(), error))?;
        let connections = self.connections(&attribute)?;
        if connections.len() > 1 {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::AmbiguousConnection,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(attribute.as_str().into()),
                format!(
                    "input has {} composed sources; exactly one is required",
                    connections.len()
                ),
            ));
        }
        if let Some(source) = connections.into_iter().next() {
            let expression = self.lower_source(&source)?;
            self.require_type(&expression, port, &attribute)?;
            return Ok((expression, true));
        }

        let attr = self
            .stage
            .prim(prim.clone())
            .attribute(&format!("inputs:{}", port.name));
        let value = attr
            .get_at::<Value>(self.time)
            .map_err(|error| self.read_error(&attribute, error))?;
        if let Some(value) = value {
            let expression = self.uniform_expression(port, value, &attribute)?;
            return Ok((expression, true));
        }
        let default = port.default_value.as_deref().ok_or_else(|| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::MissingValue,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(attribute.as_str().into()),
                format!(
                    "input {} has no connection, authored value, or NodeDef default",
                    port.name
                ),
            ))
        })?;
        let data = parse_default(port, default).map_err(|message| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::TypeMismatch,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(attribute.as_str().into()),
                message,
            ))
        })?;
        let expression = self.push_uniform(port, data, &attribute)?;
        Ok((expression, false))
    }

    fn lower_source(&mut self, source: &Path) -> Result<Expression, CompileFailure> {
        let key = source.as_str().to_string();
        if let Some(expression) = self.memo.get(&key) {
            return Ok(expression.clone());
        }
        if !self.visiting.insert(key.clone()) {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::ConnectionCycle,
                self.material.as_str(),
                Some(source.prim_path().as_str().into()),
                Some(key),
                "cycle detected while lowering MaterialX connections",
            ));
        }

        let result = self.lower_source_uncached(source);
        self.visiting.remove(source.as_str());
        if let Ok(expression) = &result {
            self.memo.insert(key, expression.clone());
        }
        result
    }

    fn lower_source_uncached(&mut self, source: &Path) -> Result<Expression, CompileFailure> {
        let (prim, property) = source.split_property().ok_or_else(|| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                self.material.as_str(),
                None,
                Some(source.as_str().into()),
                "connection target is not a property path",
            ))
        })?;
        let prim_type = self
            .stage
            .prim(prim.clone())
            .type_name()
            .map_err(|error| self.read_error(source, error))?
            .map(|token| token.as_str().to_string());

        // Material and NodeGraph outputs are interfaces. Follow their
        // composed source before attempting to interpret their owning prim as
        // a shader node.
        if matches!(prim_type.as_deref(), Some("Material" | "NodeGraph"))
            || property.starts_with("inputs:")
        {
            let connections = self.connections(source)?;
            if connections.len() > 1 {
                return self.fail(MaterialXDiagnostic::error(
                    DiagnosticCode::AmbiguousConnection,
                    self.material.as_str(),
                    Some(prim.as_str().into()),
                    Some(source.as_str().into()),
                    "interface property resolves to multiple sources",
                ));
            }
            if let Some(next) = connections.into_iter().next() {
                return self.lower_source(&next);
            }
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(source.as_str().into()),
                "interface property has no value-producing connection",
            ));
        }

        if prim_type.as_deref() != Some("Shader") || !property.starts_with("outputs:") {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(source.as_str().into()),
                "connection target is not a Shader output",
            ));
        }
        self.lower_node(&prim, property.trim_start_matches("outputs:"))
    }

    fn lower_node(&mut self, prim: &Path, output_name: &str) -> Result<Expression, CompileFailure> {
        let id = self.shader_id(prim)?;
        self.verify_id_implementation(prim)?;
        let descriptor = self.registry.node(&id).cloned().ok_or_else(|| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::UnknownNodeDef,
                self.material.as_str(),
                Some(prim.as_str().into()),
                None,
                format!("NodeDef {id} is absent from the MaterialX registry"),
            ))
        })?;

        let output = descriptor
            .outputs
            .iter()
            .find(|port| port.name == output_name)
            .or_else(|| (descriptor.outputs.len() == 1).then(|| &descriptor.outputs[0]))
            .ok_or_else(|| {
                self.failure(MaterialXDiagnostic::error(
                    DiagnosticCode::UnknownOutput,
                    self.material.as_str(),
                    Some(prim.as_str().into()),
                    Some(format!("outputs:{output_name}")),
                    format!("NodeDef {id} has no output named {output_name}"),
                ))
            })?
            .clone();

        if id == "ND_texcoord_vector2" {
            return Ok(Expression {
                code: "uv".into(),
                materialx_type: output.materialx_type,
            });
        }
        if id == "ND_image_color3" {
            return self.lower_image_color3(prim, &descriptor, &output);
        }
        if !matches!(descriptor.status.as_str(), "translated" | "composed") {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::UnsupportedFeature,
                self.material.as_str(),
                Some(prim.as_str().into()),
                None,
                format!(
                    "NodeDef {id} is {}: {}",
                    descriptor.status,
                    descriptor.reason.as_deref().unwrap_or("no implementation")
                ),
            ));
        }
        let module = descriptor.module.clone().expect("validated module");
        let symbol = descriptor.symbol.clone().expect("validated symbol");
        let mut arguments = Vec::new();
        for input in &descriptor.inputs {
            if input.wesl_type.is_none() {
                return self.fail(MaterialXDiagnostic::error(
                    DiagnosticCode::UnsupportedFeature,
                    self.material.as_str(),
                    Some(prim.as_str().into()),
                    Some(format!("inputs:{}", input.name)),
                    format!("host-only input on {id} needs special lowering"),
                ));
            }
            arguments.push(self.lower_input(prim, input)?.0.code);
        }
        self.modules.insert(module.clone());
        self.imports.insert((module, symbol.clone()));
        let variable = self.allocate_symbol();
        self.lines.push(format!(
            "    let {variable} = {symbol}({});",
            arguments.join(", ")
        ));
        Ok(Expression {
            code: variable,
            materialx_type: output.materialx_type,
        })
    }

    fn lower_image_color3(
        &mut self,
        prim: &Path,
        descriptor: &Coverage,
        output: &Port,
    ) -> Result<Expression, CompileFailure> {
        if self.textures.len() == MAX_TEXTURES {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::ResourceLimit,
                self.material.as_str(),
                Some(prim.as_str().into()),
                None,
                format!("first slice supports at most {MAX_TEXTURES} image nodes per material"),
            ));
        }
        let file_port = port(descriptor, "file")?;
        let file_attr = prim
            .append_property("inputs:file")
            .map_err(|error| self.internal_error(prim.as_str(), error))?;
        if !self.connections(&file_attr)?.is_empty() {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::UnsupportedFeature,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(file_attr.as_str().into()),
                "connected filename inputs are not supported in the first slice",
            ));
        }
        let value = self
            .stage
            .prim(prim.clone())
            .attribute("inputs:file")
            .get_at::<Value>(self.time)
            .map_err(|error| self.read_error(&file_attr, error))?;
        let (file, unresolved_asset) = match value {
            Some(Value::AssetPath(path)) => (
                path.resolved_path()
                    .unwrap_or_else(|| path.asset_path())
                    .to_string(),
                path.resolved_path().is_none(),
            ),
            Some(Value::String(path)) => (path, false),
            Some(Value::Token(path)) => (path.as_str().to_string(), false),
            Some(other) => {
                return self.fail(MaterialXDiagnostic::error(
                    DiagnosticCode::TypeMismatch,
                    self.material.as_str(),
                    Some(prim.as_str().into()),
                    Some(file_attr.as_str().into()),
                    format!("filename input has incompatible value {other:?}"),
                ));
            }
            None => (file_port.default_value.clone().unwrap_or_default(), false),
        };
        if file.is_empty() {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::MissingAsset,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(file_attr.as_str().into()),
                "image node has no texture asset",
            ));
        }
        if unresolved_asset
            || (std::path::Path::new(&file).is_absolute() && !std::path::Path::new(&file).exists())
        {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::MissingAsset,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(file_attr.as_str().into()),
                format!("resolved texture asset does not exist: {file}"),
            ));
        }

        let texture_index = self.textures.len();
        self.textures.push(CompiledTexture {
            path: file,
            // Preserve the behavior of the original UsdShade vertical slice.
            // Standalone MaterialX documents provide exact colorspace metadata.
            is_srgb: true,
            u_address_mode: "periodic".into(),
            v_address_mode: "periodic".into(),
            filter_type: "linear".into(),
        });
        let default = self.lower_input(prim, port(descriptor, "default")?)?.0;
        let texcoord = self.lower_input(prim, port(descriptor, "texcoord")?)?.0;
        let module = descriptor.module.clone().expect("validated image module");
        let symbol = descriptor.symbol.clone().expect("validated image symbol");
        self.modules.insert(module.clone());
        self.imports.insert((module, symbol.clone()));
        let variable = self.allocate_symbol();
        self.lines.push(format!(
            "    let {variable} = {symbol}(materialx_texture_{texture_index}, materialx_sampler_{texture_index}, {}, {}, vec2f(1.0), vec2f(0.0));",
            default.code, texcoord.code
        ));
        Ok(Expression {
            code: variable,
            materialx_type: output.materialx_type.clone(),
        })
    }

    fn uniform_expression(
        &mut self,
        port: &Port,
        value: Value,
        attribute: &Path,
    ) -> Result<Expression, CompileFailure> {
        let data = value_to_uniform(port, value).map_err(|message| {
            self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::TypeMismatch,
                self.material.as_str(),
                Some(attribute.prim_path().as_str().into()),
                Some(attribute.as_str().into()),
                message,
            ))
        })?;
        self.push_uniform(port, data, attribute)
    }

    fn push_uniform(
        &mut self,
        port: &Port,
        data: [f32; 4],
        attribute: &Path,
    ) -> Result<Expression, CompileFailure> {
        if self.uniforms.len() == MAX_UNIFORMS {
            return self.fail(MaterialXDiagnostic::error(
                DiagnosticCode::ResourceLimit,
                self.material.as_str(),
                Some(attribute.prim_path().as_str().into()),
                Some(attribute.as_str().into()),
                format!("first slice supports at most {MAX_UNIFORMS} uniform values"),
            ));
        }
        let index = self.uniforms.len();
        self.uniforms.push(data);
        let code = match port.materialx_type.as_str() {
            "float" => format!("materialx.values[{index}].x"),
            "integer" => format!("i32(materialx.values[{index}].x)"),
            "boolean" => format!("materialx.values[{index}].x != 0.0"),
            "vector2" => format!("materialx.values[{index}].xy"),
            "color3" | "vector3" => format!("materialx.values[{index}].xyz"),
            "color4" | "vector4" => format!("materialx.values[{index}]"),
            other => {
                return self.fail(MaterialXDiagnostic::error(
                    DiagnosticCode::UnsupportedFeature,
                    self.material.as_str(),
                    Some(attribute.prim_path().as_str().into()),
                    Some(attribute.as_str().into()),
                    format!("uniform MaterialX type {other} is not supported yet"),
                ));
            }
        };
        Ok(Expression {
            code,
            materialx_type: port.materialx_type.clone(),
        })
    }

    fn require_type(
        &self,
        expression: &Expression,
        input: &Port,
        attribute: &Path,
    ) -> Result<(), CompileFailure> {
        if expression.materialx_type == input.materialx_type {
            return Ok(());
        }
        Err(self.failure(MaterialXDiagnostic::error(
            DiagnosticCode::TypeMismatch,
            self.material.as_str(),
            Some(attribute.prim_path().as_str().into()),
            Some(attribute.as_str().into()),
            format!(
                "input expects {}, connected output is {}",
                input.materialx_type, expression.materialx_type
            ),
        )))
    }

    fn shader_id(&self, prim: &Path) -> Result<String, CompileFailure> {
        let id_path = prim
            .append_property("info:id")
            .map_err(|error| self.internal_error(prim.as_str(), error))?;
        match self
            .stage
            .prim(prim.clone())
            .attribute("info:id")
            .get::<Value>()
            .map_err(|error| self.read_error(&id_path, error))?
        {
            Some(Value::Token(id)) => Ok(id.as_str().to_string()),
            Some(Value::String(id)) => Ok(id),
            _ => Err(self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::UnknownNodeDef,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(id_path.as_str().into()),
                "shader has no token/string info:id",
            ))),
        }
    }

    fn verify_id_implementation(&self, prim: &Path) -> Result<(), CompileFailure> {
        let attr = self
            .stage
            .prim(prim.clone())
            .attribute("info:implementationSource");
        let value = attr
            .get::<Value>()
            .map_err(|error| self.read_error(attr.path(), error))?;
        let source = match value {
            None => "id".to_string(),
            Some(Value::Token(source)) => source.as_str().to_string(),
            Some(Value::String(source)) => source,
            Some(other) => format!("{other:?}"),
        };
        if source == "id" {
            Ok(())
        } else {
            Err(self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::UnsupportedImplementation,
                self.material.as_str(),
                Some(prim.as_str().into()),
                Some(attr.path().as_str().into()),
                format!("implementationSource {source:?} is not a registry id"),
            )))
        }
    }

    fn exactly_one_connection(&self, attribute: &Path) -> Result<Path, CompileFailure> {
        let connections = self.connections(attribute)?;
        match connections.as_slice() {
            [source] => Ok(source.clone()),
            [] => Err(self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::MissingValue,
                self.material.as_str(),
                Some(attribute.prim_path().as_str().into()),
                Some(attribute.as_str().into()),
                "required input is not connected",
            ))),
            _ => Err(self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::AmbiguousConnection,
                self.material.as_str(),
                Some(attribute.prim_path().as_str().into()),
                Some(attribute.as_str().into()),
                format!("required input has {} sources", connections.len()),
            ))),
        }
    }

    fn connections(&self, attribute: &Path) -> Result<Vec<Path>, CompileFailure> {
        let Some((prim, name)) = attribute.split_property() else {
            return Err(self.failure(MaterialXDiagnostic::error(
                DiagnosticCode::InvalidConnection,
                self.material.as_str(),
                None,
                Some(attribute.as_str().into()),
                "expected an attribute property path",
            )));
        };
        self.stage
            .prim(prim)
            .attribute(name)
            .connections()
            .map_err(|error| self.read_error(attribute, error))
    }

    fn allocate_symbol(&mut self) -> String {
        let symbol = format!("mx_node_{}", self.next_symbol);
        self.next_symbol += 1;
        symbol
    }

    fn read_error(&self, property: &Path, error: anyhow::Error) -> CompileFailure {
        self.failure(MaterialXDiagnostic::error(
            DiagnosticCode::InvalidConnection,
            self.material.as_str(),
            Some(property.prim_path().as_str().into()),
            Some(property.as_str().into()),
            format!("failed to read composed USD property: {error:#}"),
        ))
    }

    fn internal_error(&self, location: &str, error: impl std::fmt::Display) -> CompileFailure {
        self.failure(MaterialXDiagnostic::error(
            DiagnosticCode::InvalidConnection,
            self.material.as_str(),
            None,
            Some(location.into()),
            format!("invalid USD path: {error}"),
        ))
    }

    fn failure(&self, diagnostic: MaterialXDiagnostic) -> CompileFailure {
        let mut diagnostics = self.diagnostics.clone();
        diagnostics.push(diagnostic);
        CompileFailure { diagnostics }
    }

    fn fail<T>(&self, diagnostic: MaterialXDiagnostic) -> Result<T, CompileFailure> {
        Err(self.failure(diagnostic))
    }
}

fn port<'a>(descriptor: &'a Coverage, name: &str) -> Result<&'a Port, CompileFailure> {
    descriptor
        .inputs
        .iter()
        .find(|port| port.name == name)
        .ok_or_else(|| CompileFailure {
            diagnostics: vec![MaterialXDiagnostic::error(
                DiagnosticCode::MissingValue,
                "<registry>",
                Some(descriptor.nodedef.clone()),
                Some(format!("inputs:{name}")),
                "host descriptor is missing a required port",
            )],
        })
}

fn parse_default(port: &Port, value: &str) -> Result<[f32; 4], String> {
    match port.materialx_type.as_str() {
        "boolean" => Ok([
            f32::from(matches!(value.trim(), "true" | "1")),
            0.0,
            0.0,
            0.0,
        ]),
        "integer" | "float" | "vector2" | "vector3" | "vector4" | "color3" | "color4" => {
            let values: Result<Vec<f32>, _> = value
                .split(',')
                .map(|part| part.trim().parse::<f32>())
                .collect();
            let values = values.map_err(|error| {
                format!("invalid {} default {value:?}: {error}", port.materialx_type)
            })?;
            let expected = component_count(&port.materialx_type).unwrap();
            if values.len() != expected {
                return Err(format!(
                    "{} default expects {expected} component(s), got {} in {value:?}",
                    port.materialx_type,
                    values.len()
                ));
            }
            let mut out = [0.0; 4];
            out[..values.len()].copy_from_slice(&values);
            Ok(out)
        }
        other => Err(format!(
            "MaterialX default type {other} is not GPU-uniform compatible"
        )),
    }
}

fn value_to_uniform(port: &Port, value: Value) -> Result<[f32; 4], String> {
    let result = match (port.materialx_type.as_str(), value) {
        ("boolean", Value::Bool(value)) => [f32::from(value), 0.0, 0.0, 0.0],
        ("integer", Value::Int(value)) => [value as f32, 0.0, 0.0, 0.0],
        ("integer", Value::Int64(value)) => [value as f32, 0.0, 0.0, 0.0],
        ("float", Value::Float(value)) => [value, 0.0, 0.0, 0.0],
        ("float", Value::Double(value)) => [value as f32, 0.0, 0.0, 0.0],
        ("vector2", Value::Vec2f(value)) => [value.x, value.y, 0.0, 0.0],
        ("vector2", Value::Vec2d(value)) => [value.x as f32, value.y as f32, 0.0, 0.0],
        ("color3" | "vector3", Value::Vec3f(value)) => [value.x, value.y, value.z, 0.0],
        ("color3" | "vector3", Value::Vec3d(value)) => {
            [value.x as f32, value.y as f32, value.z as f32, 0.0]
        }
        ("color4" | "vector4", Value::Vec4f(value)) => [value.x, value.y, value.z, value.w],
        ("color4" | "vector4", Value::Vec4d(value)) => [
            value.x as f32,
            value.y as f32,
            value.z as f32,
            value.w as f32,
        ],
        (expected, actual) => {
            return Err(format!(
                "expected MaterialX {expected}, got authored USD value {actual:?}"
            ));
        }
    };
    Ok(result)
}

fn component_count(materialx_type: &str) -> Option<usize> {
    match materialx_type {
        "integer" | "float" => Some(1),
        "vector2" => Some(2),
        "vector3" | "color3" => Some(3),
        "vector4" | "color4" => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    struct BevyTestResolver<'a>(wesl::VirtualResolver<'a>);

    impl wesl::Resolver for BevyTestResolver<'_> {
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

    const FORWARD_IO_STUB: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
}
struct FragmentOutput {
    @location(0) color: vec4f,
}
"#;
    const PBR_TYPES_STUB: &str = r#"
const STANDARD_MATERIAL_FLAGS_ALPHA_MODE_BLEND: u32 = 2u << 29u;
struct StandardMaterial {
    base_color: vec4f,
    metallic: f32,
    perceptual_roughness: f32,
    emissive: vec4f,
    flags: u32,
}
struct PbrInput {
    material: StandardMaterial,
    N: vec3f,
}
"#;
    const PBR_FRAGMENT_STUB: &str = r#"
import super::forward_io::VertexOutput;
import super::pbr_types::{PbrInput, StandardMaterial};
fn pbr_input_from_vertex_output(in: VertexOutput, is_front: bool, double_sided: bool) -> PbrInput {
    return PbrInput(StandardMaterial(vec4f(1.0), 0.0, 0.5, vec4f(0.0, 0.0, 0.0, 1.0), 0u), vec3f(0.0, 0.0, 1.0));
}
"#;
    const PBR_FUNCTIONS_STUB: &str = r#"
import super::pbr_types::PbrInput;
fn apply_pbr_lighting(input: PbrInput) -> vec4f {
    return input.material.base_color + input.material.emissive;
}
fn main_pass_post_lighting_processing(input: PbrInput, color: vec4f) -> vec4f {
    return color;
}
"#;

    fn define_shader(stage: &Stage, path: &str, id: &str) {
        stage
            .define_prim(path)
            .unwrap()
            .set_type_name("Shader")
            .unwrap();
        stage
            .create_attribute(format!("{path}.info:id"), "token")
            .unwrap()
            .set(Value::Token(id.into()))
            .unwrap();
    }

    fn constant_surface() -> (Stage, Path) {
        let stage = Stage::builder().in_memory("materialx.usda").unwrap();
        stage
            .define_prim("/Mat")
            .unwrap()
            .set_type_name("Material")
            .unwrap();
        define_shader(&stage, "/Mat/Surface", "ND_standard_surface_surfaceshader");
        stage
            .create_attribute("/Mat.outputs:mtlx:surface", "token")
            .unwrap()
            .set_connections([Path::new("/Mat/Surface.outputs:out").unwrap()])
            .unwrap();
        stage
            .create_attribute("/Mat/Surface.inputs:base_color", "color3f")
            .unwrap()
            .set(Value::Vec3f([0.2, 0.4, 0.6].into()))
            .unwrap();
        (stage, Path::new("/Mat").unwrap())
    }

    fn fixture(name: &str) -> (Stage, Path) {
        let path = format!(
            "{}/../../assets/tests/materialx/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        (Stage::open(&path).unwrap(), Path::new("/Mat").unwrap())
    }

    fn validate_with_bevy_wesl(compiled: &CompiledMaterialX, registry: &MaterialXRegistry) {
        let mut resolver = wesl::VirtualResolver::new();
        for (module, _, source) in registry.translator_modules() {
            resolver.add_module(
                format!("materialx::{module}").parse().unwrap(),
                Cow::Borrowed(*source),
            );
        }
        for (module, source) in [
            ("constants", "const MATERIAL_BIND_GROUP: u32 = 2u;"),
            ("bevy_pbr::render::forward_io", FORWARD_IO_STUB),
            ("bevy_pbr::render::pbr_types", PBR_TYPES_STUB),
            ("bevy_pbr::render::pbr_fragment", PBR_FRAGMENT_STUB),
            ("bevy_pbr::render::pbr_functions", PBR_FUNCTIONS_STUB),
        ] {
            resolver.add_module(module.parse().unwrap(), Cow::Borrowed(source));
        }
        resolver.add_module(
            "usd_bevy::graph".parse().unwrap(),
            Cow::Owned(compiled.wesl.clone()),
        );
        let mut compiler = wesl::Wesl::new("").set_custom_resolver(BevyTestResolver(resolver));
        compiler.set_feature("VERTEX_UVS_A", true);
        compiler
            .compile(&"usd_bevy::graph".parse().unwrap())
            .unwrap_or_else(|error| panic!("Bevy WESL compilation failed: {error}"));
    }

    #[test]
    fn constant_standard_surface_compiles_to_runtime_uniforms() {
        let (stage, material) = constant_surface();
        let compiled =
            compile_materialx(&stage, &material, None, &MaterialXRegistry::default()).unwrap();
        assert!(compiled.wesl.contains("materialx_graph"));
        assert!(compiled.wesl.contains("pbr_input.material.base_color"));
        assert!(compiled.uniforms.contains(&[0.2, 0.4, 0.6, 0.0]));
        assert!(compiled.required_modules.is_empty());
        compiled
            .wesl
            .parse::<wesl::syntax::TranslationUnit>()
            .expect("generated root is valid WESL syntax");
    }

    #[test]
    fn arithmetic_is_lowered_in_descriptor_input_order() {
        let (stage, material) = constant_surface();
        define_shader(&stage, "/Mat/Add", "ND_add_color3");
        stage
            .create_attribute("/Mat/Add.inputs:in1", "color3f")
            .unwrap()
            .set(Value::Vec3f([0.1, 0.2, 0.3].into()))
            .unwrap();
        stage
            .create_attribute("/Mat/Add.inputs:in2", "color3f")
            .unwrap()
            .set(Value::Vec3f([0.3, 0.2, 0.1].into()))
            .unwrap();
        stage
            .create_attribute("/Mat/Surface.inputs:base_color", "color3f")
            .unwrap()
            .set_connections([Path::new("/Mat/Add.outputs:out").unwrap()])
            .unwrap();
        let compiled =
            compile_materialx(&stage, &material, None, &MaterialXRegistry::default()).unwrap();
        assert!(compiled.wesl.contains("mx_add_color3("));
        assert_eq!(compiled.required_modules, ["generated::inline"]);
    }

    #[test]
    fn mtlx_terminal_wins_over_universal() {
        let (stage, material) = constant_surface();
        define_shader(&stage, "/Mat/Bad", "NotMaterialX");
        stage
            .create_attribute("/Mat.outputs:surface", "token")
            .unwrap()
            .set_connections([Path::new("/Mat/Bad.outputs:out").unwrap()])
            .unwrap();
        assert!(compile_materialx(&stage, &material, None, &MaterialXRegistry::default()).is_ok());
    }

    #[test]
    fn universal_terminal_is_a_valid_fallback() {
        let (stage, material) = constant_surface();
        stage
            .prim(material.clone())
            .attribute("outputs:mtlx:surface")
            .clear_connections()
            .unwrap();
        stage
            .create_attribute("/Mat.outputs:surface", "token")
            .unwrap()
            .set_connections([Path::new("/Mat/Surface.outputs:out").unwrap()])
            .unwrap();
        assert!(compile_materialx(&stage, &material, None, &MaterialXRegistry::default()).is_ok());
    }

    #[test]
    fn cycles_are_diagnostics() {
        let (stage, material) = constant_surface();
        define_shader(&stage, "/Mat/A", "ND_add_color3");
        define_shader(&stage, "/Mat/B", "ND_add_color3");
        stage
            .create_attribute("/Mat/A.inputs:in1", "color3f")
            .unwrap()
            .set_connections([Path::new("/Mat/B.outputs:out").unwrap()])
            .unwrap();
        stage
            .create_attribute("/Mat/B.inputs:in1", "color3f")
            .unwrap()
            .set_connections([Path::new("/Mat/A.outputs:out").unwrap()])
            .unwrap();
        stage
            .create_attribute("/Mat/Surface.inputs:base_color", "color3f")
            .unwrap()
            .set_connections([Path::new("/Mat/A.outputs:out").unwrap()])
            .unwrap();
        let failure =
            compile_materialx(&stage, &material, None, &MaterialXRegistry::default()).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == DiagnosticCode::ConnectionCycle)
        );
    }

    #[test]
    fn text_fixtures_cover_the_first_vertical_slice() {
        let registry = MaterialXRegistry::default();
        for name in [
            "constant_standard_surface.usda",
            "arithmetic.usda",
            "image_uv0.usda",
            "universal_fallback.usda",
            "shared_upstream.usda",
        ] {
            let (stage, material) = fixture(name);
            let compiled = compile_materialx(&stage, &material, None, &registry)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            compiled
                .wesl
                .parse::<wesl::syntax::TranslationUnit>()
                .unwrap_or_else(|error| panic!("{name}: invalid generated WESL: {error}"));
            validate_with_bevy_wesl(&compiled, &registry);
        }

        let (stage, material) = fixture("shared_upstream.usda");
        let compiled = compile_materialx(&stage, &material, None, &registry).unwrap();
        assert_eq!(compiled.wesl.matches("mx_add_color3(").count(), 1);
        let (stage, material) = fixture("image_uv0.usda");
        let compiled = compile_materialx(&stage, &material, None, &registry).unwrap();
        assert_eq!(compiled.textures.len(), 1);
        assert!(compiled.wesl.contains("mx_image_color3("));
    }

    #[test]
    fn invalid_text_fixtures_report_specific_diagnostics() {
        let registry = MaterialXRegistry::default();
        for (name, code) in [
            ("cycle.usda", DiagnosticCode::ConnectionCycle),
            (
                "ambiguous_connection.usda",
                DiagnosticCode::AmbiguousConnection,
            ),
            ("unknown_node.usda", DiagnosticCode::UnknownNodeDef),
            ("type_mismatch.usda", DiagnosticCode::TypeMismatch),
            ("missing_asset.usda", DiagnosticCode::MissingAsset),
        ] {
            let (stage, material) = fixture(name);
            let failure = compile_materialx(&stage, &material, None, &registry).unwrap_err();
            assert!(
                failure
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.code == code),
                "{name}: expected {code:?}, got {:?}",
                failure.diagnostics
            );
        }
    }
}
