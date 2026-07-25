//! Curves route (SCHEMA_INTEGRATION Phase C): `UsdGeomBasisCurves` → a Bevy
//! `LineList` mesh. Read through the geom `BasisCurves` / `Curves` schema.
//!
//! Bevy has no native curve primitive, so each curve is drawn as line segments.
//! Linear curves connect their vertices directly; **cubic** curves are
//! tessellated (PLAN Phase 6e) — each segment is evaluated through its basis
//! matrix (bezier / b-spline / catmull-rom) at [`CUBIC_STEPS`] samples, so the
//! rendered polyline follows the smooth curve rather than its control hull.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;

use openusd::schemas::geom::{BasisCurves, Curves, PointBased};
use openusd::sdf::Value;

use super::{PrimRoute, RouteCtx};

/// Maps `UsdGeomBasisCurves` to a line-strip mesh.
pub struct CurvesRoute;

fn read_points(curves: &BasisCurves) -> Option<Vec<[f32; 3]>> {
    match curves.points_attr().get::<Value>() {
        Ok(Some(Value::Vec3fVec(v))) => Some(v.iter().map(|p| [p.x, p.y, p.z]).collect()),
        Ok(Some(Value::Vec3dVec(v))) => {
            Some(v.iter().map(|p| [p.x as f32, p.y as f32, p.z as f32]).collect())
        }
        _ => None,
    }
}

fn read_counts(curves: &BasisCurves) -> Vec<i32> {
    match curves.curve_vertex_counts_attr().get::<Value>() {
        Ok(Some(Value::IntVec(v))) => v,
        _ => Vec::new(),
    }
}

fn read_token(attr: openusd::usd::Attribute, default: &str) -> String {
    match attr.get::<Value>() {
        Ok(Some(Value::Token(t))) => t.as_str().to_string(),
        Ok(Some(Value::String(s))) => s,
        _ => default.to_string(),
    }
}

/// Samples per cubic segment. 8 keeps meshes light while removing the visible
/// faceting of a raw control hull.
pub const CUBIC_STEPS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Basis {
    Bezier,
    Bspline,
    CatmullRom,
}

impl Basis {
    fn parse(s: &str) -> Self {
        match s {
            "bezier" => Basis::Bezier,
            "catmullRom" => Basis::CatmullRom,
            _ => Basis::Bspline,
        }
    }

    /// Control-point stride between consecutive cubic segments.
    fn vstep(self) -> usize {
        match self {
            Basis::Bezier => 3,
            Basis::Bspline | Basis::CatmullRom => 1,
        }
    }

    /// The four blending weights for parameter `t` in `[0, 1]`.
    fn weights(self, t: f32) -> [f32; 4] {
        let (t2, t3) = (t * t, t * t * t);
        match self {
            Basis::Bezier => {
                let u = 1.0 - t;
                [u * u * u, 3.0 * t * u * u, 3.0 * t2 * u, t3]
            }
            Basis::Bspline => [
                (-t3 + 3.0 * t2 - 3.0 * t + 1.0) / 6.0,
                (3.0 * t3 - 6.0 * t2 + 4.0) / 6.0,
                (-3.0 * t3 + 3.0 * t2 + 3.0 * t + 1.0) / 6.0,
                t3 / 6.0,
            ],
            Basis::CatmullRom => [
                0.5 * (-t3 + 2.0 * t2 - t),
                0.5 * (3.0 * t3 - 5.0 * t2 + 2.0),
                0.5 * (-3.0 * t3 + 4.0 * t2 + t),
                0.5 * (t3 - t2),
            ],
        }
    }
}

fn eval(cvs: [[f32; 3]; 4], w: [f32; 4]) -> [f32; 3] {
    let mut p = [0.0f32; 3];
    for k in 0..4 {
        for c in 0..3 {
            p[c] += w[k] * cvs[k][c];
        }
    }
    p
}

/// Append a tessellated cubic curve (points `cv`, `periodic` wrap) to the
/// output position + line-index buffers.
fn tessellate_cubic(cv: &[[f32; 3]], basis: Basis, periodic: bool, out: &mut Vec<[f32; 3]>, idx: &mut Vec<u32>) {
    let n = cv.len();
    if n < 4 {
        // Not enough CVs for a cubic segment; fall back to a polyline.
        emit_polyline(cv, out, idx);
        return;
    }
    let vstep = basis.vstep();
    // Nonperiodic: last full window starts at n-4. Periodic: windows wrap.
    let seg_count = if periodic {
        n / vstep
    } else {
        (n - 4) / vstep + 1
    };
    for s in 0..seg_count {
        let base = s * vstep;
        let cvs = [
            cv[base % n],
            cv[(base + 1) % n],
            cv[(base + 2) % n],
            cv[(base + 3) % n],
        ];
        // First sample of a segment coincides with the previous segment's last;
        // start at step 1 for continued segments to avoid duplicate joints.
        let start = if s == 0 { 0 } else { 1 };
        for step in start..=CUBIC_STEPS {
            let t = step as f32 / CUBIC_STEPS as f32;
            let cur = out.len() as u32;
            out.push(eval(cvs, basis.weights(t)));
            if cur > 0 && !(s == 0 && step == 0) {
                idx.push(cur - 1);
                idx.push(cur);
            }
        }
    }
}

/// Append straight segments connecting consecutive vertices.
fn emit_polyline(cv: &[[f32; 3]], out: &mut Vec<[f32; 3]>, idx: &mut Vec<u32>) {
    let base = out.len() as u32;
    out.extend_from_slice(cv);
    for i in 0..cv.len().saturating_sub(1) {
        idx.push(base + i as u32);
        idx.push(base + i as u32 + 1);
    }
}

/// Positions + line indices for every curve. Linear curves connect vertices
/// directly; cubic curves are tessellated through their basis.
fn line_geometry(ctx: &RouteCtx) -> Option<(Vec<[f32; 3]>, Vec<u32>)> {
    let curves = BasisCurves::get(ctx.stage, ctx.path.clone()).ok()??;
    let points = read_points(&curves)?;
    if points.is_empty() {
        return None;
    }
    let is_cubic = read_token(curves.type_attr(), "cubic") == "cubic";
    let basis = Basis::parse(&read_token(curves.basis_attr(), "bspline"));
    let periodic = read_token(curves.wrap_attr(), "nonperiodic") == "periodic";

    let mut counts = read_counts(&curves);
    // Absent counts ⇒ one curve spanning all points.
    if counts.is_empty() {
        counts = vec![points.len() as i32];
    }

    let mut out: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut cursor = 0usize;
    for c in counts {
        let n = c.max(0) as usize;
        let end = (cursor + n).min(points.len());
        let cv = &points[cursor..end];
        if is_cubic {
            tessellate_cubic(cv, basis, periodic, &mut out, &mut indices);
        } else {
            emit_polyline(cv, &mut out, &mut indices);
        }
        cursor = end;
    }
    Some((out, indices))
}

impl PrimRoute for CurvesRoute {
    fn matches(&self, ctx: &RouteCtx) -> bool {
        ctx.type_name.as_deref() == Some("BasisCurves")
    }

    fn project(&self, ctx: &RouteCtx, world: &mut World, entity: Entity) {
        if world.get_resource::<Assets<Mesh>>().is_none()
            || world.get_resource::<Assets<StandardMaterial>>().is_none()
        {
            return;
        }
        let Some((points, indices)) = line_geometry(ctx) else {
            return;
        };
        let mut mesh = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default());
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, points);
        mesh.insert_indices(bevy::mesh::Indices::U32(indices));
        let mesh_handle = super::cache::intern_mesh(world, mesh);
        let material = world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        if let Ok(mut e) = world.get_entity_mut(entity) {
            e.insert((Mesh3d(mesh_handle), MeshMaterial3d(material)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStage, PrimEntities, project_stage};
    use crate::route::SchemaRegistry;
    use openusd::usd::Stage;

    #[test]
    fn curves_project_line_mesh() {
        let stage = Stage::builder().in_memory("crv.usda").unwrap();
        stage
            .define_prim("/Curve")
            .unwrap()
            .set_type_name("BasisCurves")
            .unwrap();
        stage
            .create_attribute("/Curve.points", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 0.0, 0.0].into(),
                [2.0, 0.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/Curve.curveVertexCounts", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![3]))
            .unwrap();

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let e = map.entity("/Curve").unwrap();
        let handle = world.get::<Mesh3d>(e).expect("curve mesh").0.clone();
        let mesh = world.resource::<Assets<Mesh>>().get(&handle).unwrap();
        assert_eq!(mesh.primitive_topology(), PrimitiveTopology::LineList);
        // 3 points (< 4 CVs) → polyline fallback → 2 segments → 4 line indices.
        assert_eq!(mesh.indices().map(|i| i.len()), Some(4));
    }

    #[test]
    fn cubic_curve_tessellates() {
        let stage = Stage::builder().in_memory("crv.usda").unwrap();
        stage
            .define_prim("/Curve")
            .unwrap()
            .set_type_name("BasisCurves")
            .unwrap();
        stage
            .create_attribute("/Curve.type", "token")
            .unwrap()
            .set(Value::Token("cubic".into()))
            .unwrap();
        stage
            .create_attribute("/Curve.basis", "token")
            .unwrap()
            .set(Value::Token("bezier".into()))
            .unwrap();
        // 4 CVs = one bezier segment.
        stage
            .create_attribute("/Curve.points", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 2.0, 0.0].into(),
                [2.0, 2.0, 0.0].into(),
                [3.0, 0.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/Curve.curveVertexCounts", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![4]))
            .unwrap();

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let e = map.entity("/Curve").unwrap();
        let handle = world.get::<Mesh3d>(e).expect("curve mesh").0.clone();
        let mesh = world.resource::<Assets<Mesh>>().get(&handle).unwrap();
        // One cubic segment → CUBIC_STEPS+1 samples, CUBIC_STEPS line segments.
        assert_eq!(mesh.count_vertices(), CUBIC_STEPS + 1);
        assert_eq!(mesh.indices().map(|i| i.len()), Some(CUBIC_STEPS * 2));
        // The tessellated midpoint bows off the control-hull chord: at t=0.5 a
        // symmetric bezier peaks above the straight line between endpoints.
        if let Some(bevy::mesh::VertexAttributeValues::Float32x3(pos)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        {
            let mid = pos[CUBIC_STEPS / 2];
            assert!(mid[1] > 0.5, "curve bows upward, got y={}", mid[1]);
        } else {
            panic!("no positions");
        }
    }

    /// A cubic curve whose `curveVertexCounts` overshoots the point buffer, plus
    /// a negative count, must not panic on out-of-range indexing — the geometry
    /// is clamped to the available points.
    #[test]
    fn malformed_counts_do_not_panic() {
        let stage = Stage::builder().in_memory("crv.usda").unwrap();
        stage.define_prim("/Curve").unwrap().set_type_name("BasisCurves").unwrap();
        stage
            .create_attribute("/Curve.points", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 0.0, 0.0].into(),
                [2.0, 0.0, 0.0].into(),
            ]))
            .unwrap();
        // 10 exceeds the 3 authored points; -1 is nonsense — both must be safe.
        stage
            .create_attribute("/Curve.curveVertexCounts", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![10, -1]))
            .unwrap();

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        // The assertion is simply that this does not panic.
        project_stage(&mut world, &live, &mut map);
        assert!(world.get::<Mesh3d>(map.entity("/Curve").unwrap()).is_some());
    }

    /// A periodic cubic curve exercises the wrap-around index path (`% n`).
    #[test]
    fn periodic_cubic_wraps_without_panic() {
        let stage = Stage::builder().in_memory("crv.usda").unwrap();
        stage.define_prim("/Curve").unwrap().set_type_name("BasisCurves").unwrap();
        stage
            .create_attribute("/Curve.type", "token")
            .unwrap()
            .set(Value::Token("cubic".into()))
            .unwrap();
        stage
            .create_attribute("/Curve.wrap", "token")
            .unwrap()
            .set(Value::Token("periodic".into()))
            .unwrap();
        stage
            .create_attribute("/Curve.points", "point3f[]")
            .unwrap()
            .set(Value::Vec3fVec(vec![
                [0.0, 0.0, 0.0].into(),
                [1.0, 1.0, 0.0].into(),
                [2.0, 0.0, 0.0].into(),
                [1.0, -1.0, 0.0].into(),
            ]))
            .unwrap();
        stage
            .create_attribute("/Curve.curveVertexCounts", "int[]")
            .unwrap()
            .set(Value::IntVec(vec![4]))
            .unwrap();

        let mut world = World::new();
        world.insert_resource(Assets::<Mesh>::default());
        world.insert_resource(Assets::<StandardMaterial>::default());
        world.insert_resource(SchemaRegistry::builtin());
        let live = LiveStage::new(stage);
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        let handle = world.get::<Mesh3d>(map.entity("/Curve").unwrap()).unwrap().0.clone();
        let mesh = world.resource::<Assets<Mesh>>().get(&handle).unwrap();
        // 4 CVs, vstep 1 (bspline default), periodic → 4 segments, each closing
        // back through the ring; a non-empty index buffer proves the wrap ran.
        assert!(mesh.indices().map(|i| i.len()).unwrap_or(0) > 0);
    }
}
