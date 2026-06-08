//! Shared attribute/relationship plumbing for the `read` modules.
//!
//! These helpers read composed values straight off openusd's public
//! `Prim` / `Attribute` / `Relationship` handles — the crate reads the
//! authored scene through openusd only, with no schema layer in between.
//! Each helper takes the owning prim path plus the property name and
//! returns the decoded value (`None` when unauthored or a type mismatch),
//! mirroring the old `usd_schema` reader plumbing it replaces.

use openusd::sdf::{Path, Value};
use openusd::usd::Stage;

/// Raw composed `default` value of attribute `name` on `prim`.
fn attr_default(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<Value>> {
    stage.prim_at(prim.clone()).attribute(name).get::<Value>()
}

pub fn read_f32(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<f32>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Float(v)) => Some(v),
        Some(Value::Double(v)) => Some(v as f32),
        Some(Value::Int(v)) => Some(v as f32),
        _ => None,
    })
}

pub fn read_double(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<f64>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Double(v)) => Some(v),
        Some(Value::Float(v)) => Some(v as f64),
        Some(Value::Int(v)) => Some(v as f64),
        _ => None,
    })
}

pub fn read_int(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<i32>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Int(v)) => Some(v),
        Some(Value::Int64(v)) => Some(v as i32),
        _ => None,
    })
}

pub fn read_bool(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<bool>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Bool(v)) => Some(v),
        _ => None,
    })
}

/// A `token` or `string` scalar.
pub fn read_token_or_string(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<String>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Token(s)) | Some(Value::String(s)) => Some(s),
        _ => None,
    })
}

/// An `asset`, `string`, or `token` scalar (asset path or plain text).
pub fn read_asset_path(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<String>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::AssetPath(s)) | Some(Value::String(s)) | Some(Value::Token(s)) => Some(s),
        _ => None,
    })
}

pub fn read_vec3f(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<[f32; 3]>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Vec3f(v)) => Some(v.into()),
        Some(Value::Vec3d(v)) => Some([v.x as f32, v.y as f32, v.z as f32]),
        _ => None,
    })
}

pub fn read_vec2f(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Option<[f32; 2]>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Vec2f(v)) => Some(v.into()),
        Some(Value::Vec2d(v)) => Some([v.x as f32, v.y as f32]),
        _ => None,
    })
}

pub fn read_token_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<String>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::TokenVec(v)) | Some(Value::StringVec(v)) => v,
        _ => Vec::new(),
    })
}

pub fn read_int_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<i32>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::IntVec(v)) => v,
        Some(Value::Int64Vec(v)) => v.into_iter().map(|i| i as i32).collect(),
        _ => Vec::new(),
    })
}

pub fn read_float_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<f32>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::FloatVec(v)) => v,
        Some(Value::DoubleVec(v)) => v.into_iter().map(|d| d as f32).collect(),
        _ => Vec::new(),
    })
}

pub fn read_double_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<f64>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::DoubleVec(v)) => v,
        Some(Value::FloatVec(v)) => v.into_iter().map(|f| f as f64).collect(),
        _ => Vec::new(),
    })
}

pub fn read_vec3f_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<[f32; 3]>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Vec3fVec(v)) => v.into_iter().map(Into::into).collect(),
        _ => Vec::new(),
    })
}

pub fn read_vec2f_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<[f32; 2]>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Vec2fVec(v)) => v.into_iter().map(Into::into).collect(),
        _ => Vec::new(),
    })
}

pub fn read_vec2d_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<[f64; 2]>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::Vec2dVec(v)) => v.into_iter().map(Into::into).collect(),
        _ => Vec::new(),
    })
}

pub fn read_quatf_vec(stage: &Stage, prim: &Path, name: &str) -> anyhow::Result<Vec<[f32; 4]>> {
    Ok(match attr_default(stage, prim, name)? {
        Some(Value::QuatfVec(v)) => v.into_iter().map(Into::into).collect(),
        _ => Vec::new(),
    })
}

/// Composed relationship target paths (as strings), in authored order.
pub fn read_rel_targets(stage: &Stage, prim: &Path, rel_name: &str) -> anyhow::Result<Vec<String>> {
    let targets = stage.prim_at(prim.clone()).relationship(rel_name).targets()?;
    Ok(targets.into_iter().map(|p| p.as_str().to_string()).collect())
}

/// The first composed relationship target (strongest), if any.
pub fn read_rel_first_target(stage: &Stage, prim: &Path, rel_name: &str) -> anyhow::Result<Option<String>> {
    Ok(read_rel_targets(stage, prim, rel_name)?.into_iter().next())
}
