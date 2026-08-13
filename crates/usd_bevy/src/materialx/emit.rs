//! Deterministic root WESL emission.

use std::collections::BTreeSet;

use materialx_wesl::{CompiledMaterial, ValueType};

use super::compiler::SurfaceExpressions;

pub(super) fn root_module(
    imports: &BTreeSet<(String, String)>,
    graph_lines: &[String],
    surface: &SurfaceExpressions,
) -> String {
    let mut source = String::new();
    source.push_str(
        "import bevy_pbr::render::{\n\
         \x20   forward_io::{VertexOutput, FragmentOutput},\n\
         \x20   pbr_fragment::pbr_input_from_vertex_output,\n\
         \x20   pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},\n\
         \x20   pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_BLEND,\n\
         };\n",
    );
    for (module, symbol) in imports {
        source.push_str(&format!(
            "import materialx::{}::{symbol};\n",
            module.replace("::", "::")
        ));
    }
    source.push_str(
        "\nstruct MaterialXUniforms {\n\
         \x20   values: array<vec4f, 64>,\n\
         }\n\
         @group(constants::MATERIAL_BIND_GROUP) @binding(0)\n\
         var<uniform> materialx: MaterialXUniforms;\n",
    );
    let texture_count = graph_lines
        .iter()
        .flat_map(|line| {
            (0..4).filter(move |index| line.contains(&format!("materialx_texture_{index}")))
        })
        .max()
        .map_or(0, |index| index + 1);
    for index in 0..texture_count {
        let texture_binding = 1 + index * 2;
        let sampler_binding = texture_binding + 1;
        source.push_str(&format!(
            "@group(constants::MATERIAL_BIND_GROUP) @binding({texture_binding}) var materialx_texture_{index}: texture_2d<f32>;\n\
             @group(constants::MATERIAL_BIND_GROUP) @binding({sampler_binding}) var materialx_sampler_{index}: sampler;\n"
        ));
    }
    source.push_str(
        "\nstruct MaterialXPbrParams {\n\
         \x20   base_color: vec3f,\n\
         \x20   metalness: f32,\n\
         \x20   roughness: f32,\n\
         \x20   emission: vec3f,\n\
         \x20   opacity: f32,\n\
         \x20   normal: vec3f,\n\
         }\n\n\
         fn materialx_graph(uv: vec2f) -> MaterialXPbrParams {\n",
    );
    for line in graph_lines {
        source.push_str(line);
        source.push('\n');
    }
    let normal = surface
        .normal
        .as_ref()
        .map_or("vec3f(0.0)", |value| &value.code);
    source.push_str(&format!(
        "    return MaterialXPbrParams(\n\
         \x20       {},\n\
         \x20       {},\n\
         \x20       {},\n\
         \x20       {},\n\
         \x20       {},\n\
         \x20       {},\n\
         \x20   );\n\
         }}\n\n",
        surface.base_color.code,
        surface.metalness.code,
        surface.roughness.code,
        surface.emission.code,
        surface.opacity.code,
        normal,
    ));
    source.push_str(
        "@fragment\n\
         fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {\n\
         \x20   var materialx_uv = vec2f(0.0);\n\
         @if(VERTEX_UVS_A)\n\
         \x20   materialx_uv = in.uv;\n\
         \x20   let mx = materialx_graph(materialx_uv);\n\
         \x20   var pbr_input = pbr_input_from_vertex_output(in, is_front, false);\n\
         \x20   pbr_input.material.base_color = vec4f(mx.base_color, mx.opacity);\n\
         \x20   pbr_input.material.metallic = clamp(mx.metalness, 0.0, 1.0);\n\
         \x20   pbr_input.material.perceptual_roughness = clamp(mx.roughness, 0.0, 1.0);\n\
         \x20   pbr_input.material.emissive = vec4f(mx.emission, 1.0);\n",
    );
    if surface.alpha_blend {
        source.push_str(
            "    pbr_input.material.flags = pbr_input.material.flags | STANDARD_MATERIAL_FLAGS_ALPHA_MODE_BLEND;\n",
        );
    }
    if surface.normal.is_some() {
        source.push_str("    pbr_input.N = normalize(mx.normal);\n");
    }
    source.push_str(
        "    var out: FragmentOutput;\n\
         \x20   out.color = apply_pbr_lighting(pbr_input);\n\
         \x20   out.color = main_pass_post_lighting_processing(pbr_input, out.color);\n\
         \x20   return out;\n\
         }\n",
    );
    source
}

/// Wrap a renderer-neutral standalone MaterialX graph in Bevy's forward PBR
/// fragment interface and the fixed host bind-group layout.
pub(super) fn external_root_module(compiled: &CompiledMaterial, uses_normal: bool) -> String {
    let mut source = String::new();
    source.push_str(
        "import bevy_pbr::render::{\n\
         \x20   forward_io::{VertexOutput, FragmentOutput},\n\
         \x20   pbr_fragment::pbr_input_from_vertex_output,\n\
         \x20   pbr_functions::{apply_pbr_lighting, main_pass_post_lighting_processing},\n\
         \x20   pbr_types::STANDARD_MATERIAL_FLAGS_ALPHA_MODE_BLEND,\n\
         };\n",
    );
    source.push_str(&compiled.root_wesl);
    source.push_str(
        "\nstruct MaterialXUniforms {\n\
         \x20   values: array<vec4f, 64>,\n\
         }\n\
         @group(constants::MATERIAL_BIND_GROUP) @binding(0)\n\
         var<uniform> materialx: MaterialXUniforms;\n",
    );
    for index in 0..compiled.textures.len() {
        let texture_binding = 1 + index * 2;
        let sampler_binding = texture_binding + 1;
        source.push_str(&format!(
            "@group(constants::MATERIAL_BIND_GROUP) @binding({texture_binding}) var materialx_texture_{index}: texture_2d<f32>;\n\
             @group(constants::MATERIAL_BIND_GROUP) @binding({sampler_binding}) var materialx_sampler_{index}: sampler;\n"
        ));
    }
    source.push_str(
        "\n@fragment\n\
         fn fragment(in: VertexOutput, @builtin(front_facing) is_front: bool) -> FragmentOutput {\n\
         \x20   var pbr_input = pbr_input_from_vertex_output(in, is_front, false);\n\
         \x20   var materialx_uv = vec2f(0.0);\n\
         @if(VERTEX_UVS_A)\n\
         \x20   materialx_uv = in.uv;\n",
    );
    if compiled.context.tangent || compiled.context.bitangent {
        source.push_str(
            "    var materialx_tangent = vec3f(1.0, 0.0, 0.0);\n\
             	@if(VERTEX_TANGENTS)\n\
             	materialx_tangent = normalize(in.world_tangent.xyz);\n\
             	var materialx_bitangent = normalize(cross(pbr_input.world_normal, materialx_tangent));\n\
             	@if(VERTEX_TANGENTS)\n\
             	materialx_bitangent = normalize(in.world_tangent.w * cross(pbr_input.world_normal, materialx_tangent));\n",
        );
    }

    let mut arguments = compiled
        .uniforms
        .iter()
        .enumerate()
        .map(|(index, uniform)| uniform_argument(index, &uniform.value_type))
        .collect::<Vec<_>>();
    for index in 0..compiled.textures.len() {
        arguments.push(format!("materialx_texture_{index}"));
        arguments.push(format!("materialx_sampler_{index}"));
    }
    if compiled.context.uv0 {
        arguments.push("materialx_uv".into());
    }
    if compiled.context.geometric_normal {
        arguments.push("pbr_input.world_normal".into());
    }
    if compiled.context.tangent {
        arguments.push("materialx_tangent".into());
    }
    if compiled.context.bitangent {
        arguments.push("materialx_bitangent".into());
    }
    source.push_str(&format!(
        "    let mx = materialx_graph({});\n\
         \x20   pbr_input.material.base_color = vec4f(mx.base_color, mx.opacity);\n\
         \x20   pbr_input.material.metallic = clamp(mx.metalness, 0.0, 1.0);\n\
         \x20   pbr_input.material.perceptual_roughness = clamp(mx.specular_roughness, 0.0, 1.0);\n\
         \x20   pbr_input.material.emissive = vec4f(mx.emission, 1.0);\n",
        arguments.join(", ")
    ));
    if matches!(compiled.alpha_mode, materialx_wesl::AlphaMode::Blend) {
        source.push_str(
            "    pbr_input.material.flags = pbr_input.material.flags | STANDARD_MATERIAL_FLAGS_ALPHA_MODE_BLEND;\n",
        );
    }
    if uses_normal {
        source.push_str("    pbr_input.N = normalize(mx.normal);\n");
    }
    source.push_str(
        "    var out: FragmentOutput;\n\
         \x20   out.color = apply_pbr_lighting(pbr_input);\n\
         \x20   out.color = main_pass_post_lighting_processing(pbr_input, out.color);\n\
         \x20   return out;\n\
         }\n",
    );
    source
}

fn uniform_argument(index: usize, value_type: &ValueType) -> String {
    match value_type {
        ValueType::Boolean => format!("materialx.values[{index}].x != 0.0"),
        ValueType::Integer => format!("i32(materialx.values[{index}].x)"),
        ValueType::Float => format!("materialx.values[{index}].x"),
        ValueType::Vector2 => format!("materialx.values[{index}].xy"),
        ValueType::Vector3 | ValueType::Color3 => {
            format!("materialx.values[{index}].xyz")
        }
        ValueType::Vector4 | ValueType::Color4 => format!("materialx.values[{index}]"),
        _ => unreachable!("external adapter rejects non-vec4 MaterialX uniforms"),
    }
}
