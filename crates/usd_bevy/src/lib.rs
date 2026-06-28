//! `usd_bevy` — OpenUSD → Bevy as a **live editor** (RETHINK).
//!
//! The composed USD stage is the source of truth, projected into Bevy
//! entities and kept in sync off openusd's `StageSink` (`live`), edited via
//! `authoring`, read through `read`.
//!
//! **Bevy 0.19 port note:** the legacy one-shot `Scene`-baking asset loader
//! (`asset` + `build` + their component builders) is gated off below — Bevy
//! 0.19 removed the `bevy::scene::Scene` World-container asset and the
//! `SceneRoot`/`DynamicScene::from_world` it relied on. That projection is
//! being ported into `live` (direct main-world spawning). Re-enable each
//! module as it is ported.

pub mod authoring;
pub mod live;
pub mod markers;
pub mod prim_ref;
pub mod read;

// ── Legacy Scene-baking projection — gated for the 0.19 port ──────────
// Removed-in-0.19 deps: `bevy::scene::Scene` (now a BSN trait), `SceneRoot`,
// `DynamicScene::from_world`. Being ported into `live`.
// pub mod anim;
// mod asset;
// mod build;
// pub mod curves;
// pub mod incremental;
// mod light;
// mod material;
// pub mod mesh;
// pub mod nurbs_patch;
// pub mod physics;
// pub(crate) mod physics_attach;
// pub mod skel_anim;
// pub mod tetmesh;
// mod texture;

// Marker components + prim-ref components are the projection's public API.
pub use markers::*;
pub use prim_ref::{
    UsdCustomAttrs, UsdDisplayName, UsdKind, UsdLocalExtent, UsdPrimRef, UsdProcedural, UsdPurpose,
    UsdSpatialAudio,
};

use bevy::app::{App, Plugin};

/// Registers `UsdPrimRef` + every marker component so they reflect/serialize
/// correctly. The live editor loop itself is [`live::LiveStagePlugin`] — add
/// both to an app.
#[derive(Default)]
pub struct UsdPlugin;

impl Plugin for UsdPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<UsdPrimRef>()
            .register_type::<UsdLocalExtent>()
            .register_type::<UsdKind>()
            .register_type::<UsdPurpose>()
            .register_type::<prim_ref::UsdSkelAnimDriver>()
            .register_type::<prim_ref::UsdBlendShapeBinding>()
            .register_type::<markers::UsdPhysicsScene>()
            .register_type::<markers::UsdRigidBody>()
            .register_type::<markers::UsdMass>()
            .register_type::<markers::UsdCollider>()
            .register_type::<markers::UsdColliderShape>()
            .register_type::<markers::UsdCollisionApprox>()
            .register_type::<markers::UsdPhysicsMaterial>()
            .register_type::<markers::UsdArticulationRoot>()
            .register_type::<markers::UsdPhysicsJoint>()
            .register_type::<markers::UsdJointKind>()
            .register_type::<markers::UsdJointLimit>()
            .register_type::<markers::UsdJointDrive>()
            .register_type::<markers::UsdDof>()
            .register_type::<markers::UsdDriveType>()
            .register_type::<markers::UsdCollisionGroup>()
            .register_type::<markers::UsdCollisionFilter>();
    }
}
