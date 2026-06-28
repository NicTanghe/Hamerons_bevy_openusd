//! Live, editable USD stage + change-driven reprojection (RETHINK P2/P3).
//!
//! The composed USD stage is the single source of truth. We hold it live
//! (not baked to a one-shot `Scene`), project it into Bevy entities, and
//! keep them in sync off openusd's `StageSink` (`UsdNotice`) change stream:
//! every committed edit fires the sink, we copy the changed paths out, and a
//! Bevy system reprojects exactly the affected entities.
//!
//! The openusd `Stage` is `Rc`/`RefCell`-backed (`!Send`), so [`LiveStage`]
//! is a **non-send** resource (main thread only). The path↔entity index
//! [`PrimEntities`] is plain data and is a normal `Resource`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use bevy::prelude::*;
use openusd::usd::{CommittedChange, Stage, StageSinkId};

/// One committed stage change, copied out of the borrowed [`CommittedChange`]
/// so it can outlive the sink callback and be drained on a later frame.
///
/// * `resynced` — composition restructured (define / remove / reparent /
///   variant / reference / layer-mute …); the subtree must be reprojected.
/// * `changed_info` — a field/value/target changed, namespace intact; the
///   corresponding component(s) can be patched in place.
#[derive(Clone, Debug, Default)]
pub struct StageChange {
    pub resynced: Vec<String>,
    pub changed_info: Vec<String>,
}

impl StageChange {
    /// All paths mentioned by this change (resynced ∪ changed-info).
    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.resynced.iter().chain(self.changed_info.iter())
    }
}

/// The live, editable USD stage and its change queue. **Non-send** — insert
/// via `world.insert_non_send_resource(LiveStage::new(stage))`.
///
/// Authoring goes through `live.stage` (every method is `&self`); each commit
/// fires the installed sink, which records a [`StageChange`] onto the queue.
/// A reprojection system drains the queue once per frame.
pub struct LiveStage {
    pub stage: Stage,
    queue: Rc<RefCell<Vec<StageChange>>>,
    // Kept so the sink lives as long as the stage; removed on drop.
    sink: Option<StageSinkId>,
}

impl LiveStage {
    /// Wrap a stage and install the change sink.
    pub fn new(stage: Stage) -> Self {
        let queue: Rc<RefCell<Vec<StageChange>>> = Rc::new(RefCell::new(Vec::new()));
        let q = queue.clone();
        let sink = stage.add_sink(move |_stage: &Stage, change: &CommittedChange<'_>| {
            q.borrow_mut().push(StageChange {
                resynced: change.resynced.iter().map(|p| p.as_str().to_string()).collect(),
                changed_info: change
                    .changed_info_only
                    .iter()
                    .map(|p| p.as_str().to_string())
                    .collect(),
            });
        });
        Self {
            stage,
            queue,
            sink: Some(sink),
        }
    }

    /// Take and clear all changes recorded since the last drain.
    pub fn drain_changes(&self) -> Vec<StageChange> {
        std::mem::take(&mut *self.queue.borrow_mut())
    }

    /// Whether any change is pending (cheap check before doing work).
    pub fn has_changes(&self) -> bool {
        !self.queue.borrow().is_empty()
    }
}

impl Drop for LiveStage {
    fn drop(&mut self) {
        if let Some(id) = self.sink.take() {
            self.stage.remove_sink(id);
        }
    }
}

/// Bidirectional `SdfPath ↔ Entity` index — the reprojection key. Plain
/// `Resource` (the paths are owned `String`s, the entities are ids).
#[derive(Resource, Default)]
pub struct PrimEntities {
    by_path: HashMap<String, Entity>,
    by_entity: HashMap<Entity, String>,
}

impl PrimEntities {
    pub fn insert(&mut self, path: impl Into<String>, entity: Entity) {
        let path = path.into();
        self.by_entity.insert(entity, path.clone());
        self.by_path.insert(path, entity);
    }

    pub fn entity(&self, path: &str) -> Option<Entity> {
        self.by_path.get(path).copied()
    }

    pub fn path(&self, entity: Entity) -> Option<&str> {
        self.by_entity.get(&entity).map(String::as_str)
    }

    /// Remove a path's mapping, returning the entity it pointed at.
    pub fn remove_path(&mut self, path: &str) -> Option<Entity> {
        let e = self.by_path.remove(path)?;
        self.by_entity.remove(&e);
        Some(e)
    }

    /// Remove an entity's mapping (e.g. on despawn).
    pub fn remove_entity(&mut self, entity: Entity) -> Option<String> {
        let p = self.by_entity.remove(&entity)?;
        self.by_path.remove(&p);
        Some(p)
    }

    /// Every `(path, entity)` currently mapped.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Entity)> {
        self.by_path.iter().map(|(p, e)| (p.as_str(), *e))
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// Every `(path, entity)` whose path is `prefix` or a descendant of it —
    /// the set a `resynced` parent invalidates.
    pub fn subtree(&self, prefix: &str) -> Vec<(String, Entity)> {
        let with_slash = format!("{prefix}/");
        self.by_path
            .iter()
            .filter(|(p, _)| p.as_str() == prefix || p.starts_with(&with_slash))
            .map(|(p, e)| (p.clone(), *e))
            .collect()
    }
}

// ─── Projection + reprojection (v1: transforms) ─────────────────────
//
// Minimal slice of the project/sync loop: one entity per prim carrying
// `UsdPrimRef` + `Transform`. Mesh / material / the full field→component
// routing (RETHINK §12) layer on top of this same shape.

use crate::prim_ref::UsdPrimRef;
use crate::read::xform::read_transform;

fn to_bevy_transform(t: crate::read::xform::Transform3) -> Transform {
    Transform {
        translation: Vec3::from_array(t.translate),
        rotation: Quat::from_array(t.rotate),
        scale: Vec3::from_array(t.scale),
    }
}

fn transform_at(stage: &Stage, prim_path: &str) -> Transform {
    openusd::sdf::path(prim_path)
        .ok()
        .and_then(|p| read_transform(stage, &p).ok().flatten())
        .map(to_bevy_transform)
        .unwrap_or_default()
}

/// The prim path owning a (possibly property) path: `/Foo.bar` → `/Foo`.
fn prim_of(path: &str) -> &str {
    path.split('.').next().unwrap_or(path)
}

/// Project every prim in the stage into an entity (`UsdPrimRef` +
/// `Transform`), recording the path↔entity bimap. Idempotent only on an
/// empty world — call once on load.
pub fn project_stage(world: &mut World, live: &LiveStage, map: &mut PrimEntities) {
    let stage = &live.stage;
    let _ = stage.traverse(openusd::usd::PrimPredicate::default(), |path: &openusd::sdf::Path| {
        let transform = transform_at(stage, path.as_str());
        let entity = world
            .spawn((
                UsdPrimRef {
                    path: path.as_str().to_string(),
                    ..Default::default()
                },
                transform,
            ))
            .id();
        map.insert(path.as_str().to_string(), entity);
    });
    // Projecting authored the initial read; clear so the first sync starts clean.
    let _ = live.drain_changes();
}

/// Drain the change queue and reproject affected entities.
///
/// * Any `resynced` change → reconcile the entity set against the stage
///   (spawn entities for new prims, despawn entities for removed prims,
///   patch the rest). v1 reconciles the whole stage; a later version scopes
///   to the resynced subtree.
/// * `changed_info` only → patch the touched prims' transforms in place.
pub fn apply_changes(world: &mut World, live: &LiveStage, map: &mut PrimEntities) {
    let changes = live.drain_changes();
    if changes.is_empty() {
        return;
    }
    if changes.iter().any(|c| !c.resynced.is_empty()) {
        reconcile(world, live, map);
        return;
    }
    for change in &changes {
        for path in change.paths() {
            let prim = prim_of(path);
            if let Some(entity) = map.entity(prim) {
                let t = transform_at(&live.stage, prim);
                if let Some(mut tr) = world.get_mut::<Transform>(entity) {
                    *tr = t;
                }
            }
        }
    }
}

/// Reconcile the projected entities against the stage's current prims:
/// despawn entities whose prim was removed, spawn entities for new prims,
/// patch transforms on the rest.
fn reconcile(world: &mut World, live: &LiveStage, map: &mut PrimEntities) {
    let stage = &live.stage;
    let mut current: std::collections::HashSet<String> = std::collections::HashSet::new();
    let _ = stage.traverse(openusd::usd::PrimPredicate::default(), |p: &openusd::sdf::Path| {
        current.insert(p.as_str().to_string());
    });

    // Despawn entities for prims no longer present.
    let stale: Vec<(String, Entity)> = map
        .iter()
        .filter(|(p, _)| !current.contains(*p))
        .map(|(p, e)| (p.to_string(), e))
        .collect();
    for (path, entity) in stale {
        world.despawn(entity);
        map.remove_path(&path);
    }

    // Spawn new prims; patch transforms on existing ones.
    for path in &current {
        let t = transform_at(stage, path);
        if let Some(entity) = map.entity(path) {
            if let Some(mut tr) = world.get_mut::<Transform>(entity) {
                *tr = t;
            }
        } else {
            let entity = world
                .spawn((
                    UsdPrimRef {
                        path: path.clone(),
                        ..Default::default()
                    },
                    t,
                ))
                .id();
            map.insert(path.clone(), entity);
        }
    }
}

// ─── Authoring back (entity edit → stage) ───────────────────────────
//
// The write direction: an entity's `Transform` (e.g. after a gizmo drag)
// authored back onto the prim as a single `xformOp:transform` matrix under
// the stage's current edit target. The commit fires the sink, so the edit
// re-projects like any other change (idempotent — the entity already holds
// the value). Authoring one matrix op (instead of decomposed T/R/S) keeps a
// clean round-trip with `read_transform`.

/// Author `transform` onto `prim_path` as `xformOp:transform`. Errors if the
/// path is malformed or the layer rejects the edit.
pub fn author_transform(
    stage: &Stage,
    prim_path: &str,
    transform: &Transform,
) -> anyhow::Result<()> {
    use openusd::sdf::Value;
    let prim = openusd::sdf::path(prim_path)?;
    let cols =
        Mat4::from_scale_rotation_translation(transform.scale, transform.rotation, transform.translation)
            .to_cols_array();
    let m: [f64; 16] = std::array::from_fn(|i| cols[i] as f64);

    let xop = prim.append_property("xformOp:transform")?;
    stage
        .create_attribute(xop, "matrix4d")?
        .set(Value::Matrix4d(openusd::gf::Matrix4d(m)))?;
    let order = prim.append_property("xformOpOrder")?;
    stage
        .create_attribute(order, "token[]")?
        .set(Value::TokenVec(vec!["xformOp:transform".into()]))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openusd::sdf::Value;

    /// The `UsdNotice` loop: authoring an edit fires the sink, and the change
    /// (mentioning the edited path) lands on the drainable queue. This is the
    /// foundation the whole live-editor reprojection is built on.
    #[test]
    fn sink_records_authored_edits() {
        let stage = Stage::builder()
            .in_memory("live_test.usda")
            .expect("in-memory stage");
        stage
            .define_prim("/Foo")
            .expect("define")
            .set_type_name("Xform")
            .expect("type");

        let live = LiveStage::new(stage);
        assert!(!live.has_changes(), "no changes before any edit");

        // Author an attribute on /Foo — this commits and should fire the sink.
        live.stage
            .create_attribute("/Foo.size", "double")
            .expect("create attr")
            .set(Value::Double(2.0))
            .expect("set value");

        let changes = live.drain_changes();
        assert!(!changes.is_empty(), "the edit should have recorded a change");
        let mentioned: Vec<String> = changes.iter().flat_map(|c| c.paths().cloned()).collect();
        assert!(
            mentioned.iter().any(|p| p.starts_with("/Foo")),
            "change should mention /Foo, got {mentioned:?}"
        );
        assert!(live.drain_changes().is_empty(), "queue is empty after drain");
    }

    /// Namespace edits (define/remove) surface as `resynced` — the signal to
    /// reproject a subtree.
    #[test]
    fn define_and_remove_resync() {
        let stage = Stage::builder().in_memory("resync.usda").unwrap();
        let live = LiveStage::new(stage);

        live.stage.define_prim("/World").unwrap();
        live.stage.define_prim("/World/Child").unwrap();
        let after_define = live.drain_changes();
        let resynced: Vec<String> = after_define.iter().flat_map(|c| c.resynced.clone()).collect();
        assert!(
            resynced.iter().any(|p| p.starts_with("/World")),
            "defining prims should resync /World, got resynced={resynced:?}"
        );

        live.stage.remove_prim("/World/Child").unwrap();
        let after_remove = live.drain_changes();
        assert!(
            !after_remove.is_empty(),
            "removing a prim should record a change"
        );
    }

    /// The full loop: project a prim's transform into an entity, author a
    /// new translate on the stage, sync, and confirm the entity's
    /// `Transform` was reprojected from the edit.
    #[test]
    fn edit_reprojects_transform() {
        let stage = Stage::builder().in_memory("e2e.usda").unwrap();
        stage.define_prim("/Foo").unwrap().set_type_name("Xform").unwrap();
        stage
            .create_attribute("/Foo.xformOp:translate", "double3")
            .unwrap()
            .set(Value::Vec3d(openusd::gf::Vec3d::from([1.0, 0.0, 0.0])))
            .unwrap();
        stage
            .create_attribute("/Foo.xformOpOrder", "token[]")
            .unwrap()
            .set(Value::TokenVec(vec!["xformOp:translate".into()]))
            .unwrap();

        let live = LiveStage::new(stage);
        let mut world = World::new();
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);

        let foo = map.entity("/Foo").expect("/Foo projected");
        assert_eq!(
            world.get::<Transform>(foo).unwrap().translation,
            Vec3::new(1.0, 0.0, 0.0),
            "initial projection reads the authored translate"
        );

        // Author a new translate; the sink records it; sync reprojects.
        live.stage
            .attribute("/Foo.xformOp:translate")
            .set(Value::Vec3d(openusd::gf::Vec3d::from([2.0, 5.0, 0.0])))
            .unwrap();
        assert!(live.has_changes(), "the edit fired the sink");
        apply_changes(&mut world, &live, &mut map);

        assert_eq!(
            world.get::<Transform>(foo).unwrap().translation,
            Vec3::new(2.0, 5.0, 0.0),
            "sync reprojected the edited transform onto the entity"
        );
    }

    /// Namespace edits reconcile the entity set: a new prim spawns an
    /// entity, a removed prim despawns it.
    #[test]
    fn resync_spawns_and_despawns_entities() {
        let stage = Stage::builder().in_memory("rs.usda").unwrap();
        stage.define_prim("/World").unwrap();
        let live = LiveStage::new(stage);
        let mut world = World::new();
        let mut map = PrimEntities::default();
        project_stage(&mut world, &live, &mut map);
        let base = map.len();

        live.stage.define_prim("/World/NewChild").unwrap();
        apply_changes(&mut world, &live, &mut map);
        let child = map.entity("/World/NewChild").expect("new prim projected");
        assert_eq!(map.len(), base + 1);
        assert!(world.get_entity(child).is_ok(), "child entity exists");

        live.stage.remove_prim("/World/NewChild").unwrap();
        apply_changes(&mut world, &live, &mut map);
        assert!(map.entity("/World/NewChild").is_none(), "removed prim despawned");
        assert_eq!(map.len(), base);
        assert!(world.get_entity(child).is_err(), "child entity despawned");
    }

    /// The write path: authoring an entity transform back onto the prim
    /// round-trips through `read_transform`, and fires the sink.
    #[test]
    fn author_transform_roundtrips_and_notifies() {
        let stage = Stage::builder().in_memory("auth.usda").unwrap();
        stage.define_prim("/Foo").unwrap().set_type_name("Xform").unwrap();
        let live = LiveStage::new(stage);

        let t = Transform::from_xyz(3.0, 4.0, 5.0).with_scale(Vec3::splat(2.0));
        author_transform(&live.stage, "/Foo", &t).unwrap();
        assert!(live.has_changes(), "authoring fires the sink");

        let read = read_transform(&live.stage, &openusd::sdf::path("/Foo").unwrap())
            .unwrap()
            .expect("transform authored");
        let back = to_bevy_transform(read);
        assert!(
            (back.translation - Vec3::new(3.0, 4.0, 5.0)).length() < 1e-4,
            "translation round-trips, got {:?}",
            back.translation
        );
        assert!(
            (back.scale - Vec3::splat(2.0)).length() < 1e-4,
            "scale round-trips, got {:?}",
            back.scale
        );
    }

    #[test]
    fn prim_entities_bimap_subtree() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let c = world.spawn_empty().id();
        let mut map = PrimEntities::default();
        map.insert("/World", a);
        map.insert("/World/Mesh", b);
        map.insert("/Other", c);

        assert_eq!(map.entity("/World/Mesh"), Some(b));
        assert_eq!(map.path(a), Some("/World"));

        let sub: Vec<String> = map.subtree("/World").into_iter().map(|(p, _)| p).collect();
        assert_eq!(sub.len(), 2, "subtree of /World = {{/World, /World/Mesh}}");
        assert!(sub.iter().all(|p| p.starts_with("/World")));

        assert_eq!(map.remove_path("/World/Mesh"), Some(b));
        assert_eq!(map.entity("/World/Mesh"), None);
        assert_eq!(map.path(b), None);
    }
}
