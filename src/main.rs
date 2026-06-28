//! `usdview` — mara UI host embedding a Bevy viewport for the live USD editor.
//!
//! mara (egui/eframe) owns the window, ribbons, and panes; the USD scene
//! renders in an embedded Bevy viewport (`MaraBevyViewport`) offscreen into a
//! mara-provided render target. The Bevy side runs `UsdPlugin` +
//! `LiveStagePlugin` (project / sync / author / undo / persistence) and uses
//! mara's camera rig + ground grid.

use bevy::camera::RenderTarget;
use bevy::prelude::*;

use mara::host::MaraHostCtx;
use mara::ui::mara_core::{style, RibbonAvoidance, WorkspaceStack};
use mara::ui::modules::bevy as mara_bevy;
use mara::window::{CreationContext, WindowApp};

use openusd::usd::Stage;
use usd_bevy::live::{LiveStage, LiveStagePlugin};
use usd_bevy::UsdPlugin;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    mara::window::run::<UsdApp>()
}

/// The mara window app: a Bevy viewport showing the live USD scene.
struct UsdApp {
    bevy_view: mara_bevy::MaraBevyViewport,
    workspace: WorkspaceStack,
}

impl WindowApp for UsdApp {
    fn new(ctx: CreationContext<'_>) -> Self {
        let path = std::env::args().nth(1);
        Self {
            bevy_view: mara_bevy::MaraBevyViewport::with_render_state_and_content(
                ctx.render_state,
                move |app: &mut App| configure_usd_app(app, path.clone()),
            ),
            workspace: WorkspaceStack::new("usd-workspace"),
        }
    }

    fn update(&mut self, host: &mut MaraHostCtx<'_>) {
        let accent = style::active_accent();
        let mut ctx = host.view_ctx(&mut self.workspace, accent, RibbonAvoidance::all());
        self.bevy_view.show(&mut ctx, host.render_state(), accent);
    }
}

/// The USD file path argument, handed to the embedded app as a resource so a
/// startup system can open it on the Bevy side.
#[derive(Resource, Clone)]
struct UsdArg(Option<String>);

fn configure_usd_app(app: &mut App, path: Option<String>) {
    app.add_plugins((UsdPlugin, LiveStagePlugin, mara_bevy::GroundGridPlugin))
        .init_resource::<mara_bevy::BevyViewportInput>()
        .insert_resource(mara_bevy::GroundGrid {
            visible: true,
            color: Color::srgba(0.30, 0.38, 0.50, 0.42),
        })
        .insert_resource(ClearColor(Color::srgb_u8(12, 14, 18)))
        .insert_resource(UsdArg(path))
        .add_systems(
            Startup,
            setup_camera.after(mara_bevy::BevyViewportSet::SetupTarget),
        )
        .add_systems(Startup, open_usd)
        .add_systems(Update, mara_bevy::apply_viewport_camera_input_system);
}

fn setup_camera(
    mut commands: Commands,
    render_target: Option<Res<mara_bevy::BevyViewportRenderTarget>>,
) {
    let chase = mara_bevy::ChaseCamera::default();
    let mut transform = Transform::default();
    mara_bevy::apply_rig(&chase, &mut transform);
    let mut camera = commands.spawn((
        Camera3d::default(),
        transform,
        AmbientLight {
            brightness: 220.0,
            ..default()
        },
        chase,
    ));
    if let Some(render_target) = render_target {
        camera.insert(RenderTarget::from(render_target.0.clone()));
    }
    commands.spawn((
        DirectionalLight {
            illuminance: 9_000.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(4.0, 10.0, 6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// Exclusive startup system: open the USD file and install the live stage
/// (a non-send resource — the openusd `Stage` is `!Send`). `LiveStagePlugin`
/// projects it on the next frame.
fn open_usd(world: &mut World) {
    let path = world.resource::<UsdArg>().0.clone();
    let Some(path) = path else {
        info!("usage: usdview <file.usd|usda|usdz>");
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
