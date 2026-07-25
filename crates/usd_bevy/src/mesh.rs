//! UsdGeom → `bevy::render::mesh::Mesh`.
//!
//! Two kinds of input:
//! - Full meshes (`UsdGeom.Mesh`) — converts points / face indices / normals
//!   / uvs, fan-triangulates faces > 3 verts, expands `faceVarying` primvars.
//! - Primitive shapes (`Cube`, `Sphere`, `Cylinder`, `Capsule`) — delegate
//!   to Bevy's built-in `Meshable` primitives with the right dimensions.
//!
//! Orientation (`"leftHanded"` flips winding) and missing-normal fallback
//! (`compute_smooth_normals`) are handled here.

use crate::read::geom::{Axis, Interpolation, MeshPrimvar, Orientation, ReadCylinder, ReadMesh};
use bevy::asset::RenderAssetUsages;
use bevy::math::Vec3;
use bevy::mesh::{Indices, Mesh, Meshable, PrimitiveTopology, VertexAttributeValues};

/// Convert a `crate::read::geom::ReadMesh` into a Bevy mesh.
///
/// Steps:
/// 1. Triangulate each face by fan (works for triangles and convex quads;
///    non-convex n-gons need an ear-clip pass we punt to M2.1).
/// 2. Expand per-vertex attributes for `faceVarying` primvars (one vertex
///    per corner) or keep indexed when interpolation is `vertex`.
/// 3. Fall back to `compute_smooth_normals` when normals aren't authored.
/// 4. Flip index winding when `orientation == LeftHanded`.
pub fn mesh_from_usd(read: &ReadMesh) -> Mesh {
    mesh_from_usd_subset(read, None)
}

/// Same as [`mesh_from_usd`] but emits only the faces in `face_subset` when
/// provided (`None` = every face). Used to split a `UsdGeom.Mesh` into one
/// Bevy mesh per `GeomSubset` so each subset can carry its own material.
pub fn mesh_from_usd_subset(read: &ReadMesh, face_subset: Option<&[i32]>) -> Mesh {
    // Face-Varying or Uniform (per-face) primvars break the indexed
    // point-sharing optimisation — vertex-indexed output can't represent
    // a per-face or per-corner value when a vertex is shared between
    // faces with different authored values. Expand to per-corner layout
    // in those cases.
    let non_indexed = |interp: Interpolation| {
        matches!(interp, Interpolation::FaceVarying | Interpolation::Uniform)
    };
    let expand = read
        .normals
        .as_ref()
        .map(|p| non_indexed(p.interpolation))
        .unwrap_or(false)
        || read
            .uvs
            .as_ref()
            .map(|p| non_indexed(p.interpolation))
            .unwrap_or(false)
        || read
            .display_color
            .as_ref()
            .map(|p| non_indexed(p.interpolation))
            .unwrap_or(false)
        || read
            .display_opacity
            .as_ref()
            .map(|p| non_indexed(p.interpolation))
            .unwrap_or(false);

    let (positions, normals, uvs, colors, indices) = if expand {
        build_expanded(read, face_subset)
    } else {
        build_indexed(read, face_subset)
    };

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    // USD's `primvars:st` convention puts (0,0) at the texture's
    // bottom-left corner. Bevy / glTF / wgpu use top-left, so V is
    // inverted between the two systems. Flip on the way in so the
    // authored texture lands right-side-up — without this, eyes paint
    // on tails, etc.
    let uvs: Vec<[f32; 2]> = uvs.into_iter().map(|[u, v]| [u, 1.0 - v]).collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    if let Some(cs) = colors {
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, cs);
    }
    // Indices first so `compute_smooth_normals` has a topology to
    // average across — it requires an indexed mesh to find adjacent
    // faces.
    mesh.insert_indices(Indices::U32(indices));
    if let Some(ns) = normals {
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, ns);
    } else {
        // `compute_flat_normals` replicates positions so normals are per-face
        // — correct but bloats the mesh. Smooth normals keep the original
        // topology and average adjacent face normals. For plain USD stages
        // without authored normals that's the intuitive default.
        mesh.compute_smooth_normals();
    }
    // MikkT vertex tangents — Bevy's PBR shader needs `ATTRIBUTE_TANGENT`
    // to evaluate normal maps correctly. Without them, normal-mapped
    // surfaces silently fall back to geometric normals and the surface
    // detail (drummer stitching, glove leather, biplane rivets) looks
    // flat. `generate_tangents` requires positions + normals + UV0 — all
    // present at this point; failures are fatal-but-rare and we just log.
    if let Err(e) = mesh.generate_tangents() {
        bevy::log::debug!("mesh: generate_tangents failed: {e}");
    }
    mesh
}

/// Assembled mesh attributes: `(positions, normals?, uvs, colors?, indices)`.
type BuiltMesh = (
    Vec<[f32; 3]>,
    Option<Vec<[f32; 3]>>,
    Vec<[f32; 2]>,
    Option<Vec<[f32; 4]>>,
    Vec<u32>,
);

/// Build the common case: indexed triangle list, one vertex per USD point.
/// Uses vertex-level or constant interpolation only.
fn build_indexed(read: &ReadMesh, face_subset: Option<&[i32]>) -> BuiltMesh {
    let positions = read.points.clone();

    // Normals: pick up vertex-indexed data if present; else None and let
    // `compute_smooth_normals` handle it. `Varying` is semantically
    // per-point for polygonal meshes (USD spec) so we ride the same
    // path as `Vertex` — silently dropping it would force generated
    // smooth normals over authored ones.
    let normals = read.normals.as_ref().and_then(|p| match p.interpolation {
        Interpolation::Vertex | Interpolation::Varying => {
            Some(expand_vertex_primvar(p, positions.len(), [0.0, 1.0, 0.0]))
        }
        Interpolation::Constant if !p.values.is_empty() => Some(vec![p.values[0]; positions.len()]),
        _ => None,
    });

    let uvs = read
        .uvs
        .as_ref()
        .and_then(|p| match p.interpolation {
            Interpolation::Vertex | Interpolation::Varying => {
                Some(expand_vertex_primvar(p, positions.len(), [0.0, 0.0]))
            }
            _ => None,
        })
        .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);

    let colors = build_vertex_colors_indexed(read, positions.len());

    let indices = triangulate_polygon(
        &positions,
        &read.face_vertex_counts,
        &read.face_vertex_indices,
        read.orientation,
        face_subset,
    );
    (positions, normals, uvs, colors, indices)
}

/// For indexed output, displayColor / displayOpacity only contribute when
/// they're vertex- or constant-interpolated (faceVarying/uniform force the
/// expanded path). Returns `None` when there's nothing to emit so the
/// caller can skip writing the attribute at all.
fn build_vertex_colors_indexed(read: &ReadMesh, vertex_count: usize) -> Option<Vec<[f32; 4]>> {
    if read.display_color.is_none() && read.display_opacity.is_none() {
        return None;
    }
    let mut colors = vec![[1.0f32, 1.0, 1.0, 1.0]; vertex_count];
    if let Some(dc) = read.display_color.as_ref() {
        let rgbs = match dc.interpolation {
            Interpolation::Constant if !dc.values.is_empty() => {
                vec![dc.values[0]; vertex_count]
            }
            // Single-value primvar — broadcast regardless of which
            // interpolation token was authored. Pixar's Kitchen_set
            // authors `primvars:displayColor = [(0.5, 0.5, 0.4)]`
            // with no `interpolation` token; the schema reader's
            // default of `Vertex` then fails to expand a 1-element
            // array to vertex_count and falls through to white.
            _ if dc.values.len() == 1 => vec![dc.values[0]; vertex_count],
            // `Varying` is semantically per-vertex for polygonal meshes,
            // so it rides the same indexed path as `Vertex`.
            Interpolation::Vertex | Interpolation::Varying => {
                expand_vertex_primvar(dc, vertex_count, [1.0, 1.0, 1.0])
            }
            _ => vec![[1.0, 1.0, 1.0]; vertex_count],
        };
        for (i, rgb) in rgbs.iter().enumerate() {
            colors[i][0] = rgb[0];
            colors[i][1] = rgb[1];
            colors[i][2] = rgb[2];
        }
    }
    if let Some(dop) = read.display_opacity.as_ref() {
        let alphas = match dop.interpolation {
            Interpolation::Constant if !dop.values.is_empty() => {
                vec![dop.values[0]; vertex_count]
            }
            // Single-value primvar — broadcast regardless of declared
            // interpolation (see `display_color` arm above for the
            // Pixar Kitchen_set rationale).
            _ if dop.values.len() == 1 => vec![dop.values[0]; vertex_count],
            Interpolation::Vertex | Interpolation::Varying => {
                expand_vertex_primvar(dop, vertex_count, 1.0)
            }
            _ => vec![1.0; vertex_count],
        };
        for (i, a) in alphas.iter().enumerate() {
            colors[i][3] = *a;
        }
    }
    Some(colors)
}

/// Build the fully-expanded form: one vertex per face corner so `faceVarying`
/// primvars (cube uvs, seams) can be represented.
fn build_expanded(read: &ReadMesh, face_subset: Option<&[i32]>) -> BuiltMesh {
    let corner_count: usize = read.face_vertex_counts.iter().map(|c| (*c).max(0) as usize).sum();
    let mut positions = Vec::with_capacity(corner_count);
    let mut normals_out: Vec<[f32; 3]> = Vec::with_capacity(corner_count);
    let mut uvs_out: Vec<[f32; 2]> = Vec::with_capacity(corner_count);
    let mut colors_out: Vec<[f32; 4]> = Vec::with_capacity(corner_count);

    let want_normals = read.normals.is_some();
    let want_uvs = read.uvs.is_some();
    let want_colors = read.display_color.is_some() || read.display_opacity.is_some();

    // When normals aren't authored, compute them on the *unexpanded*
    // point-indexed mesh so vertices shared between faces produce a
    // smoothed (averaged) normal. If we let `Mesh::compute_smooth_normals`
    // run after expansion, every corner is its own vertex (because some
    // other primvar — usually FaceVarying UVs for texture seams — forced
    // expansion), so "smooth" normals collapse to face normals and you
    // see every polygon. Compute once, then index per corner.
    let smooth_per_point: Option<Vec<[f32; 3]>> =
        (!want_normals).then(|| compute_point_smooth_normals(read));

    let mut corner_ix: usize = 0;
    for (face_ix, face_verts) in read.face_vertex_counts.iter().enumerate() {
        for k in 0..((*face_verts).max(0) as usize) {
            // Tolerate malformed indices: a missing corner reads as 0, and an
            // index past the point buffer clamps to the last point (never OOB).
            let raw = read.face_vertex_indices.get(corner_ix + k).copied().unwrap_or(0);
            let point_ix = (raw.max(0) as usize).min(read.points.len().saturating_sub(1));
            positions.push(read.points.get(point_ix).copied().unwrap_or([0.0, 0.0, 0.0]));
            if want_normals {
                normals_out.push(corner_normal(read, face_ix, corner_ix + k, point_ix));
            } else if let Some(ref ns) = smooth_per_point {
                normals_out.push(*ns.get(point_ix).unwrap_or(&[0.0, 1.0, 0.0]));
            }
            if want_uvs {
                uvs_out.push(corner_uv(read, face_ix, corner_ix + k, point_ix));
            } else {
                // Pad UVs so `ATTRIBUTE_UV_0` always has the same
                // length as `ATTRIBUTE_POSITION` — mismatched lengths
                // make Bevy silently drop the mesh.
                uvs_out.push([0.0, 0.0]);
            }
            if want_colors {
                colors_out.push(corner_color(read, face_ix, corner_ix + k, point_ix));
            }
        }
        corner_ix += (*face_verts).max(0) as usize;
    }

    // After expansion, indices become sequential 0..N per face, then
    // fan-triangulated. Re-derive a pseudo `faceVertexIndices` of the form
    // [0,1,2,3, 4,5,6, …] so `triangulate_fan` can do its job.
    let mut sequential = Vec::with_capacity(corner_count);
    let mut running = 0u32;
    for face_verts in &read.face_vertex_counts {
        for _ in 0..*face_verts {
            sequential.push(running as i32);
            running += 1;
        }
    }
    let indices = triangulate_polygon(
        &positions,
        &read.face_vertex_counts,
        &sequential,
        read.orientation,
        face_subset,
    );

    // We emit normals whenever they were authored OR we synthesised
    // them from the point-smooth pass. The latter is the difference
    // between "smooth like Hydra" and "every face is visible".
    let emit_normals = want_normals || smooth_per_point.is_some();
    (
        positions,
        emit_normals.then_some(normals_out),
        uvs_out,
        want_colors.then_some(colors_out),
        indices,
    )
}

/// Per-point area-weighted smooth normals on the *unexpanded* mesh.
/// USD's faceVertexIndices is a flat per-corner list; we accumulate each
/// face's plane normal (scaled by 2× area) into all its corner points.
/// Ear-/fan-decompose larger faces just like the renderer does — using
/// (a, b, c) for k=1..n-1 keeps the contribution proportional to face
/// area for convex polygons and is good enough for concave ones since
/// a missing area cancels symmetrically.
///
/// Returns `read.points.len()` normals, normalised. Vertices unreferenced
/// by any face fall back to (0,1,0).
fn compute_point_smooth_normals(read: &ReadMesh) -> Vec<[f32; 3]> {
    let mut accum = vec![Vec3::ZERO; read.points.len()];
    // Tolerate a short/oversized `faceVertexIndices` (malformed USD): a missing
    // corner reads as index 0, and any index past the point buffer is skipped
    // rather than panicking.
    let fvi = |c: usize| -> usize {
        read.face_vertex_indices.get(c).copied().unwrap_or(0).max(0) as usize
    };
    let mut corner_ix = 0usize;
    for face_verts in &read.face_vertex_counts {
        let n = (*face_verts).max(0) as usize;
        if n >= 3 {
            let i0 = fvi(corner_ix);
            if let Some(p0a) = read.points.get(i0) {
                let p0 = Vec3::from_array(*p0a);
                for k in 1..(n - 1) {
                    let i1 = fvi(corner_ix + k);
                    let i2 = fvi(corner_ix + k + 1);
                    if let (Some(p1a), Some(p2a)) =
                        (read.points.get(i1), read.points.get(i2))
                    {
                        let p1 = Vec3::from_array(*p1a);
                        let p2 = Vec3::from_array(*p2a);
                        let face_n = match read.orientation {
                            Orientation::RightHanded => (p1 - p0).cross(p2 - p0),
                            Orientation::LeftHanded => (p2 - p0).cross(p1 - p0),
                        };
                        if let Some(a) = accum.get_mut(i0) {
                            *a += face_n;
                        }
                        if let Some(a) = accum.get_mut(i1) {
                            *a += face_n;
                        }
                        if let Some(a) = accum.get_mut(i2) {
                            *a += face_n;
                        }
                    }
                }
            }
        }
        corner_ix += n;
    }
    accum
        .into_iter()
        .map(|v| {
            if v.length_squared() > 1e-20 {
                let n = v.normalize();
                [n.x, n.y, n.z]
            } else {
                [0.0, 1.0, 0.0]
            }
        })
        .collect()
}

fn corner_normal(read: &ReadMesh, face: usize, corner: usize, point: usize) -> [f32; 3] {
    let p = read.normals.as_ref().unwrap();
    sample_primvar_3(p, face, corner, point, [0.0, 1.0, 0.0])
}

fn corner_color(read: &ReadMesh, face: usize, corner: usize, point: usize) -> [f32; 4] {
    let rgb_fallback = [1.0_f32, 1.0, 1.0];
    let rgb = read
        .display_color
        .as_ref()
        .map(|dc| sample_primvar_3(dc, face, corner, point, rgb_fallback))
        .unwrap_or(rgb_fallback);
    let a = read
        .display_opacity
        .as_ref()
        .map(|dop| sample_primvar_1(dop, face, corner, point, 1.0))
        .unwrap_or(1.0);
    [rgb[0], rgb[1], rgb[2], a]
}

/// Sample a vec3 primvar at a specific corner. Single-value primvars
/// broadcast regardless of declared interpolation — Pixar's
/// Kitchen_set authors `primvars:displayColor = [(0.5, 0.5, 0.4)]`
/// without an `interpolation` token; the schema reader's default of
/// `Vertex` then fails to expand the 1-element array to vertex_count
/// and falls back to white.
fn sample_primvar_3(
    p: &MeshPrimvar<[f32; 3]>,
    face: usize,
    corner: usize,
    point: usize,
    fallback: [f32; 3],
) -> [f32; 3] {
    if p.values.len() == 1 {
        return p.values[0];
    }
    let lookup = |slot: usize| -> [f32; 3] {
        let ix = if !p.indices.is_empty() {
            *p.indices.get(slot).unwrap_or(&0) as usize
        } else {
            slot
        };
        p.values.get(ix).copied().unwrap_or(fallback)
    };
    match p.interpolation {
        Interpolation::Constant => p.values.first().copied().unwrap_or(fallback),
        Interpolation::Uniform => lookup(face),
        Interpolation::Vertex | Interpolation::Varying => lookup(point),
        Interpolation::FaceVarying => lookup(corner),
    }
}

fn sample_primvar_1(
    p: &MeshPrimvar<f32>,
    face: usize,
    corner: usize,
    point: usize,
    fallback: f32,
) -> f32 {
    if p.values.len() == 1 {
        return p.values[0];
    }
    let lookup = |slot: usize| -> f32 {
        let ix = if !p.indices.is_empty() {
            *p.indices.get(slot).unwrap_or(&0) as usize
        } else {
            slot
        };
        p.values.get(ix).copied().unwrap_or(fallback)
    };
    match p.interpolation {
        Interpolation::Constant => p.values.first().copied().unwrap_or(fallback),
        Interpolation::Uniform => lookup(face),
        Interpolation::Vertex | Interpolation::Varying => lookup(point),
        Interpolation::FaceVarying => lookup(corner),
    }
}

fn corner_uv(read: &ReadMesh, face: usize, corner: usize, point: usize) -> [f32; 2] {
    let p = read.uvs.as_ref().unwrap();
    let fallback = [0.0, 0.0];
    if p.values.len() == 1 {
        return p.values[0];
    }
    match p.interpolation {
        Interpolation::Constant => p.values.first().copied().unwrap_or(fallback),
        Interpolation::Uniform => {
            let ix = if !p.indices.is_empty() {
                *p.indices.get(face).unwrap_or(&0) as usize
            } else {
                face
            };
            p.values.get(ix).copied().unwrap_or(fallback)
        }
        Interpolation::Vertex | Interpolation::Varying => {
            let ix = if !p.indices.is_empty() {
                *p.indices.get(point).unwrap_or(&0) as usize
            } else {
                point
            };
            p.values.get(ix).copied().unwrap_or(fallback)
        }
        Interpolation::FaceVarying => {
            let ix = if !p.indices.is_empty() {
                *p.indices.get(corner).unwrap_or(&0) as usize
            } else {
                corner
            };
            p.values.get(ix).copied().unwrap_or(fallback)
        }
    }
}

fn expand_vertex_primvar<T: Copy>(
    primvar: &MeshPrimvar<T>,
    expected_len: usize,
    fallback: T,
) -> Vec<T> {
    // Always emit exactly `expected_len` entries — Bevy 0.18 silently
    // drops the mesh if attribute lengths don't match
    // `ATTRIBUTE_POSITION`. Pad with `fallback` if the authored data
    // is short, truncate if it's long.
    let mut out = vec![fallback; expected_len];
    if primvar.indices.is_empty() {
        for (i, v) in primvar.values.iter().take(expected_len).enumerate() {
            out[i] = *v;
        }
    } else {
        for (i, ix) in primvar.indices.iter().take(expected_len).enumerate() {
            if let Some(v) = primvar.values.get(*ix as usize) {
                out[i] = *v;
            }
        }
    }
    out
}

/// Triangulate each face into a triangle list. Smart enough for the three
/// cases real USD assets throw at us:
///
/// - **n = 3**: emit as-is.
/// - **n = 4** (the dominant case in production assets): pick the *shorter*
///   diagonal. Non-planar quads — almost universal in subdivided cages and
///   imported FBX — produce a visible crease along whichever diagonal a fan
///   triangulator picks. Choosing the diagonal that minimises the triangle
///   pair's perimeter aligns the crease with the surface curvature, which
///   is what every offline renderer (and Maya/Blender's default) does.
/// - **n ≥ 5**: ear-clip. Fan triangulation of a concave n-gon emits
///   triangles *outside* the polygon (showing through to the back) and
///   misses parts inside it — which is exactly the "spiky / missing
///   triangles" symptom on production-asset n-gons. Ear clipping handles
///   concave faces correctly. We compute the polygon normal via Newell's
///   method (works on non-planar polygons too) and pick ears in 2D after
///   projecting onto the plane perpendicular to that normal.
///
/// Falls back to fan triangulation if the polygon is degenerate (all colinear
/// points) — emitting *something* matches USD's permissive behaviour.
///
/// `LeftHanded` orientation flips the winding so Bevy's default back-face
/// culling shows the right side.
///
/// `face_subset = Some(&[face_ix])` emits only the listed faces — used by
/// the GeomSubset per-material split.
fn triangulate_polygon(
    positions: &[[f32; 3]],
    counts: &[i32],
    indices: &[i32],
    orientation: Orientation,
    face_subset: Option<&[i32]>,
) -> Vec<u32> {
    // No vertices → no triangles. Emitting indices into an empty buffer would
    // later panic Bevy's normal/tangent generation.
    if positions.is_empty() {
        return Vec::new();
    }
    // Precompute each face's starting corner so a subset by face index
    // jumps straight to the right slice without rewalking the counts. Negative
    // counts (malformed USD) contribute zero rather than wrapping to a huge
    // `usize` that would overflow the running sum.
    let mut face_starts = Vec::with_capacity(counts.len());
    let mut running = 0usize;
    for c in counts {
        face_starts.push(running);
        running += (*c).max(0) as usize;
    }

    let face_iter: Box<dyn Iterator<Item = usize>> = match face_subset {
        None => Box::new(0..counts.len()),
        Some(sub) => Box::new(
            sub.iter()
                .map(|i| *i as usize)
                .filter(|i| *i < counts.len()),
        ),
    };

    let mut out = Vec::new();
    let emit = |out: &mut Vec<u32>, a: u32, b: u32, c: u32| match orientation {
        Orientation::RightHanded => out.extend_from_slice(&[a, b, c]),
        Orientation::LeftHanded => out.extend_from_slice(&[a, c, b]),
    };
    let nv = positions.len();
    // Read a corner's vertex index, tolerating an index array shorter than the
    // counts imply (malformed USD) — a missing corner reads as index 0.
    let idx_at = |c: usize| -> i32 { indices.get(c).copied().unwrap_or(0) };
    // Clamp a raw (possibly out-of-range or negative) point index so an emitted
    // mesh index never points past the vertex buffer (which the GPU would read
    // out of bounds).
    let clamp_v = |i: i32| -> u32 {
        if nv == 0 {
            0
        } else {
            (i.max(0) as usize).min(nv - 1) as u32
        }
    };
    let pos_of = |idx: i32| -> Vec3 {
        match positions.get(idx.max(0) as usize) {
            Some(p) => Vec3::new(p[0], p[1], p[2]),
            None => Vec3::ZERO,
        }
    };

    for face_ix in face_iter {
        let face_start = face_starts[face_ix];
        let n = counts[face_ix].max(0) as usize;
        if n < 3 {
            continue;
        }
        if n == 3 {
            let a = clamp_v(idx_at(face_start));
            let b = clamp_v(idx_at(face_start + 1));
            let c = clamp_v(idx_at(face_start + 2));
            emit(&mut out, a, b, c);
            continue;
        }
        if n == 4 {
            let i0 = idx_at(face_start);
            let i1 = idx_at(face_start + 1);
            let i2 = idx_at(face_start + 2);
            let i3 = idx_at(face_start + 3);
            // Pick the shorter diagonal: 0–2 vs 1–3.
            let p0 = pos_of(i0);
            let p1 = pos_of(i1);
            let p2 = pos_of(i2);
            let p3 = pos_of(i3);
            let d02 = (p2 - p0).length_squared();
            let d13 = (p3 - p1).length_squared();
            if d02 <= d13 {
                emit(&mut out, clamp_v(i0), clamp_v(i1), clamp_v(i2));
                emit(&mut out, clamp_v(i0), clamp_v(i2), clamp_v(i3));
            } else {
                emit(&mut out, clamp_v(i1), clamp_v(i2), clamp_v(i3));
                emit(&mut out, clamp_v(i1), clamp_v(i3), clamp_v(i0));
            }
            continue;
        }
        // n >= 5: ear clip. Clamp the slice end so a counts/indices mismatch
        // can't panic; skip the face if fewer than a triangle survives. Corner
        // indices are pre-clamped so ear-clip's emitted mesh indices stay valid.
        let end = (face_start + n).min(indices.len());
        if end.saturating_sub(face_start) < 3 {
            continue;
        }
        let face_indices: Vec<i32> = indices[face_start..end]
            .iter()
            .map(|i| clamp_v(*i) as i32)
            .collect();
        let face_positions: Vec<Vec3> = face_indices.iter().map(|i| pos_of(*i)).collect();
        ear_clip_into(&face_positions, &face_indices, &mut out, orientation);
    }
    out
}

/// Ear-clip a polygon (n ≥ 4 in practice) into triangles, appending into
/// `out`. Robust against concave polygons; for non-planar polygons we
/// project onto the plane perpendicular to the Newell normal so the 2D
/// containment test is meaningful.
///
/// Falls back to fan triangulation if no ears can be found (e.g. fully
/// degenerate / self-intersecting input). That matches USD's "translate
/// what you can, drop nothing" expectation.
fn ear_clip_into(
    positions: &[Vec3],
    indices: &[i32],
    out: &mut Vec<u32>,
    orientation: Orientation,
) {
    let n = positions.len();
    let emit = |out: &mut Vec<u32>, a: u32, b: u32, c: u32| match orientation {
        Orientation::RightHanded => out.extend_from_slice(&[a, b, c]),
        Orientation::LeftHanded => out.extend_from_slice(&[a, c, b]),
    };

    // Newell's method: robust normal even for non-planar polygons. Sums
    // per-edge cross-products of the projected components.
    let mut normal = Vec3::ZERO;
    for i in 0..n {
        let cur = positions[i];
        let nxt = positions[(i + 1) % n];
        normal.x += (cur.y - nxt.y) * (cur.z + nxt.z);
        normal.y += (cur.z - nxt.z) * (cur.x + nxt.x);
        normal.z += (cur.x - nxt.x) * (cur.y + nxt.y);
    }
    if normal.length_squared() < 1e-20 {
        // Degenerate polygon — fall back to fan.
        for k in 1..(n - 1) {
            emit(
                out,
                indices[0] as u32,
                indices[k] as u32,
                indices[k + 1] as u32,
            );
        }
        return;
    }
    let normal = normal.normalize();

    // Build orthonormal basis (u, v) on the polygon plane to project into
    // 2D. Pick the smallest absolute component of the normal as the
    // helper axis to avoid degeneracy.
    let helper = if normal.x.abs() < normal.y.abs() && normal.x.abs() < normal.z.abs() {
        Vec3::X
    } else if normal.y.abs() < normal.z.abs() {
        Vec3::Y
    } else {
        Vec3::Z
    };
    let u = normal.cross(helper).normalize();
    let v = normal.cross(u);
    let project = |p: Vec3| -> [f32; 2] { [p.dot(u), p.dot(v)] };

    let pts2: Vec<[f32; 2]> = positions.iter().map(|p| project(*p)).collect();

    // Determine polygon winding in 2D. Signed area > 0 → CCW.
    let mut signed_area = 0.0f32;
    for i in 0..n {
        let a = pts2[i];
        let b = pts2[(i + 1) % n];
        signed_area += a[0] * b[1] - b[0] * a[1];
    }
    let ccw = signed_area > 0.0;

    // Active vertex list (linked-list-style via Vec).
    let mut remaining: Vec<usize> = (0..n).collect();
    // Worst-case ear clipping is O(n²) but n is tiny (≤ ~10 in practice).
    let max_iters = n * n + 8;
    let mut iters = 0;
    while remaining.len() > 3 && iters < max_iters {
        iters += 1;
        let m = remaining.len();
        let mut clipped = false;
        for i in 0..m {
            let i_prev = remaining[(i + m - 1) % m];
            let i_cur = remaining[i];
            let i_next = remaining[(i + 1) % m];
            let a = pts2[i_prev];
            let b = pts2[i_cur];
            let c = pts2[i_next];
            // Convex test in chosen winding.
            let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            let convex = if ccw { cross > 0.0 } else { cross < 0.0 };
            if !convex {
                continue;
            }
            // Ear test: no other remaining vertex inside triangle (a,b,c).
            let mut contains_other = false;
            for &idx in &remaining {
                if idx == i_prev || idx == i_cur || idx == i_next {
                    continue;
                }
                let p = pts2[idx];
                if point_in_triangle_2d(p, a, b, c) {
                    contains_other = true;
                    break;
                }
            }
            if contains_other {
                continue;
            }
            // Emit and clip.
            emit(
                out,
                indices[i_prev] as u32,
                indices[i_cur] as u32,
                indices[i_next] as u32,
            );
            remaining.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            // No ear found — bail out and fan-triangulate the rest.
            break;
        }
    }
    if remaining.len() == 3 {
        emit(
            out,
            indices[remaining[0]] as u32,
            indices[remaining[1]] as u32,
            indices[remaining[2]] as u32,
        );
    } else if remaining.len() > 3 {
        // Fallback: fan over what's left.
        let r0 = remaining[0];
        for k in 1..(remaining.len() - 1) {
            emit(
                out,
                indices[r0] as u32,
                indices[remaining[k]] as u32,
                indices[remaining[k + 1]] as u32,
            );
        }
    }
}

/// Standard barycentric inside-triangle test. Includes points exactly on
/// edges (we still emit ears even if a vertex sits on an edge — the
/// alternative is endless retries on collinear data).
fn point_in_triangle_2d(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let d_x = p[0] - c[0];
    let d_y = p[1] - c[1];
    let denom = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
    if denom.abs() < 1e-20 {
        return false;
    }
    let s = ((b[1] - c[1]) * d_x + (c[0] - b[0]) * d_y) / denom;
    let t = ((c[1] - a[1]) * d_x + (a[0] - c[0]) * d_y) / denom;
    s > 0.0 && t > 0.0 && (s + t) < 1.0
}

// ── Primitive shapes ────────────────────────────────────────────────────

/// Build a Bevy mesh from a UsdGeom.Cube's `size`. The USD cube is
/// size × size × size centred at the prim origin.
pub fn mesh_cube(size: f64) -> Mesh {
    Mesh::from(bevy::math::primitives::Cuboid::new(
        size as f32,
        size as f32,
        size as f32,
    ))
}

/// UsdGeom.Sphere radius → Bevy's UV sphere.
pub fn mesh_sphere(radius: f64) -> Mesh {
    Mesh::from(bevy::math::primitives::Sphere::new(radius as f32))
}

/// UsdGeom.Cylinder dimensions + axis. Bevy's `Cylinder` points up the Y
/// axis by convention, so we apply an axis rotation for X/Z cases.
pub fn mesh_cylinder(params: ReadCylinder) -> Mesh {
    let mut mesh = Mesh::from(bevy::math::primitives::Cylinder::new(
        params.radius as f32,
        params.height as f32,
    ));
    apply_axis(&mut mesh, params.axis);
    mesh
}

/// UsdGeom.Plane `width` × `length`. Y-normal plane centred at the origin.
pub fn mesh_plane(width: f64, length: f64) -> Mesh {
    Mesh::from(
        bevy::math::primitives::Plane3d::default()
            .mesh()
            .size(width as f32, length as f32),
    )
}

/// UsdGeom.Capsule dimensions + axis. Bevy's `Capsule3d` is Y-axis aligned.
pub fn mesh_capsule(params: ReadCylinder) -> Mesh {
    // UsdGeom.Capsule's `height` is the cylinder portion length (hemispheres
    // add `2*radius` to the total). Bevy's Capsule3d takes `half_length` =
    // half the cylinder portion.
    let mut mesh = Mesh::from(bevy::math::primitives::Capsule3d::new(
        params.radius as f32,
        params.height as f32,
    ));
    apply_axis(&mut mesh, params.axis);
    mesh
}

/// Rotate vertices so a Y-up primitive faces the requested axis.
fn apply_axis(mesh: &mut Mesh, axis: Axis) {
    let rot = match axis {
        Axis::Y => return,
        Axis::X => bevy::math::Quat::from_rotation_z(-core::f32::consts::FRAC_PI_2),
        Axis::Z => bevy::math::Quat::from_rotation_x(core::f32::consts::FRAC_PI_2),
    };
    rotate_mesh(mesh, rot);
}

pub fn rotate_mesh(mesh: &mut Mesh, rot: bevy::math::Quat) {
    if let Some(VertexAttributeValues::Float32x3(ps)) = mesh.attribute_mut(Mesh::ATTRIBUTE_POSITION)
    {
        for p in ps.iter_mut() {
            let v = rot * Vec3::new(p[0], p[1], p[2]);
            *p = [v.x, v.y, v.z];
        }
    }
    if let Some(VertexAttributeValues::Float32x3(ns)) = mesh.attribute_mut(Mesh::ATTRIBUTE_NORMAL) {
        for n in ns.iter_mut() {
            let v = rot * Vec3::new(n[0], n[1], n[2]);
            *n = [v.x, v.y, v.z];
        }
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use crate::read::geom::SubdivScheme;

    /// Minimal `ReadMesh` with the geometry under test and everything else empty.
    fn mesh(points: Vec<[f32; 3]>, counts: Vec<i32>, indices: Vec<i32>) -> ReadMesh {
        ReadMesh {
            points,
            face_vertex_counts: counts,
            face_vertex_indices: indices,
            normals: None,
            uvs: None,
            orientation: Orientation::RightHanded,
            display_color: None,
            display_opacity: None,
            subsets: Vec::new(),
            double_sided: false,
            extent: None,
            subdivision_scheme: SubdivScheme::None,
        }
    }

    /// Bevy silently drops a mesh whose attribute lengths disagree with
    /// `ATTRIBUTE_POSITION`; every build path must keep them equal.
    fn assert_consistent(m: &Mesh) {
        let pos = m.count_vertices();
        if let Some(VertexAttributeValues::Float32x2(uv)) = m.attribute(Mesh::ATTRIBUTE_UV_0) {
            assert_eq!(uv.len(), pos, "uv length matches positions");
        }
        if let Some(VertexAttributeValues::Float32x3(n)) = m.attribute(Mesh::ATTRIBUTE_NORMAL) {
            assert_eq!(n.len(), pos, "normal length matches positions");
        }
    }

    #[test]
    fn out_of_range_indices_do_not_panic() {
        // A triangle references point 99 with only 3 points authored.
        let m = mesh(
            vec![[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
            vec![3],
            vec![0, 1, 99],
        );
        assert_consistent(&mesh_from_usd(&m));
    }

    #[test]
    fn counts_exceeding_indices_do_not_panic() {
        // Counts claim a quad, but only three indices exist.
        let m = mesh(
            vec![[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
            vec![4],
            vec![0, 1, 2],
        );
        assert_consistent(&mesh_from_usd(&m));
    }

    #[test]
    fn ngon_with_truncated_indices_does_not_panic() {
        // A declared 6-gon whose index array is too short — exercises the
        // ear-clip slice guard.
        let m = mesh(
            vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.]],
            vec![6],
            vec![0, 1, 2],
        );
        let _ = mesh_from_usd(&m);
    }

    #[test]
    fn negative_counts_and_indices_do_not_panic() {
        let m = mesh(
            vec![[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
            vec![-1, 3],
            vec![-5, 1, 2],
        );
        assert_consistent(&mesh_from_usd(&m));
    }

    #[test]
    fn expanded_path_out_of_range_does_not_panic() {
        // A faceVarying uv forces the expanded build path; indices still point
        // past the buffer.
        let mut m = mesh(
            vec![[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
            vec![3],
            vec![0, 1, 50],
        );
        m.uvs = Some(MeshPrimvar {
            values: vec![[0., 0.], [1., 0.], [0., 1.]],
            interpolation: Interpolation::FaceVarying,
            indices: Vec::new(),
        });
        assert_consistent(&mesh_from_usd(&m));
    }

    #[test]
    fn empty_points_with_faces_do_not_panic() {
        let m = mesh(Vec::new(), vec![3], vec![0, 1, 2]);
        let _ = mesh_from_usd(&m);
    }

    #[test]
    fn valid_quad_still_triangulates() {
        // Regression: a well-formed quad must still fan into two triangles.
        let m = mesh(
            vec![[0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.]],
            vec![4],
            vec![0, 1, 2, 3],
        );
        let mesh = mesh_from_usd(&m);
        assert_eq!(mesh.indices().map(|i| i.len()), Some(6), "quad → 2 tris");
    }
}
