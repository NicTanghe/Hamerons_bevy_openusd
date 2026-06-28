//! `usdview` — mara UI host (ribbons + panes) embedding a Bevy USD viewport.
//!
//! mara (egui/eframe) owns the window + ribbons + panes; the USD scene renders
//! in an embedded Bevy viewport. A left ribbon toggles an **Outliner** (prim
//! tree) and a **Properties** pane; a Save action writes the stage back.

use bevy::camera::RenderTarget;
use bevy::prelude::*;

use mara::host::{MaraHostCtx, RibbonRail};
use mara::ui::mara_core;
use mara::ui::modules::bevy as mara_bevy;
use mara::window::{CreationContext, WindowApp};
use mara_core::pane::{PaneAnchor, PaneBody, RailZone};
use mara_core::pod::Pod;
use mara_core::ribbon::RibbonAction;
use mara_core::style::active_accent;
use mara_core::vocab::{Color32 as MaraColor32, Id as MaraId};
use mara_core::{RibbonAvoidance, WorkspaceStack};

use std::cell::RefCell;

use openusd::usd::Stage;
use usd_bevy::UsdPlugin;
use usd_bevy::live::{LiveStage, LiveStagePlugin};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    mara::window::run::<UsdApp>()
}

// ─── Ribbon / pane ids ──────────────────────────────────────────────

const RIBBON_LEFT: &str = "usd_ribbon_left";
const PANE_OUTLINER: &str = "usd_pane_outliner";
const PANE_PROPERTIES: &str = "usd_pane_properties";
const ACTION_SAVE: &str = "usd_action_save";

fn ribbon_action(id: &'static str) -> RibbonAction {
    RibbonAction::Command(MaraId::new(id))
}

// ─── App ────────────────────────────────────────────────────────────

struct PrimRow {
    path: String,
    name: String,
    depth: u32,
}

/// The mara window app.
struct UsdApp {
    bevy_view: mara_bevy::MaraBevyViewport,
    workspace: WorkspaceStack,
    /// A read-side stage for the outliner/properties panes (the embedded
    /// viewport renders its own live copy).
    stage: Option<Stage>,
    prims: Vec<PrimRow>,
    selected: Option<String>,
}

impl WindowApp for UsdApp {
    fn new(ctx: CreationContext<'_>) -> Self {
        let path = std::env::args().nth(1);
        let viewport_path = path.clone();
        let bevy_view = mara_bevy::MaraBevyViewport::with_render_state_and_content(
            ctx.render_state,
            move |app: &mut App| configure_usd_app(app, viewport_path.clone()),
        );

        let stage = path.as_deref().and_then(|p| Stage::open(p).ok());
        let prims = stage.as_ref().map(collect_prims).unwrap_or_default();

        Self {
            bevy_view,
            workspace: WorkspaceStack::new("usd-workspace"),
            stage,
            prims,
            selected: None,
        }
    }

    fn update(&mut self, host: &mut MaraHostCtx<'_>) {
        let Self {
            bevy_view,
            workspace,
            stage,
            prims,
            selected,
        } = self;
        let accent = active_accent();

        // Viewport (root, behind the ribbon-avoiding panes).
        {
            let mut vctx = host.view_ctx(workspace, accent, RibbonAvoidance::all());
            bevy_view.show(&mut vctx, host.render_state(), accent);
        }

        // Panes + ribbon rail. Mara owns the pane/ribbon wiring,
        // open-state, pane-id publication, and paint ordering.
        let selected = RefCell::new(selected);
        let rail = RibbonRail::view_left(RIBBON_LEFT, "usdview.ribbons")
            .default_open(PANE_OUTLINER)
            .pane(
                PANE_OUTLINER,
                "list",
                "Outliner",
                PaneAnchor::LeftRail(RailZone::Start),
                |body| {
                    let mut selected = selected.borrow_mut();
                    outliner_pane(body, prims, &mut **selected, accent);
                },
            )
            .pane(
                PANE_PROPERTIES,
                "options",
                "Properties",
                PaneAnchor::LeftRail(RailZone::Middle),
                |body| {
                    let selected = selected.borrow();
                    properties_pane(body, stage, &**selected);
                },
            )
            .action(
                ACTION_SAVE,
                "document",
                "Save stage",
                ribbon_action(ACTION_SAVE),
            );
        for click in host.show_ribbon_rail(rail, accent) {
            if click.action == ribbon_action(ACTION_SAVE) {
                if let Some(stage) = stage.as_ref() {
                    match usd_bevy::authoring::save_stage_as(stage, "usdview_out.usda") {
                        Ok(()) => info!("saved stage to usdview_out.usda"),
                        Err(e) => error!("save failed: {e:#}"),
                    }
                }
            }
        }
    }
}

fn collect_prims(stage: &Stage) -> Vec<PrimRow> {
    let mut out = Vec::new();
    let _ = stage.traverse(
        openusd::usd::PrimPredicate::default(),
        |path: &openusd::sdf::Path| {
            let s = path.as_str();
            let name = s.rsplit('/').next().unwrap_or(s).to_string();
            let depth = (s.matches('/').count() as u32).saturating_sub(1);
            out.push(PrimRow {
                path: s.to_string(),
                name,
                depth,
            });
        },
    );
    out
}

fn outliner_pane(
    body: &mut PaneBody,
    prims: &[PrimRow],
    selected: &mut Option<String>,
    accent: MaraColor32,
) {
    let pods: Vec<Pod> = prims
        .iter()
        .map(|p| {
            let label = format!("{}{}", "    ".repeat(p.depth as usize), p.name);
            Pod::new(MaraId::new(p.path.as_str())).with_button(label, accent)
        })
        .collect();
    body.add_normal("usd.outliner", "Outliner", "list", pods);

    let rendered = body.render();
    for p in prims {
        if let Some(resps) = rendered.get(&MaraId::new(p.path.as_str())) {
            if resps.iter().any(|r| r.buttons.iter().any(|b| b.clicked)) {
                *selected = Some(p.path.clone());
            }
        }
    }
}

fn properties_pane(body: &mut PaneBody, stage: &Option<Stage>, selected: &Option<String>) {
    let pods = match (stage, selected) {
        (Some(stage), Some(path)) => {
            let ty = openusd::sdf::path(path)
                .ok()
                .and_then(|p| stage.prim(p).type_name().ok().flatten())
                .map(|t| t.as_str().to_string())
                .unwrap_or_else(|| "—".to_string());
            vec![
                Pod::new("usd.prop.path").with_readout("Path", path.clone()),
                Pod::new("usd.prop.type").with_readout("Type", ty),
            ]
        }
        _ => vec![Pod::new("usd.prop.none").with_readout("Selection", "none")],
    };
    body.add_normal("usd.properties", "Properties", "options", pods);
}

// ─── Embedded Bevy viewport (the USD scene) ─────────────────────────

#[derive(Resource, Clone)]
struct UsdArg(Option<String>);

fn configure_usd_app(app: &mut App, path: Option<String>) {
    // The mara viewport already adds GroundGridPlugin + the core/render
    // plugins; we add only our own and configure the grid resource.
    app.add_plugins((UsdPlugin, LiveStagePlugin))
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
