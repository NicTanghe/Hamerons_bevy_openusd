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
use mara_core::container::SeparatorStyle;
use mara_core::pane::{PaneAnchor, PaneBody, RailZone};
use mara_core::pod::Pod;
use mara_core::ribbon::RibbonAction;
use mara_core::style::{AccentColor, GlassOpacity, Mode, active_accent};
use mara_core::vocab::{Color32 as MaraColor32, Id as MaraId};
use mara_core::widget::{TreeBody, TreeIconKind, TreeIconSlot};
use mara_core::{RibbonAvoidance, WorkspaceStack};

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use openusd::usd::Stage;
use usd_bevy::UsdPlugin;
use usd_bevy::live::{LiveStage, LiveStagePlugin, PrimEntities};

/// Everything (trace + panics + backtraces) is mirrored here so a hard crash
/// is still recoverable after the window dies.
const LOG_FILE: &str = "/tmp/usdview.log";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    install_panic_logger();
    tracing::info!(target: "usdview", "usdview starting — full log at {LOG_FILE}");
    mara::window::run::<UsdApp>()
}

/// Tracing to BOTH stderr and [`LOG_FILE`]. The embedded Bevy app has no
/// `LogPlugin`, so without this the logs go nowhere; the file copy survives a
/// crash that eats stderr. Override the filter with `RUST_LOG`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::writer::MakeWriterExt;
    let _ = std::fs::write(LOG_FILE, ""); // truncate per run
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,usdview=trace,usd_bevy=trace,openusd=info"));
    let to_file = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_FILE)
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap())
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr.and(to_file))
        .try_init();
}

/// Capture panics (message + full backtrace) to the log file and stderr —
/// otherwise a panic inside the embedded app vanishes with the window.
fn install_panic_logger() {
    unsafe {
        if std::env::var_os("RUST_BACKTRACE").is_none() {
            std::env::set_var("RUST_BACKTRACE", "full");
        }
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let msg = format!("\n==== usdview PANIC ====\n{info}\n{backtrace}\n");
        tracing::error!(target: "usdview", "PANIC: {info}");
        eprint!("{msg}");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_FILE)
        {
            let _ = f.write_all(msg.as_bytes());
        }
        prev(info);
    }));
}

/// Open a stage + collect its prims, returning a human-readable status line
/// (logged at info/error too).
fn open_stage(path: &str) -> (Option<Stage>, Vec<PrimRow>, String) {
    match Stage::open(path) {
        Ok(stage) => {
            let prims = collect_prims(&stage);
            let status = format!("loaded {} prims — {path}", prims.len());
            tracing::info!(target: "usdview", "{status}");
            (Some(stage), prims, status)
        }
        Err(e) => {
            let status = format!("FAILED to open {path}: {e:#}");
            tracing::error!(target: "usdview", "{status}");
            (None, Vec::new(), status)
        }
    }
}

// ─── Ribbon / pane ids ──────────────────────────────────────────────

const RIBBON_LEFT: &str = "usd_ribbon_left";
const PANE_OUTLINER: &str = "usd_pane_outliner";
const PANE_PROPERTIES: &str = "usd_pane_properties";
const ACTION_SAVE: &str = "usd_action_save";
const ACTION_OPEN: &str = "usd_action_open";

/// Shared path slot: the egui Open action pushes a file here; a Bevy system
/// (`poll_reload`) picks it up and swaps the live stage.
type LoadSlot = Arc<Mutex<Option<String>>>;

fn ribbon_action(id: &'static str) -> RibbonAction {
    RibbonAction::Command(MaraId::new(id))
}

// ─── App ────────────────────────────────────────────────────────────

struct PrimRow {
    path: String,
    name: String,
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
    /// Last load result, shown in the Outliner.
    status: String,
    /// A file path chosen this frame, applied at the top of the next.
    pending_open: Option<String>,
    /// Shared with the embedded Bevy app so it reloads the viewport.
    load_queue: LoadSlot,
}

impl WindowApp for UsdApp {
    fn new(ctx: CreationContext<'_>) -> Self {
        // Initial file: `USD_FILE` env var, else argv[1], else none.
        let path = std::env::var("USD_FILE")
            .ok()
            .or_else(|| std::env::args().nth(1));
        let viewport_path = path.clone();
        let load_queue: LoadSlot = Arc::new(Mutex::new(None));
        let queue = load_queue.clone();
        let bevy_view = mara_bevy::MaraBevyViewport::with_render_state_and_content(
            ctx.__internal_render_state(),
            move |app: &mut App| configure_usd_app(app, viewport_path.clone(), queue.clone()),
        );

        let (stage, prims, status) = match path.as_deref() {
            Some(p) => open_stage(p),
            None => (None, Vec::new(), "no file — use Open USD…".to_string()),
        };

        Self {
            bevy_view,
            workspace: WorkspaceStack::new("usd-workspace"),
            stage,
            prims,
            selected: None,
            status,
            pending_open: None,
            load_queue,
        }
    }

    fn update(&mut self, host: &mut MaraHostCtx<'_>) {
        // Apply a file-open chosen last frame: reload the read-side stage for
        // the panes, and signal the embedded viewport to reload too.
        if let Some(path) = self.pending_open.take() {
            let (stage, prims, status) = open_stage(&path);
            self.stage = stage;
            self.prims = prims;
            self.status = status;
            self.selected = None;
            *self.load_queue.lock().unwrap() = Some(path);
        }

        let Self {
            bevy_view,
            workspace,
            stage,
            prims,
            selected,
            status,
            ..
        } = self;
        // Apply the mara theme every frame (without this the panes/ribbons
        // render with raw-egui defaults).
        mara_core::style::set_theme(mara_core::style::theme_pro(Mode::Dark));
        host.apply_theme(AccentColor::default(), GlassOpacity::default());
        let accent = active_accent();

        // Viewport (root, behind the ribbon-avoiding panes).
        {
            let mut vctx = host.view_ctx(workspace, accent, RibbonAvoidance::all());
            bevy_view.show(&mut vctx, host.gpu(), accent);
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
                    outliner_pane(body, prims, &mut **selected, status.as_str(), accent);
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
                ACTION_OPEN,
                "folder",
                "Open USD…",
                ribbon_action(ACTION_OPEN),
            )
            .action(
                ACTION_SAVE,
                "document",
                "Save stage",
                ribbon_action(ACTION_SAVE),
            );
        let mut picked: Option<String> = None;
        for click in host.show_ribbon_rail(rail, accent) {
            if click.action == ribbon_action(ACTION_SAVE) {
                if let Some(stage) = stage.as_ref() {
                    match usd_bevy::authoring::save_stage_as(stage, "usdview_out.usda") {
                        Ok(()) => info!("saved stage to usdview_out.usda"),
                        Err(e) => error!("save failed: {e:#}"),
                    }
                }
            } else if click.action == ribbon_action(ACTION_OPEN) {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("USD", &["usd", "usda", "usdc", "usdz"])
                    .pick_file()
                {
                    picked = Some(file.to_string_lossy().into_owned());
                }
            }
        }
        // Release the borrow of `self.selected` before touching `self` again.
        drop(selected);
        if let Some(p) = picked {
            self.pending_open = Some(p);
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
            out.push(PrimRow {
                path: s.to_string(),
                name,
            });
        },
    );
    out
}

/// A node in the prim hierarchy (built from the flat traversal list).
struct UsdNode {
    path: String,
    name: String,
    children: Vec<usize>,
}

/// Build the prim hierarchy + the root indices from the flat, depth-ordered
/// prim list. A prim's parent is the path up to its last `/`.
fn build_usd_tree(prims: &[PrimRow]) -> (Vec<UsdNode>, Vec<usize>) {
    let mut nodes: Vec<UsdNode> = prims
        .iter()
        .map(|p| UsdNode {
            path: p.path.clone(),
            name: p.name.clone(),
            children: Vec::new(),
        })
        .collect();
    let index: std::collections::HashMap<&str, usize> = prims
        .iter()
        .enumerate()
        .map(|(i, p)| (p.path.as_str(), i))
        .collect();
    let mut roots = Vec::new();
    for (i, p) in prims.iter().enumerate() {
        let parent = &p.path[..p.path.rfind('/').unwrap_or(0)];
        match (!parent.is_empty()).then(|| index.get(parent)).flatten() {
            Some(&pi) => nodes[pi].children.push(i),
            None => roots.push(i),
        }
    }
    (nodes, roots)
}

fn outliner_pane(
    body: &mut PaneBody,
    prims: &[PrimRow],
    selected: &mut Option<String>,
    status: &str,
    accent: MaraColor32,
) {
    // Load status (shows failures like "FAILED to open … unsupported .usd").
    body.add_normal(
        "usd.status",
        "Status",
        "list",
        vec![Pod::new(MaraId::new(("usd.outliner", "status"))).with_readout("file", status)],
    );

    let tree_root = MaraId::new(("usd.outliner", "tree_root"));
    let sel_key = tree_root.with("selected");
    // Selection from last frame (the tree writes it during render).
    let sel = body.temp_string(sel_key).unwrap_or_default();
    *selected = (!sel.is_empty()).then(|| sel.clone());

    let search_id = MaraId::new(("usd.outliner", "scene", 0usize));
    let filter = body.search_query(search_id, 0).to_lowercase();
    let (nodes, roots) = build_usd_tree(prims);

    body.add_normal(
        "usd.outliner",
        "Scene",
        "folder",
        vec![
            Pod::new(search_id)
                .with_separator(SeparatorStyle::Line)
                .with_search("filter by name / path…", accent),
            Pod::new(MaraId::new(("usd.outliner", "scene", 1usize)))
                .with_separator(SeparatorStyle::Line)
                .fill()
                .with_tree(7, move |tree| {
                    usd_tree(tree, tree_root, accent, &filter, &nodes, &roots)
                }),
            Pod::new(MaraId::new(("usd.outliner", "scene", 2usize))).with_readout(
                "selected",
                if sel.is_empty() {
                    "—".to_string()
                } else {
                    sel
                },
            ),
        ],
    );
}

fn usd_tree(
    tree: &mut TreeBody,
    root_id: MaraId,
    accent: MaraColor32,
    filter: &str,
    nodes: &[UsdNode],
    roots: &[usize],
) {
    let sel_key = root_id.with("selected");
    let mut selected = tree.temp_string(sel_key).unwrap_or_default();
    let mut clicked: Option<String> = None;
    for &r in roots {
        walk_usd_tree(
            tree,
            root_id,
            nodes,
            r,
            0,
            &selected,
            accent,
            filter,
            &mut clicked,
        );
    }
    if let Some(p) = clicked {
        selected = p;
        tree.set_temp_string(sel_key, selected);
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_usd_tree(
    tree: &mut TreeBody,
    root_id: MaraId,
    nodes: &[UsdNode],
    i: usize,
    depth: u32,
    selected: &str,
    accent: MaraColor32,
    filter: &str,
    clicked: &mut Option<String>,
) {
    if !usd_tree_passes(nodes, i, filter) {
        return;
    }
    let node = &nodes[i];
    let is_branch = !node.children.is_empty();
    let exp_key = root_id.with(("exp", node.path.as_str()));
    let eye_key = root_id.with(("eye", node.path.as_str()));
    let mut expanded = tree.persisted_bool(exp_key).unwrap_or(true);
    let mut eye_on = tree.persisted_bool(eye_key).unwrap_or(true);
    let mut slots =
        [TreeIconSlot::new(TreeIconKind::Eye, &mut eye_on).with_tooltip("Toggle visibility")];
    let resp = tree.row(
        i,
        depth,
        if is_branch { Some(&mut expanded) } else { None },
        Some("cube"),
        &node.name,
        selected == node.path,
        accent,
        &mut slots,
    );
    if resp.body.clicked {
        *clicked = Some(node.path.clone());
    }
    tree.set_persisted_bool(exp_key, expanded);
    tree.set_persisted_bool(eye_key, eye_on);
    if is_branch && expanded {
        for &c in &node.children {
            walk_usd_tree(
                tree,
                root_id,
                nodes,
                c,
                depth + 1,
                selected,
                accent,
                filter,
                clicked,
            );
        }
    }
}

/// A node passes when it (or any descendant) matches the lowercase `filter`.
fn usd_tree_passes(nodes: &[UsdNode], i: usize, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let node = &nodes[i];
    if node.name.to_lowercase().contains(filter) || node.path.to_lowercase().contains(filter) {
        return true;
    }
    node.children
        .iter()
        .any(|&c| usd_tree_passes(nodes, c, filter))
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

/// The shared load slot, as a Bevy resource the reload system reads.
#[derive(Resource, Clone)]
struct LoadQueue(LoadSlot);

fn configure_usd_app(app: &mut App, path: Option<String>, load_queue: LoadSlot) {
    // The mara viewport already adds GroundGridPlugin + the core/render
    // plugins; we add only our own and configure the grid resource.
    app.add_plugins((UsdPlugin, LiveStagePlugin))
        .init_resource::<mara_bevy::BevyViewportInput>()
        .insert_resource(mara_bevy::GroundGrid {
            visible: true,
            color: grid_color(1.0),
        })
        .insert_resource(ClearColor(Color::srgb_u8(12, 14, 18)))
        .insert_resource(UsdArg(path))
        .insert_resource(LoadQueue(load_queue))
        .add_systems(
            Startup,
            setup_camera.after(mara_bevy::BevyViewportSet::SetupTarget),
        )
        .add_systems(Startup, open_usd)
        .add_systems(Update, mara_bevy::apply_viewport_camera_input_system)
        .add_systems(
            Update,
            draw_large_editor_grid_system.after(mara_bevy::apply_viewport_camera_input_system),
        )
        .add_systems(Update, poll_reload);
}

/// Extend Mara's small close-range gizmo grid with a large camera-following
/// editor grid. It keeps a stable world spacing and fades at the far edge so
/// lines do not pop/flicker as the camera moves.
fn draw_large_editor_grid_system(
    grid: Option<Res<mara_bevy::GroundGrid>>,
    cameras: Query<&mara_bevy::ChaseCamera>,
    mut gizmos: Gizmos,
) {
    let Some(grid) = grid else {
        return;
    };
    if !grid.visible {
        return;
    }
    let Some(camera) = cameras.iter().next() else {
        return;
    };

    const STEP: f32 = 32.0;
    const MIN_HALF_EXTENT: f32 = 16_384.0;
    const MAX_HALF_LINES: i32 = 1024;
    // Mara's built-in grid covers roughly ±32 units at 1-unit spacing. Do not
    // draw the coarse extension over that same center area, or coincident
    // gizmo lines fight and shimmer.
    const NEAR_SKIP_EXTENT: f32 = 34.0;
    let focus = camera.focus;
    let step = STEP;
    let requested_half_extent = (camera.distance * 12.0).max(MIN_HALF_EXTENT);
    let half_lines = ((requested_half_extent / step).ceil() as i32).clamp(512, MAX_HALF_LINES);
    let half_extent = half_lines as f32 * step;
    let fade_start = half_extent * 0.55;
    let center_x = (focus.x / step).round() * step;
    let center_z = (focus.z / step).round() * step;
    let near_center_x = focus.x.round();
    let near_center_z = focus.z.round();
    let near_min_x = near_center_x - NEAR_SKIP_EXTENT;
    let near_max_x = near_center_x + NEAR_SKIP_EXTENT;
    let near_min_z = near_center_z - NEAR_SKIP_EXTENT;
    let near_max_z = near_center_z + NEAR_SKIP_EXTENT;
    let min_x = center_x - half_extent;
    let max_x = center_x + half_extent;
    let min_z = center_z - half_extent;
    let max_z = center_z + half_extent;

    for i in -half_lines..=half_lines {
        let x = center_x + i as f32 * step;
        let z = center_z + i as f32 * step;
        let x_alpha = far_grid_alpha((x - center_x).abs(), fade_start, half_extent);
        let z_alpha = far_grid_alpha((z - center_z).abs(), fade_start, half_extent);

        if x >= near_min_x && x <= near_max_x {
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(x, 0.0, min_z),
                Vec3::new(x, 0.0, near_min_z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                x_alpha,
            );
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(x, 0.0, near_max_z),
                Vec3::new(x, 0.0, max_z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                x_alpha,
            );
        } else {
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(x, 0.0, min_z),
                Vec3::new(x, 0.0, center_z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                x_alpha,
            );
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(x, 0.0, center_z),
                Vec3::new(x, 0.0, max_z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                x_alpha,
            );
        }

        if z >= near_min_z && z <= near_max_z {
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(min_x, 0.0, z),
                Vec3::new(near_min_x, 0.0, z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                z_alpha,
            );
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(near_max_x, 0.0, z),
                Vec3::new(max_x, 0.0, z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                z_alpha,
            );
        } else {
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(min_x, 0.0, z),
                Vec3::new(center_x, 0.0, z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                z_alpha,
            );
            draw_fading_grid_segment(
                &mut gizmos,
                Vec3::new(center_x, 0.0, z),
                Vec3::new(max_x, 0.0, z),
                center_x,
                center_z,
                fade_start,
                half_extent,
                z_alpha,
            );
        }
    }
}

fn draw_fading_grid_segment(
    gizmos: &mut Gizmos,
    start: Vec3,
    end: Vec3,
    center_x: f32,
    center_z: f32,
    fade_start: f32,
    fade_end: f32,
    line_alpha: f32,
) {
    if start.distance_squared(end) > 0.01 {
        let start_alpha = line_alpha
            * far_grid_alpha(
                (start.x - center_x).abs().max((start.z - center_z).abs()),
                fade_start,
                fade_end,
            );
        let end_alpha = line_alpha
            * far_grid_alpha(
                (end.x - center_x).abs().max((end.z - center_z).abs()),
                fade_start,
                fade_end,
            );
        if start_alpha > 0.01 || end_alpha > 0.01 {
            gizmos.line_gradient(start, end, grid_color(start_alpha), grid_color(end_alpha));
        }
    }
}

fn far_grid_alpha(distance: f32, fade_start: f32, fade_end: f32) -> f32 {
    1.0 - smoothstep01((distance - fade_start) / (fade_end - fade_start).max(1.0))
}

fn smoothstep01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn grid_color(alpha: f32) -> Color {
    Color::srgba(0.44, 0.44, 0.46, 0.35 * alpha.clamp(0.0, 1.0))
}

/// Pick up a path pushed by the egui "Open" action, despawn the current scene,
/// and install a fresh `LiveStage` (which `LiveStagePlugin` then reprojects).
fn poll_reload(world: &mut World) {
    let path = world
        .resource::<LoadQueue>()
        .0
        .lock()
        .ok()
        .and_then(|mut slot| slot.take());
    let Some(path) = path else {
        return;
    };

    world.remove_non_send::<LiveStage>();
    let entities: Vec<Entity> = world
        .resource::<PrimEntities>()
        .iter()
        .map(|(_, e)| e)
        .collect();
    for entity in entities {
        world.despawn(entity);
    }
    *world.resource_mut::<PrimEntities>() = PrimEntities::default();

    match Stage::open(&path) {
        Ok(stage) => {
            info!("reloaded USD stage: {path}");
            world.insert_non_send(LiveStage::new(stage));
        }
        Err(e) => error!("failed to reload {path}: {e:#}"),
    }
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
