//! Structured MaterialX compiler and runtime diagnostics.

use std::collections::VecDeque;
use std::fmt;

use bevy::prelude::{Component, Resource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticCode {
    InvalidDocument,
    MissingMaterial,
    AmbiguousMaterial,
    MissingTerminal,
    AmbiguousConnection,
    InvalidConnection,
    ConnectionCycle,
    UnknownNodeDef,
    UnsupportedImplementation,
    UnknownOutput,
    MissingValue,
    TypeMismatch,
    UnsupportedFeature,
    MissingAsset,
    ResourceLimit,
}

/// One location-aware MaterialX diagnostic suitable for logging or UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialXDiagnostic {
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub material: String,
    pub node: Option<String>,
    pub property: Option<String>,
    pub message: String,
}

impl MaterialXDiagnostic {
    pub fn error(
        code: DiagnosticCode,
        material: impl Into<String>,
        node: Option<String>,
        property: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            code,
            material: material.into(),
            node,
            property,
            message: message.into(),
        }
    }

    pub fn warning(
        code: DiagnosticCode,
        material: impl Into<String>,
        node: Option<String>,
        property: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            material: material.into(),
            node,
            property,
            message: message.into(),
        }
    }
}

impl fmt::Display for MaterialXDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {:?}: {}",
            self.material, self.code, self.message
        )?;
        if let Some(node) = &self.node {
            write!(formatter, " [node {node}]")?;
        }
        if let Some(property) = &self.property {
            write!(formatter, " [property {property}]")?;
        }
        Ok(())
    }
}

/// Diagnostics attached to an entity whose MaterialX graph failed.
#[derive(Component, Debug, Clone)]
pub struct MaterialXFailure(pub Vec<MaterialXDiagnostic>);

/// Bounded session history for editor/UI inspection.
#[derive(Resource, Debug)]
pub struct MaterialXDiagnostics {
    entries: VecDeque<MaterialXDiagnostic>,
    capacity: usize,
}

impl Default for MaterialXDiagnostics {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: 256,
        }
    }
}

impl MaterialXDiagnostics {
    pub fn push(&mut self, diagnostic: MaterialXDiagnostic) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(diagnostic);
    }

    pub fn iter(&self) -> impl Iterator<Item = &MaterialXDiagnostic> {
        self.entries.iter()
    }
}
