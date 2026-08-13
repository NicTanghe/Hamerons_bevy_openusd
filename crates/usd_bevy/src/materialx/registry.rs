//! Machine-readable MaterialX NodeDef registry bundled with the WESL modules.

use std::collections::HashMap;

use bevy::prelude::Resource;
use serde::Deserialize;

const COVERAGE_RON: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../vendor/materialx-wesl/WESL/metadata/coverage.ron"
));

include!(concat!(env!("OUT_DIR"), "/materialx_modules.rs"));

/// MaterialX source provenance recorded by the translator.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Upstream {
    pub project: String,
    pub version: String,
    pub commit: String,
    pub source_url: String,
}

/// Coverage totals from the translator snapshot.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Summary {
    pub nodedefs: usize,
    pub inline_generated: usize,
    pub file_backed: usize,
    pub generator_context: usize,
    pub nodegraphs: usize,
    pub host_only: usize,
    pub unsupported_upstream: usize,
}

/// One ordered MaterialX input or output port.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub materialx_type: String,
    pub wesl_type: Option<String>,
    pub default_value: Option<String>,
    pub uniform: bool,
}

/// Descriptor for one MaterialX NodeDef.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Coverage {
    pub nodedef: String,
    pub node: String,
    pub node_group: String,
    pub source_kind: String,
    pub source: Option<String>,
    pub symbol: Option<String>,
    pub module: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub inputs: Vec<Port>,
    pub outputs: Vec<Port>,
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    schema_version: u32,
    generator: String,
    upstream: Upstream,
    summary: Summary,
    nodes: Vec<Coverage>,
}

/// Parsed, indexed descriptor registry used by the graph compiler.
#[derive(Resource, Debug, Clone)]
pub struct MaterialXRegistry {
    schema_version: u32,
    generator: String,
    upstream: Upstream,
    summary: Summary,
    nodes: Vec<Coverage>,
    by_id: HashMap<String, usize>,
}

impl Default for MaterialXRegistry {
    fn default() -> Self {
        Self::bundled().expect("build.rs already validated bundled MaterialX registry")
    }
}

impl MaterialXRegistry {
    /// Parse the descriptor embedded from the pinned translator submodule.
    pub fn bundled() -> Result<Self, ron::error::SpannedError> {
        let mut manifest: Manifest = ron::from_str(COVERAGE_RON)?;

        // Standard Surface lives in MaterialX pbrlib, while the current
        // translator snapshot covers stdlib. It is a host node in this first
        // Bevy-PBR slice, so keep its deliberately small interface here.
        manifest.nodes.push(host_standard_surface());

        let by_id = manifest
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.nodedef.clone(), index))
            .collect();
        Ok(Self {
            schema_version: manifest.schema_version,
            generator: manifest.generator,
            upstream: manifest.upstream,
            summary: manifest.summary,
            nodes: manifest.nodes,
            by_id,
        })
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn generator(&self) -> &str {
        &self.generator
    }

    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    pub fn node(&self, nodedef: &str) -> Option<&Coverage> {
        self.by_id.get(nodedef).map(|index| &self.nodes[*index])
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Every bundled WESL module as `(descriptor module, Bevy asset path,
    /// source)`. Registration does not copy or rewrite translator source.
    pub fn translator_modules(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        TRANSLATOR_MODULES
    }
}

fn port(name: &str, materialx_type: &str, wesl_type: &str, default_value: &str) -> Port {
    Port {
        name: name.into(),
        materialx_type: materialx_type.into(),
        wesl_type: Some(wesl_type.into()),
        default_value: Some(default_value.into()),
        uniform: false,
    }
}

fn host_standard_surface() -> Coverage {
    Coverage {
        nodedef: "ND_standard_surface_surfaceshader".into(),
        node: "standard_surface".into(),
        node_group: "pbr".into(),
        source_kind: "host".into(),
        source: None,
        symbol: None,
        module: None,
        status: "host_pbr".into(),
        reason: Some("Lowered approximately to Bevy StandardMaterial/PBR inputs".into()),
        inputs: vec![
            port("base", "float", "f32", "1.0"),
            port("base_color", "color3", "vec3f", "0.8, 0.8, 0.8"),
            port("metalness", "float", "f32", "0.0"),
            port("specular_roughness", "float", "f32", "0.2"),
            port("emission", "float", "f32", "0.0"),
            port("emission_color", "color3", "vec3f", "1.0, 1.0, 1.0"),
            port("opacity", "color3", "vec3f", "1.0, 1.0, 1.0"),
            port("normal", "vector3", "vec3f", "0.0, 0.0, 1.0"),
        ],
        outputs: vec![Port {
            name: "out".into(),
            materialx_type: "surfaceshader".into(),
            wesl_type: None,
            default_value: None,
            uniform: false,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_registry_has_expected_provenance_and_nodes() {
        let registry = MaterialXRegistry::bundled().unwrap();
        assert_eq!(registry.schema_version(), 1);
        assert_eq!(registry.upstream().version, "1.39.5");
        assert_eq!(registry.summary().nodedefs, 693);
        assert!(registry.node("ND_add_color3").is_some());
        assert_eq!(
            registry
                .node("ND_standard_surface_surfaceshader")
                .unwrap()
                .status,
            "host_pbr"
        );
    }

    #[test]
    fn every_callable_descriptor_resolves_to_a_bundled_module_and_symbol() {
        let registry = MaterialXRegistry::bundled().unwrap();
        let modules: HashMap<_, _> = registry
            .translator_modules()
            .iter()
            .map(|(module, _, source)| (*module, *source))
            .collect();
        for node in &registry.nodes {
            if !matches!(node.status.as_str(), "translated" | "composed") {
                continue;
            }
            let module = node.module.as_deref().unwrap();
            let symbol = node.symbol.as_deref().unwrap();
            assert!(
                modules[module].contains(&format!("fn {symbol}(")),
                "{} -> {module}::{symbol}",
                node.nodedef
            );
        }
    }
}
