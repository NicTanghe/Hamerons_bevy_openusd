//! `usdview` — minimal Bevy 0.19 host for the live USD editor (RETHINK P1).
//!
//! Opens a USD file as a live `Stage`, projects it into entities and keeps
//! them in sync via `usd_bevy::live::LiveStagePlugin`, and renders with a
//! camera, a directional light, and a gizmo-drawn ground grid.
//!
//! This is the temporary mara-less shell: the panel UI + transform-gizmo
//! editing are re-added once `mara` is on Bevy 0.19. The live editor model
//! (project / sync / author / undo / persistence) already lives in
//! `usd_bevy` and is driven here.

use bevy::prelude::*;

use openusd::usd::Stage;
use usd_bevy::live::{LiveStage, LiveStagePlugin};
use usd_bevy::UsdPlugin;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "usdview — live USD editor".into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins((UsdPlugin, LiveStagePlugin))
        .insert_resource(UsdArg(std::env::args().nth(1)))
        .add_systems(Startup, setup)
        .add_systems(Startup, open_usd)
        .add_systems(Update, draw_grid)
        .run();
}

/// The USD file path from `argv[1]` (if any).
#[derive(Resource)]
struct UsdArg(Option<String>);

fn setup(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(6.0, 6.0, 12.0).looking_at(Vec3::ZERO, Vec3::Y),
        // AmbientLight is a per-view component in 0.19.
        AmbientLight {
            brightness: 250.0,
            ..default()
        },
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 8_000.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(4.0, 10.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// Exclusive startup system: open the USD file and install it as the live
/// stage (a non-send resource — the openusd `Stage` is `!Send`).
/// `LiveStagePlugin` projects it on the next frame.
fn open_usd(world: &mut World) {
    let path = world.resource::<UsdArg>().0.clone();
    let Some(path) = path else {
        info!("usage: usdview <file.usd|usda|usdz>  (no file given — empty scene)");
        return;
    };
    match Stage::open(&path) {
        Ok(stage) => {
            info!("opened USD stage: {path}");
            world.insert_non_send(LiveStage::new(stage));
        }
        Err(e) => error!("failed to open {path}: {e:#}"),
    }
}

/// A simple gizmo-drawn ground grid (replaces the first-party InfiniteGrid
/// for now to avoid the dev_tools dependency).
fn draw_grid(mut gizmos: Gizmos) {
    gizmos.grid(
        Isometry3d::from_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
        UVec2::splat(20),
        Vec2::splat(1.0),
        Color::srgb(0.25, 0.25, 0.28),
    );
}
