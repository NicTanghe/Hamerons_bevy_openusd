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
