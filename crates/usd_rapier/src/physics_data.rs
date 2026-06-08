//! Physics decode interface between `usd_bevy` (which reads the stage into
//! these records) and the rapier builders below. These structs used to live
//! in openusd's `physics` module; openusd #103 reorganized physics into typed
//! views, so the interchange types now live here in the consumer crate.

pub use openusd::schemas::physics::{CollisionApprox, Dof, DriveType};

/// USD physics joint kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JointKind {
    Fixed,
    Revolute,
    Prismatic,
    Spherical,
    Distance,
    Generic,
}

/// One authored `PhysicsLimitAPI` entry on a joint.
#[derive(Debug, Clone, Copy)]
pub struct ReadLimit {
    pub dof: Dof,
    pub low: Option<f32>,
    pub high: Option<f32>,
}

/// One authored `PhysicsDriveAPI` entry on a joint.
#[derive(Debug, Clone, Copy)]
pub struct ReadDrive {
    pub dof: Dof,
    pub drive_type: DriveType,
    pub target_position: Option<f32>,
    pub target_velocity: Option<f32>,
    pub stiffness: f32,
    pub damping: f32,
    pub max_force: Option<f32>,
}

/// A decoded physics joint, ready for the rapier builders.
#[derive(Debug, Clone)]
pub struct ReadJoint {
    pub path: String,
    pub kind: JointKind,
    pub body0: Option<String>,
    pub body1: Option<String>,
    pub local_pos0: [f32; 3],
    pub local_rot0: [f32; 4],
    pub local_pos1: [f32; 3],
    pub local_rot1: [f32; 4],
    pub axis: Option<String>,
    pub lower_limit: Option<f32>,
    pub upper_limit: Option<f32>,
    pub collision_enabled: bool,
    pub joint_enabled: bool,
    pub exclude_from_articulation: bool,
    pub break_force: Option<f32>,
    pub break_torque: Option<f32>,
    pub min_distance: Option<f32>,
    pub max_distance: Option<f32>,
    pub cone_angle_0: Option<f32>,
    pub cone_angle_1: Option<f32>,
    pub limits: Vec<ReadLimit>,
    pub drives: Vec<ReadDrive>,
}
