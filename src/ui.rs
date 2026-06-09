//! Viewer UI — mara ribbons + rail-anchored panes + widgets.
//!
//! Left rail is one `ThreeSided` panel ribbon. Primary tools live in
//! the `Start` cluster (top), the play toggle in `Middle`, utility/help
//! in `End` (bottom). Panel visibility is driven by mara's `RibbonOpen`
//! resource — clicking a rail button toggles its pane exclusively
//! (one open pane per ribbon).
//!
//! Each pane body paints imperatively via `body.ui()` (mara's
//! immediate escape hatch) using mara_core widgets, so the panel code
//! reads like the old bevy_frost version.

use bevy::asset::Assets;
use bevy::ecs::hierarchy::Children;
use bevy::mesh::Mesh3d;
use bevy::pbr::{MeshMaterial3d, StandardMaterial};
use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, egui};
use bevy_mara::prelude::*;
use mara_core::pane::{Pane, PaneAnchor, PaneResize, RailZone};
use mara_core::ribbon::{
    RibbonAction, RibbonCluster, RibbonDrag, RibbonEdge, RibbonGlyph, RibbonMode, RibbonOpen,
    RibbonPlacement, RibbonRole, RibbonSlotClick, RibbonSlotItem, ResolvedSlotRibbon,
    draw_slot_ribbons_featureful,
};
use mara_core::style;
use mara_core::style::AccentColor;
use mara_core::widget::section;
use mara_core::widget::{
    TreeIconKind, TreeIconSlot, badge_row, chip, chip_colored, context_menu_mara,
    hybrid_select_row, keybinding_row, labelled_row, pretty_slider, readout_row, row_separator,
    search_field, sub_caption, toggle, tree_row, wide_button,
};
use mara_core::{CommandPaletteState, PaletteItem, command_palette};
use std::collections::HashMap;
use std::hash::Hash;
use std::path::PathBuf;
use usd_bevy::{UsdAsset, UsdDisplayName, UsdKind, UsdPrimRef, UsdProcedural, UsdSpatialAudio};

use crate::camera::ArcballCamera;
use crate::overlays::DisplayToggles;
use crate::state::{
    CameraBookmark, CameraBookmarks, CameraMount, FlyTo, LoadRequest, LoaderTuning,
    PendingAnimationClip, ReloadRequest, SelectedPrim, StageInfo, UsdStageTime,
};

// ─── Ribbon declaration ─────────────────────────────────────────────

pub const RIBBON_LEFT: &str = "viewer_left";

pub const RIB_SELECTION: &str = "viewer_selection";
pub const RIB_TREE: &str = "viewer_tree";
pub const RIB_INFO: &str = "viewer_info";
pub const RIB_VARIANTS: &str = "viewer_variants";
pub const RIB_CAMERAS: &str = "viewer_cameras";
pub const RIB_MATERIALS: &str = "viewer_materials";
pub const RIB_OVERLAYS: &str = "viewer_overlays";
pub const RIB_TIMELINE: &str = "viewer_timeline";
pub const RIB_KEYS: &str = "viewer_keys";
pub const RIB_LOG: &str = "viewer_log";
pub const RIB_PLAY: &str = "viewer_play";

/// A ribbon rail (mara `ResolvedSlotRibbon`s are built from this per frame).
#[derive(Clone, Copy)]
struct RibbonSpec {
    id: &'static str,
    edge: RibbonEdge,
    role: RibbonRole,
    mode: RibbonMode,
    accepts: &'static [&'static str],
}

/// A ribbon button (a pane toggle, or an `Icon`-role action).
#[derive(Clone, Copy)]
struct RibbonButtonSpec {
    id: &'static str,
    ribbon: &'static str,
    cluster: RibbonCluster,
    // Documents intended order within a cluster; mara paints in
    // declaration order, so this isn't read at runtime.
    #[allow(dead_code)]
    slot: u32,
    draggable: bool,
    glyph: RibbonGlyph,
    tooltip: &'static str,
    child_ribbon: Option<&'static str>,
    role: Option<RibbonRole>,
}

const RIBBONS: &[RibbonSpec] = &[RibbonSpec {
    id: RIBBON_LEFT,
    edge: RibbonEdge::Left,
    role: RibbonRole::Panel,
    mode: RibbonMode::ThreeSided,
    accepts: &[],
}];

const RIBBON_ITEMS: &[RibbonButtonSpec] = &[
    RibbonButtonSpec {
        id: RIB_SELECTION,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Start,
        slot: 0,
        draggable: false,
        glyph: RibbonGlyph::Icon("cursor"),
        tooltip: "File / selection",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_TREE,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Start,
        slot: 1,
        draggable: false,
        glyph: RibbonGlyph::Icon("folder"),
        tooltip: "Prim tree (T)",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_INFO,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Start,
        slot: 2,
        draggable: false,
        glyph: RibbonGlyph::Icon("document"),
        tooltip: "Stage info (I)",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_VARIANTS,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Start,
        slot: 3,
        draggable: false,
        glyph: RibbonGlyph::Icon("options"),
        tooltip: "Variants",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_CAMERAS,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Start,
        slot: 4,
        draggable: false,
        glyph: RibbonGlyph::Icon("cube"),
        tooltip: "Cameras",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_MATERIALS,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Start,
        slot: 5,
        draggable: false,
        glyph: RibbonGlyph::Icon("color"),
        tooltip: "Materials",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_PLAY,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::Middle,
        slot: 0,
        draggable: false,
        glyph: RibbonGlyph::Icon("play"),
        tooltip: "Play / pause physics",
        child_ribbon: None,
        role: Some(RibbonRole::Icon),
    },
    RibbonButtonSpec {
        id: RIB_OVERLAYS,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::End,
        slot: 0,
        draggable: false,
        glyph: RibbonGlyph::Icon("square-multiple"),
        tooltip: "Overlays (O)",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_TIMELINE,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::End,
        slot: 1,
        draggable: false,
        glyph: RibbonGlyph::Icon("clock"),
        tooltip: "Timeline",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_KEYS,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::End,
        slot: 2,
        draggable: false,
        glyph: RibbonGlyph::Icon("keyboard"),
        tooltip: "Controls (?)",
        child_ribbon: None,
        role: None,
    },
    RibbonButtonSpec {
        id: RIB_LOG,
        ribbon: RIBBON_LEFT,
        cluster: RibbonCluster::End,
        slot: 3,
        draggable: false,
        glyph: RibbonGlyph::Icon("list"),
        tooltip: "Log",
        child_ribbon: None,
        role: None,
    },
];

/// Prim-tree expansion state, keyed by `UsdPrimRef.path`. Entries
/// default to expanded the first time a row is rendered.
#[derive(Resource, Default)]
pub struct TreeExpanded(pub HashMap<String, bool>);

/// Free-text filter for the prim-tree panel. When non-empty, the
/// panel switches to a flat-list mode showing every prim whose path
/// contains the substring (case-insensitive).
#[derive(Resource, Default)]
pub struct TreeFilter(pub String);

/// Wrapper around mara's `CommandPaletteState` so Bevy can track it
/// as a Resource without needing to derive on an upstream type.
#[derive(Resource, Default)]
pub struct ViewerCommandPalette(pub CommandPaletteState);

/// The palette's static action list. Adding a new id here only
/// requires a matching arm in the dispatch below.
const PALETTE_ITEMS: &[PaletteItem] = &[
    PaletteItem {
        id: "open_selection",
        label: "Open: Selection panel",
        hint: Some("F"),
    },
    PaletteItem {
        id: "open_tree",
        label: "Open: Prim tree",
        hint: Some("T"),
    },
    PaletteItem {
        id: "open_info",
        label: "Open: Stage info",
        hint: Some("I"),
    },
    PaletteItem {
        id: "open_variants",
        label: "Open: Variants",
        hint: None,
    },
    PaletteItem {
        id: "open_cameras",
        label: "Open: Cameras",
        hint: None,
    },
    PaletteItem {
        id: "open_overlays",
        label: "Open: Overlays",
        hint: Some("O"),
    },
    PaletteItem {
        id: "open_timeline",
        label: "Open: Timeline",
        hint: None,
    },
    PaletteItem {
        id: "open_keys",
        label: "Open: Controls",
        hint: Some("?"),
    },
    PaletteItem {
        id: "open_log",
        label: "Open: Log",
        hint: None,
    },
    PaletteItem {
        id: "toggle_grid",
        label: "Toggle: Ground grid",
        hint: Some("G"),
    },
    PaletteItem {
        id: "toggle_axes",
        label: "Toggle: World axes",
        hint: Some("X"),
    },
    PaletteItem {
        id: "toggle_markers",
        label: "Toggle: Prim markers",
        hint: Some("P"),
    },
    PaletteItem {
        id: "toggle_wireframe",
        label: "Toggle: Wireframe",
        hint: None,
    },
    PaletteItem {
        id: "reload_stage",
        label: "Stage: Reload",
        hint: Some("R"),
    },
    PaletteItem {
        id: "browse_usd",
        label: "Stage: Browse for USD…",
        hint: None,
    },
];

// ─── Ribbon helpers ─────────────────────────────────────────────────

fn ribbon_action(id: &'static str) -> RibbonAction {
    RibbonAction::Command(egui::Id::new(id))
}

/// Rail-anchor for a pane, derived from its ribbon button's cluster.
/// All viewer panes live on the left rail.
fn pane_anchor_for(item_id: &'static str) -> PaneAnchor {
    let zone = RIBBON_ITEMS
        .iter()
        .find(|i| i.id == item_id)
        .map(|i| match i.cluster {
            RibbonCluster::Start => RailZone::Start,
            RibbonCluster::Middle => RailZone::Middle,
            RibbonCluster::End => RailZone::End,
        })
        .unwrap_or(RailZone::Start);
    PaneAnchor::LeftRail(zone)
}

/// Build mara `ResolvedSlotRibbon`s from the static specs and paint
/// them. Panel-role buttons toggle their pane's `RibbonOpen` state
/// internally; `Icon`-role buttons (play) surface as clicks.
fn draw_unified_ribbons(
    ctx: &egui::Context,
    accent: egui::Color32,
    ribbons: &[RibbonSpec],
    items: &[RibbonButtonSpec],
    open: &mut RibbonOpen,
    placement: &mut RibbonPlacement,
    drag: &mut RibbonDrag,
    active: impl Fn(&'static str) -> bool,
) -> Vec<RibbonSlotClick> {
    let mut resolved = Vec::new();
    for ribbon in ribbons {
        for cluster in [
            RibbonCluster::Start,
            RibbonCluster::Middle,
            RibbonCluster::End,
        ] {
            let slot_items: Vec<RibbonSlotItem> = items
                .iter()
                .filter(|item| item.ribbon == ribbon.id && item.cluster == cluster)
                .map(|item| {
                    let icon = match item.glyph {
                        RibbonGlyph::Icon(i) | RibbonGlyph::Text(i) | RibbonGlyph::Svg(i) => i,
                    };
                    let mut slot = RibbonSlotItem::featureful(
                        item.id,
                        icon,
                        item.id,
                        item.tooltip,
                        ribbon_action(item.id),
                    )
                    .with_role(item.role.unwrap_or(ribbon.role));
                    if let Some(child) = item.child_ribbon {
                        slot = slot.with_child_ribbon(child);
                    }
                    slot.draggable = item.draggable;
                    slot.active = active(item.id);
                    slot
                })
                .collect();
            if slot_items.is_empty() {
                continue;
            }
            resolved.push(ResolvedSlotRibbon {
                id: egui::Id::new((ribbon.id, cluster)),
                chrome_id: Some(ribbon.id),
                scope: mara_core::RibbonScope::Permanent,
                edge: ribbon.edge,
                role: ribbon.role,
                mode: ribbon.mode,
                cluster,
                accepts: ribbon.accepts,
                items: slot_items,
            });
        }
    }
    draw_slot_ribbons_featureful(ctx, accent, &resolved, open, placement, drag)
}

// ─── Plugin ─────────────────────────────────────────────────────────

pub struct ViewerUiPlugin;

impl Plugin for ViewerUiPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<MaraPlugin>() {
            app.add_plugins(MaraPlugin);
        }
        app.init_resource::<TreeExpanded>()
            .init_resource::<TreeFilter>()
            .init_resource::<ViewerCommandPalette>()
            .add_systems(
                EguiPrimaryContextPass,
                (
                    // Publish pane-button ids first (Pane::show needs
                    // them); panes paint next; the ribbon assembly
                    // registers last so its `Area`s layer above the
                    // panes. Click handling updates `RibbonOpen` for the
                    // next frame.
                    publish_pane_ids,
                    draw_selection_panel,
                    draw_tree_panel,
                    draw_info_panel,
                    draw_variants_panel,
                    draw_cameras_panel,
                    draw_materials_panel,
                    draw_overlays_panel,
                    draw_timeline_panel,
                    draw_keys_panel,
                    draw_log_panel,
                    draw_palette_panel,
                    draw_ribbons,
                )
                    .chain()
                    .after(RibbonGhostSet),
            );
    }
}

// ─── Ribbon rail ────────────────────────────────────────────────────

fn draw_ribbons(
    mut contexts: EguiContexts,
    accent: Res<AccentColor>,
    mut open: ResMut<RibbonOpen>,
    mut placement: ResMut<RibbonPlacement>,
    mut drag: ResMut<RibbonDrag>,
    mut physics: ResMut<usd_bevy::physics::PhysicsActive>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let physics_on = physics.0;
    let clicks = draw_unified_ribbons(
        ctx,
        accent.0,
        RIBBONS,
        RIBBON_ITEMS,
        &mut open,
        &mut placement,
        &mut drag,
        |id| id == RIB_PLAY && physics_on,
    );
    for click in clicks {
        if click.item == egui::Id::new(RIB_PLAY) {
            physics.0 = !physics.0;
        }
    }
}

/// mara's `Pane::show` requires the set of ribbon pane-button ids to be
/// published each frame (it uses them to lay out the rails). Runs before
/// any pane so the geometry is ready.
fn publish_pane_ids(mut contexts: EguiContexts) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let ids: Vec<egui::Id> = RIBBON_ITEMS
        .iter()
        .filter(|i| i.role.is_none())
        .map(|i| egui::Id::new(i.id))
        .collect();
    mara_core::pane::publish_ribbon_pane_ids(ctx, ids);
}

fn is_panel_open(open: &RibbonOpen, item: &'static str) -> bool {
    open.is_open(RIBBON_LEFT, item)
}

// ─── Selection panel ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_selection_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    info: Res<StageInfo>,
    requested: Res<crate::RequestedAsset>,
    mut load_req: ResMut<LoadRequest>,
    mut selected: ResMut<SelectedPrim>,
    prims: Query<(Entity, &Name, &UsdPrimRef)>,
    mesh_q: Query<(), With<Mesh3d>>,
    kind_q: Query<&UsdKind>,
    audio_q: Query<&UsdSpatialAudio>,
    proc_q: Query<&UsdProcedural>,
    vis_q: Query<&Visibility>,
    children: Query<&Children>,
) {
    if !is_panel_open(&open, RIB_SELECTION) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_SELECTION, "Selection", pane_anchor_for(RIB_SELECTION), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "sel_stage", "Loaded stage", accent_col, true, |ui| {
                readout_row(ui, "file", info.path.as_str());
                if wide_button(ui, "📁  Browse USD…", accent_col).clicked()
                    && let Some(picked) = rfd::FileDialog::new()
                        .add_filter("USD stages", &["usda", "usdc", "usd", "usdz"])
                        .pick_file()
                {
                    load_req.path = Some(PathBuf::from(picked));
                }
                if wide_button(ui, "🗂  Reveal in filesystem", accent_col).clicked() {
                    let full = requested.root.join(&info.path);
                    let target = full.parent().unwrap_or(&requested.root).to_path_buf();
                    let _ = std::process::Command::new("xdg-open").arg(&target).spawn();
                }
            });
            section(ui, "sel_prim", "Selected prim", accent_col, true, |ui| match selected.0 {
                Some(entity) => {
                    if let Ok((_, n, pr)) = prims.get(entity) {
                        readout_row(ui, "name", n.as_str());
                        readout_row(ui, "path", pr.path.as_str());

                        // Feature chips — derived purely from ECS
                        // component presence so the row stays in sync
                        // with the live stage without a dedicated cache.
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 3.0;
                            if mesh_q.get(entity).is_ok() {
                                chip(ui, "mesh", accent_col);
                            }
                            if let Ok(k) = kind_q.get(entity) {
                                chip(ui, &format!("kind:{}", k.kind), accent_col);
                            }
                            if children.get(entity).map(|c| !c.is_empty()).unwrap_or(false) {
                                chip(ui, "parent", accent_col);
                            }
                            if audio_q.get(entity).is_ok() {
                                chip(ui, "audio", accent_col);
                            }
                            if proc_q.get(entity).is_ok() {
                                chip(ui, "procedural", accent_col);
                            }
                            if matches!(vis_q.get(entity), Ok(Visibility::Hidden)) {
                                chip_colored(ui, "hidden", style::WARNING, accent_col);
                            }
                        });

                        if wide_button(ui, "Clear selection", accent_col).clicked() {
                            selected.0 = None;
                        }
                    } else {
                        sub_caption(ui, "(selection stale)");
                        selected.0 = None;
                    }
                }
                None => sub_caption(ui, "Click a prim in the Tree panel"),
            });
        });
}

// ─── Prim-tree panel ────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_tree_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    mut selected: ResMut<SelectedPrim>,
    mut fly: ResMut<FlyTo>,
    mut expanded: ResMut<TreeExpanded>,
    mut filter: ResMut<TreeFilter>,
    materials: Res<Assets<StandardMaterial>>,
    cameras: Query<&ArcballCamera>,
    gt_query: Query<&GlobalTransform>,
    extent_q: Query<&usd_bevy::UsdLocalExtent>,
    prims: Query<(Entity, &Name, &UsdPrimRef, Option<&UsdDisplayName>)>,
    mat_q: Query<&MeshMaterial3d<StandardMaterial>>,
    mut visibility_q: Query<(Entity, &mut Visibility)>,
    children: Query<&Children>,
) {
    if !is_panel_open(&open, RIB_TREE) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_TREE, "Prim tree", pane_anchor_for(RIB_TREE), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "tree_hierarchy", "Hierarchy", accent_col, true, |ui| {
                sub_caption(ui, &format!("{} prims", prims.iter().count()));
                ui.add_space(style::space::TIGHT);
                search_field(ui, &mut filter.0, "Search prims…", accent_col);
                ui.add_space(style::space::BLOCK);

                // Snapshot the current Visibility state so the tree
                // rows can drive eye-icon toggles via plain &mut bool —
                // we commit changes back to the ECS once the row
                // rendering is finished.
                let mut vis_cache: HashMap<Entity, bool> = HashMap::new();
                for (e, v) in visibility_q.iter() {
                    vis_cache.insert(e, !matches!(*v, Visibility::Hidden));
                }
                let vis_before = vis_cache.clone();

                let filter_lc = filter.0.to_lowercase();
                let flat = !filter_lc.is_empty();

                let mut outcome = RowOutcome::default();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .min_scrolled_height(600.0)
                    .max_height(600.0)
                    .show(ui, |ui| {
                        if flat {
                            let mut matches: Vec<(
                                Entity,
                                &Name,
                                &UsdPrimRef,
                                Option<&UsdDisplayName>,
                            )> = prims
                                .iter()
                                .filter(|(_, _, pref, _)| {
                                    pref.path.to_lowercase().contains(&filter_lc)
                                })
                                .collect();
                            matches.sort_by(|a, b| a.2.path.cmp(&b.2.path));
                            if matches.is_empty() {
                                sub_caption(ui, "(no matches)");
                            }
                            for (entity, name, pref, dn) in &matches {
                                let sub = draw_tree_row(
                                    ui,
                                    *entity,
                                    name,
                                    pref,
                                    *dn,
                                    &prims,
                                    &mat_q,
                                    &materials,
                                    &mut vis_cache,
                                    &children,
                                    &selected,
                                    &mut expanded,
                                    accent_col,
                                    0,
                                    true,
                                );
                                outcome.merge(sub);
                            }
                        } else {
                            let mut roots: Vec<(
                                Entity,
                                &Name,
                                &UsdPrimRef,
                                Option<&UsdDisplayName>,
                            )> = prims
                                .iter()
                                .filter(|(_, _, pref, _)| {
                                    let p = pref.path.as_str();
                                    p.starts_with('/') && p.len() > 1 && !p[1..].contains('/')
                                })
                                .collect();
                            roots.sort_by(|a, b| a.2.path.cmp(&b.2.path));

                            if roots.is_empty() {
                                sub_caption(ui, "(no prims yet — stage loading)");
                            } else {
                                for (entity, name, pref, dn) in &roots {
                                    let sub = draw_tree_row(
                                        ui,
                                        *entity,
                                        name,
                                        pref,
                                        *dn,
                                        &prims,
                                        &mat_q,
                                        &materials,
                                        &mut vis_cache,
                                        &children,
                                        &selected,
                                        &mut expanded,
                                        accent_col,
                                        0,
                                        false,
                                    );
                                    outcome.merge(sub);
                                }
                            }
                        }
                    });

                // Commit eye-icon toggles back to the ECS.
                for (entity, visible) in &vis_cache {
                    if vis_before.get(entity) != Some(visible) {
                        if let Ok((_, mut v)) = visibility_q.get_mut(*entity) {
                            *v = if *visible {
                                Visibility::Inherited
                            } else {
                                Visibility::Hidden
                            };
                        }
                    }
                }

                if let Some(action) = outcome.ctx_action {
                    match action {
                        CtxAction::FlyTo(entity) => {
                            selected.0 = Some(entity);
                            if let (Ok(target_gt), Ok(cam)) =
                                (gt_query.get(entity), cameras.single())
                            {
                                let target = target_gt.translation();
                                let target_dist = (cam.distance * 0.25).clamp(0.2, 40.0);
                                fly.start_focus = cam.focus;
                                fly.start_distance = cam.distance;
                                fly.target_focus = target;
                                fly.target_distance = target_dist;
                                fly.duration = 0.4;
                                fly.remaining = 0.4;
                            }
                        }
                        CtxAction::Fit(entity) => {
                            selected.0 = Some(entity);
                            if let Ok(cam) = cameras.single() {
                                let (target, target_dist) = fit_params_for_entity(
                                    entity,
                                    &gt_query,
                                    &extent_q,
                                    &children,
                                    cam.distance,
                                );
                                fly.start_focus = cam.focus;
                                fly.start_distance = cam.distance;
                                fly.target_focus = target;
                                fly.target_distance = target_dist;
                                fly.duration = 0.4;
                                fly.remaining = 0.4;
                            }
                        }
                        CtxAction::ExpandDesc(entity) => {
                            set_subtree_expanded(entity, &prims, &children, &mut expanded, true);
                        }
                        CtxAction::CollapseDesc(entity) => {
                            set_subtree_expanded(entity, &prims, &children, &mut expanded, false);
                        }
                    }
                }

                if let Some(entity) = outcome.double_clicked {
                    selected.0 = Some(entity);
                    if let Ok(cam) = cameras.single() {
                        let (target, target_dist) = fit_params_for_entity(
                            entity,
                            &gt_query,
                            &extent_q,
                            &children,
                            cam.distance,
                        );
                        fly.start_focus = cam.focus;
                        fly.start_distance = cam.distance;
                        fly.target_focus = target;
                        fly.target_distance = target_dist;
                        fly.duration = 0.4;
                        fly.remaining = 0.4;
                    }
                } else if let Some(entity) = outcome.clicked {
                    selected.0 = Some(entity);
                    if let (Ok(target_gt), Ok(cam)) = (gt_query.get(entity), cameras.single()) {
                        let target = target_gt.translation();
                        let target_dist = (cam.distance * 0.25).clamp(0.2, 40.0);
                        fly.start_focus = cam.focus;
                        fly.start_distance = cam.distance;
                        fly.target_focus = target;
                        fly.target_distance = target_dist;
                        fly.duration = 0.4;
                        fly.remaining = 0.4;
                    }
                }
            });
        });
}

#[derive(Default, Clone, Copy)]
struct RowOutcome {
    clicked: Option<Entity>,
    double_clicked: Option<Entity>,
    ctx_action: Option<CtxAction>,
}

#[derive(Clone, Copy, Debug)]
enum CtxAction {
    FlyTo(Entity),
    Fit(Entity),
    ExpandDesc(Entity),
    CollapseDesc(Entity),
}

impl RowOutcome {
    fn merge(&mut self, other: RowOutcome) {
        if other.double_clicked.is_some() {
            self.double_clicked = other.double_clicked;
        }
        if other.clicked.is_some() {
            self.clicked = other.clicked;
        }
        if other.ctx_action.is_some() {
            self.ctx_action = other.ctx_action;
        }
    }
}

/// Walk the subtree rooted at `root` and set each descendant's
/// `TreeExpanded` entry to `open`.
fn set_subtree_expanded(
    root: Entity,
    prims: &Query<(Entity, &Name, &UsdPrimRef, Option<&UsdDisplayName>)>,
    children: &Query<&Children>,
    expanded: &mut TreeExpanded,
    open: bool,
) {
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        if let Ok((_, _, pref, _)) = prims.get(e) {
            expanded.0.insert(pref.path.clone(), open);
        }
        if let Ok(cs) = children.get(e) {
            for c in cs.iter() {
                stack.push(c);
            }
        }
    }
}

/// Lookup the first-bound material's `base_color` for `entity` (or one
/// of its direct mesh-carrying children) and convert linear sRGB into
/// an egui colour suitable for a tree-row swatch.
fn swatch_color_for(
    entity: Entity,
    mat_q: &Query<&MeshMaterial3d<StandardMaterial>>,
    children: &Query<&Children>,
    materials: &Assets<StandardMaterial>,
) -> Option<egui::Color32> {
    let pick = |e: Entity| -> Option<egui::Color32> {
        let mm = mat_q.get(e).ok()?;
        let mat = materials.get(&mm.0)?;
        let c = mat.base_color.to_linear();
        Some(style::srgb_to_egui([c.red, c.green, c.blue]))
    };
    if let Some(c) = pick(entity) {
        return Some(c);
    }
    if let Ok(cs) = children.get(entity) {
        for c in cs.iter() {
            if let Some(col) = pick(c) {
                return Some(col);
            }
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn draw_tree_row(
    ui: &mut egui::Ui,
    entity: Entity,
    name: &Name,
    prim_ref: &UsdPrimRef,
    display_name: Option<&UsdDisplayName>,
    prims: &Query<(Entity, &Name, &UsdPrimRef, Option<&UsdDisplayName>)>,
    mat_q: &Query<&MeshMaterial3d<StandardMaterial>>,
    materials: &Assets<StandardMaterial>,
    vis_cache: &mut HashMap<Entity, bool>,
    children: &Query<&Children>,
    selected: &SelectedPrim,
    expanded: &mut TreeExpanded,
    accent: egui::Color32,
    depth: u32,
    leaf_override: bool,
) -> RowOutcome {
    let child_ids: Vec<Entity> = children
        .get(entity)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let mut prim_children: Vec<(Entity, &Name, &UsdPrimRef, Option<&UsdDisplayName>)> = child_ids
        .iter()
        .filter_map(|c| prims.get(*c).ok())
        .collect();
    prim_children.sort_by(|a, b| a.2.path.cmp(&b.2.path));
    let has_children = !leaf_override && !prim_children.is_empty();

    let is_selected = selected.0 == Some(entity);
    let path_key = prim_ref.path.clone();
    let row_id_salt = entity.to_bits();
    let mut outcome = RowOutcome::default();

    // Eye + swatch slots.
    let mut visible_flag = *vis_cache.get(&entity).unwrap_or(&true);
    let swatch = swatch_color_for(entity, mat_q, children, materials);
    let mut color_sentinel = false;

    // Label preference: authored `ui:displayName` (UsdUI) > prim leaf
    // name.
    let label_owned: String = display_name
        .map(|d| d.0.clone())
        .unwrap_or_else(|| name.as_str().to_string());

    let resp = {
        let mut slot_buf: Vec<TreeIconSlot<'_>> = Vec::with_capacity(2);
        slot_buf.push(
            TreeIconSlot::new(TreeIconKind::Eye, &mut visible_flag)
                .with_tooltip("Toggle visibility"),
        );
        if let Some(c) = swatch {
            slot_buf.push(TreeIconSlot::new(TreeIconKind::Color(c), &mut color_sentinel));
        }

        if has_children {
            let is_open = *expanded.0.entry(path_key.clone()).or_insert(true);
            let mut open_ref = is_open;
            let r = tree_row(
                ui,
                row_id_salt,
                depth,
                Some(&mut open_ref),
                None,
                &label_owned,
                is_selected,
                accent,
                &mut slot_buf,
            );
            if open_ref != is_open {
                expanded.0.insert(path_key.clone(), open_ref);
            }
            r
        } else {
            tree_row(
                ui,
                row_id_salt,
                depth,
                None,
                None,
                &label_owned,
                is_selected,
                accent,
                &mut slot_buf,
            )
        }
    };

    vis_cache.insert(entity, visible_flag);

    if resp.body.hovered() {
        resp.body.clone().on_hover_text(&prim_ref.path);
    }
    if resp.body.double_clicked() {
        outcome.double_clicked = Some(entity);
    } else if resp.body.clicked() {
        outcome.clicked = Some(entity);
    }

    context_menu_mara(&resp.body, accent, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if wide_button(ui, "Fly to", accent).clicked() {
            outcome.ctx_action = Some(CtxAction::FlyTo(entity));
            ui.close();
        }
        if wide_button(ui, "Fit to bounds", accent).clicked() {
            outcome.ctx_action = Some(CtxAction::Fit(entity));
            ui.close();
        }
        if wide_button(ui, "Copy path", accent).clicked() {
            ui.ctx().copy_text(prim_ref.path.clone());
            ui.close();
        }
        if wide_button(ui, "Expand descendants", accent).clicked() {
            outcome.ctx_action = Some(CtxAction::ExpandDesc(entity));
            ui.close();
        }
        if wide_button(ui, "Collapse descendants", accent).clicked() {
            outcome.ctx_action = Some(CtxAction::CollapseDesc(entity));
            ui.close();
        }
    });

    let show_children = if has_children {
        *expanded.0.get(&path_key).unwrap_or(&true)
    } else {
        false
    };
    if show_children {
        for (child_entity, child_name, child_ref, child_dn) in prim_children {
            let sub = draw_tree_row(
                ui,
                child_entity,
                child_name,
                child_ref,
                child_dn,
                prims,
                mat_q,
                materials,
                vis_cache,
                children,
                selected,
                expanded,
                accent,
                depth + 1,
                false,
            );
            outcome.merge(sub);
        }
    }

    outcome
}

/// Walk the subtree rooted at `root`, transforming each descendant's
/// authored local extent into world space, and fold into one AABB.
/// Returns `(focus, distance)` sized for arcball framing.
fn fit_params_for_entity(
    root: Entity,
    gt_q: &Query<&GlobalTransform>,
    extent_q: &Query<&usd_bevy::UsdLocalExtent>,
    children: &Query<&Children>,
    current_cam_dist: f32,
) -> (Vec3, f32) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    let mut found = false;

    let mut stack: Vec<Entity> = vec![root];
    while let Some(e) = stack.pop() {
        if let (Ok(gt), Ok(le)) = (gt_q.get(e), extent_q.get(e)) {
            let m = gt.to_matrix();
            for i in 0..8 {
                let c = Vec3::new(
                    if i & 1 == 0 { le.min[0] } else { le.max[0] },
                    if i & 2 == 0 { le.min[1] } else { le.max[1] },
                    if i & 4 == 0 { le.min[2] } else { le.max[2] },
                );
                let w = m.transform_point3(c);
                min = min.min(w);
                max = max.max(w);
            }
            found = true;
        }
        if let Ok(cs) = children.get(e) {
            for c in cs.iter() {
                stack.push(c);
            }
        }
    }

    if found {
        let center = (min + max) * 0.5;
        let size = (max - min).abs();
        let max_dim = size.x.max(size.y).max(size.z).max(0.05);
        let dist = (max_dim * 1.6).clamp(0.2, 200.0);
        (center, dist)
    } else if let Ok(gt) = gt_q.get(root) {
        (gt.translation(), (current_cam_dist * 0.25).clamp(0.2, 40.0))
    } else {
        (Vec3::ZERO, current_cam_dist)
    }
}

// ─── Stage-info panel ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_info_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    info: Res<StageInfo>,
    mut reload: ResMut<ReloadRequest>,
    prims: Query<&UsdPrimRef>,
    meshes_q: Query<&Mesh3d, With<UsdPrimRef>>,
    spatial_audio_q: Query<&UsdSpatialAudio>,
    procedural_q: Query<&UsdProcedural>,
) {
    if !is_panel_open(&open, RIB_INFO) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_INFO, "Stage info", pane_anchor_for(RIB_INFO), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "info_stage", "Stage", accent_col, true, |ui| {
                readout_row(ui, "file", &info.path);
                readout_row(ui, "defaultPrim", info.default_prim.as_deref().unwrap_or("—"));
                readout_row(ui, "layers", &info.layer_count.to_string());
                readout_row(ui, "prims", &prims.iter().count().to_string());
                readout_row(ui, "meshes", &meshes_q.iter().count().to_string());
                readout_row(ui, "variants", &info.variant_count.to_string());
            });
            section(ui, "info_lights", "Lights & instances", accent_col, true, |ui| {
                let light_labels = [
                    format!("{} dir", info.lights_directional),
                    format!("{} pt", info.lights_point),
                    format!("{} spot", info.lights_spot),
                    format!("{} dome", info.lights_dome),
                ];
                let refs: Vec<&str> = light_labels.iter().map(String::as_str).collect();
                badge_row(ui, "lights", &refs, accent_col);

                let inst_labels = [
                    format!("{} prim", info.instance_prim_count),
                    format!("{} reuse", info.instance_prototype_reuses),
                ];
                let refs: Vec<&str> = inst_labels.iter().map(String::as_str).collect();
                badge_row(ui, "instances", &refs, accent_col);

                readout_row(ui, "animated", &format!("{} prim(s)", info.animated_prim_count));
            });
            section(ui, "info_skel_render", "Skel & render", accent_col, true, |ui| {
                let skel_labels = [
                    format!("{} skel", info.skeleton_count),
                    format!("{} root", info.skel_root_count),
                    format!("{} bind", info.skel_binding_count),
                ];
                let refs: Vec<&str> = skel_labels.iter().map(String::as_str).collect();
                badge_row(ui, "skel", &refs, accent_col);

                let render_labels = [
                    format!("{} settings", info.render_settings_count),
                    format!("{} product", info.render_product_count),
                    format!("{} var", info.render_var_count),
                ];
                let refs: Vec<&str> = render_labels.iter().map(String::as_str).collect();
                badge_row(ui, "render", &refs, accent_col);

                if let Some([w, h]) = info.render_primary_resolution {
                    readout_row(ui, "resolution", &format!("{w} × {h}"));
                }

                let phys_labels = [
                    format!("{} scene", info.physics_scene_count),
                    format!("{} rigid", info.rigid_body_count),
                    format!("{} joint", info.joint_count),
                ];
                let refs: Vec<&str> = phys_labels.iter().map(String::as_str).collect();
                badge_row(ui, "physics", &refs, accent_col);
            });
            section(ui, "info_authoring", "Authoring detail", accent_col, true, |ui| {
                readout_row(
                    ui,
                    "custom",
                    &format!(
                        "{} prim · {} layer entries",
                        info.custom_attr_prim_count, info.custom_layer_data_entries
                    ),
                );
                readout_row(
                    ui,
                    "subdiv",
                    &format!("{} mesh(es) subdivision", info.subdivision_prim_count),
                );
                readout_row(
                    ui,
                    "light-link",
                    &format!("{} light(s) linked", info.light_linked_count),
                );
                readout_row(ui, "clips", &format!("{} prim(s) UsdClipsAPI", info.clip_prim_count));
                readout_row(
                    ui,
                    "spatial-audio",
                    &format!("{} source(s)", spatial_audio_q.iter().count()),
                );
                readout_row(ui, "procedural", &format!("{} prim(s)", procedural_q.iter().count()));
            });
            section(ui, "info_actions", "Actions", accent_col, true, |ui| {
                if wide_button(ui, "⟳  Reload stage (R)", accent_col).clicked() {
                    reload.requested = true;
                }
            });
        });
}

// ─── Variants panel ─────────────────────────────────────────────────

fn draw_variants_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    stage: Option<Res<crate::StageHandle>>,
    usd_assets: Res<Assets<UsdAsset>>,
    mut loader_tuning: ResMut<LoaderTuning>,
    mut pending_anim: ResMut<PendingAnimationClip>,
    mut pending_variant_switch: ResMut<usd_bevy::incremental::PendingVariantSwitch>,
    mut reload: ResMut<ReloadRequest>,
) {
    if !is_panel_open(&open, RIB_VARIANTS) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_VARIANTS, "Variants", pane_anchor_for(RIB_VARIANTS), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "variants_animation", "Animation clips", accent_col, true, |ui| {
                let asset = stage.as_ref().and_then(|stage| usd_assets.get(&stage.0));
                let Some(asset) = asset else {
                    sub_caption(ui, "(no stage loaded yet)");
                    return;
                };

                let mut anim_sets: Vec<_> = asset
                    .variants
                    .iter()
                    .flat_map(|(prim_path, sets)| {
                        sets.iter()
                            .filter(|set| set.name == "anim" && !set.options.is_empty())
                            .map(move |set| (prim_path, set))
                    })
                    .collect();
                anim_sets.sort_by(|a, b| a.0.cmp(b.0));

                if anim_sets.is_empty() {
                    if asset.skel_animations.is_empty() {
                        sub_caption(ui, "No UsdSkel animations or `anim` variant set found.");
                    } else {
                        sub_caption(
                            ui,
                            &format!(
                                "{} SkelAnimation prim(s) found; this stage does not expose an `anim` variant switch.",
                                asset.skel_animations.len()
                            ),
                        );
                    }
                    return;
                }

                sub_caption(ui, "Switches the live UsdSkel clip without reloading the stage.");
                ui.add_space(style::space::BLOCK);

                let mut changed = false;
                for (prim_path, set) in anim_sets {
                    let key = (prim_path.clone(), set.name.clone());
                    let authored = set.selection.as_deref().unwrap_or("");
                    let current = loader_tuning
                        .variants
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| authored.to_string());
                    let mut selected_idx =
                        set.options.iter().position(|o| o == &current).unwrap_or(0);
                    let options_str: Vec<&str> = set.options.iter().map(|s| s.as_str()).collect();

                    labelled_row(ui, prim_path.as_str(), |ui| {
                        let r = scroll_dropdown_control(
                            ui,
                            (prim_path.as_str(), "animation_clip"),
                            &mut selected_idx,
                            &options_str,
                            accent_col,
                        );
                        if r.changed() {
                            let picked = set.options[selected_idx].clone();
                            if picked != current {
                                loader_tuning.variants.insert(key.clone(), picked);
                                pending_anim.name = Some(set.options[selected_idx].clone());
                                changed = true;
                            }
                        }
                    });
                }
                let _ = changed;
            });
            section(ui, "variants_all", "Variant sets", accent_col, true, |ui| {
                let asset = stage.as_ref().and_then(|stage| usd_assets.get(&stage.0));
                match asset {
                    Some(asset)
                        if asset
                            .variants
                            .values()
                            .any(|sets| sets.iter().any(|set| set.name != "anim")) =>
                    {
                        let variant_prim_count = asset
                            .variants
                            .values()
                            .filter(|sets| sets.iter().any(|set| set.name != "anim"))
                            .count();
                        sub_caption(
                            ui,
                            &format!("{variant_prim_count} prims author non-animation variant sets"),
                        );
                        ui.add_space(style::space::BLOCK);

                        let mut changed = false;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let mut entries: Vec<_> = asset
                                .variants
                                .iter()
                                .filter_map(|(prim_path, sets)| {
                                    let variant_sets: Vec<_> =
                                        sets.iter().filter(|set| set.name != "anim").collect();
                                    (!variant_sets.is_empty()).then_some((prim_path, variant_sets))
                                })
                                .collect();
                            entries.sort_by(|a, b| a.0.cmp(b.0));
                            for (prim_path, sets) in entries {
                                section(
                                    ui,
                                    prim_path.as_str(),
                                    prim_path.as_str(),
                                    accent_col,
                                    true,
                                    |ui| {
                                        for set in sets {
                                            let key = (prim_path.clone(), set.name.clone());
                                            let authored = set.selection.as_deref().unwrap_or("");
                                            let current = loader_tuning
                                                .variants
                                                .get(&key)
                                                .cloned()
                                                .unwrap_or_else(|| authored.to_string());

                                            if set.options.is_empty() {
                                                readout_row(ui, &set.name, "(no options)");
                                                continue;
                                            }

                                            let mut selected_idx = set
                                                .options
                                                .iter()
                                                .position(|o| o == &current)
                                                .unwrap_or(0);
                                            let options_str: Vec<&str> =
                                                set.options.iter().map(|s| s.as_str()).collect();

                                            labelled_row(ui, &set.name, |ui| {
                                                let r = scroll_dropdown_control(
                                                    ui,
                                                    (prim_path.as_str(), set.name.as_str()),
                                                    &mut selected_idx,
                                                    &options_str,
                                                    accent_col,
                                                );
                                                if r.changed() {
                                                    let picked =
                                                        set.options[selected_idx].clone();
                                                    if picked != current {
                                                        loader_tuning
                                                            .variants
                                                            .insert(key.clone(), picked.clone());
                                                        // Apply the switch incrementally (live
                                                        // material swap, no scene rebuild).
                                                        pending_variant_switch.queue.push((
                                                            prim_path.clone(),
                                                            set.name.clone(),
                                                            picked,
                                                        ));
                                                    }
                                                }
                                            });

                                            if !current.is_empty() && current != authored {
                                                labelled_row(ui, "", |ui| {
                                                    if ui
                                                        .small_button("reset to authored")
                                                        .clicked()
                                                    {
                                                        loader_tuning.variants.remove(&key);
                                                        changed = true;
                                                    }
                                                });
                                            }
                                        }
                                    },
                                );
                            }
                        });
                        if changed {
                            reload.requested = true;
                        }
                    }
                    Some(_) => {
                        sub_caption(ui, "Stage authors no non-animation variant sets.");
                    }
                    None => {
                        sub_caption(ui, "(no stage loaded yet)");
                    }
                }
            });
        });
}

/// Local dropdown for long USD variant / animation lists. egui's
/// ComboBox has a built-in scroll area via `.height(...)`, while still
/// returning a normal changed Response.
fn scroll_dropdown_control(
    ui: &mut egui::Ui,
    id_salt: impl Hash,
    selected: &mut usize,
    options: &[&str],
    _accent: egui::Color32,
) -> egui::Response {
    let display = options.get(*selected).copied().unwrap_or("—");
    let max_w = ui.available_width().max(60.0).min(200.0);
    let mut changed = false;
    let mut response = egui::ComboBox::from_id_salt(("usdview_scroll_dropdown", id_salt))
        .selected_text(display)
        .width(max_w)
        .height(240.0)
        .show_ui(ui, |ui| {
            for (idx, opt) in options.iter().enumerate() {
                if ui.selectable_label(*selected == idx, *opt).clicked() {
                    if *selected != idx {
                        *selected = idx;
                        changed = true;
                    }
                    ui.close();
                }
            }
        })
        .response;
    if response.clicked() || changed {
        response.request_focus();
    }
    if response.has_focus() && !options.is_empty() {
        ui.ctx().memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
        let delta = ui.input_mut(|i| {
            i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) as isize
                - i.count_and_consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) as isize
        });
        if delta != 0 {
            let len = options.len() as isize;
            let next = (*selected as isize + delta).rem_euclid(len) as usize;
            if next != *selected {
                *selected = next;
                changed = true;
                response.request_focus();
            }
        }
    }
    if changed {
        response.mark_changed();
    }
    response
}

// ─── Cameras panel ──────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_cameras_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    usd_assets: Res<Assets<UsdAsset>>,
    mut camera_mount: ResMut<CameraMount>,
    mut bookmarks: ResMut<CameraBookmarks>,
    mut fly: ResMut<FlyTo>,
    cameras: Query<&ArcballCamera>,
) {
    if !is_panel_open(&open, RIB_CAMERAS) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_CAMERAS, "Cameras", pane_anchor_for(RIB_CAMERAS), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "cameras_bookmarks", "Bookmarks", accent_col, true, |ui| {
                if wide_button(ui, "💾  Save current view", accent_col).clicked() {
                    if let Ok(cam) = cameras.single() {
                        let seq = bookmarks.next_seq + 1;
                        bookmarks.next_seq = seq;
                        bookmarks.items.push(CameraBookmark {
                            name: format!("View {seq}"),
                            focus: cam.focus,
                            distance: cam.distance,
                            yaw: cam.yaw,
                            elevation: cam.elevation,
                        });
                    }
                }
                if bookmarks.items.is_empty() {
                    sub_caption(ui, "(no bookmarks yet)");
                } else {
                    let mut to_delete: Option<usize> = None;
                    let mut to_jump: Option<usize> = None;
                    for (idx, bm) in bookmarks.items.iter().enumerate() {
                        let r = hybrid_select_row(
                            ui,
                            ("bookmark", idx),
                            &bm.name,
                            Some(&format!("d {:.1}", bm.distance)),
                            false,
                            false,
                            accent_col,
                        );
                        if r.body.clicked() {
                            to_jump = Some(idx);
                        }
                        if r.radio.clicked() {
                            to_delete = Some(idx);
                        }
                    }
                    if let Some(idx) = to_jump
                        && let (Ok(cam), Some(bm)) = (cameras.single(), bookmarks.items.get(idx))
                    {
                        *camera_mount = CameraMount::Arcball;
                        fly.start_focus = cam.focus;
                        fly.start_distance = cam.distance;
                        fly.start_yaw = Some(cam.yaw);
                        fly.start_elevation = Some(cam.elevation);
                        fly.target_focus = bm.focus;
                        fly.target_distance = bm.distance;
                        fly.target_yaw = Some(bm.yaw);
                        fly.target_elevation = Some(bm.elevation);
                        fly.duration = 0.5;
                        fly.remaining = 0.5;
                    }
                    if let Some(idx) = to_delete {
                        bookmarks.items.remove(idx);
                    }
                    sub_caption(ui, "Click row to jump · click radio to delete");
                }
            });

            section(ui, "cameras_all", "Cameras", accent_col, true, |ui| {
                let asset = usd_assets.iter().next().map(|(_, a)| a);
                let Some(asset) = asset else {
                    sub_caption(ui, "(no stage loaded yet)");
                    return;
                };
                sub_caption(ui, &format!("{} authored cameras", asset.cameras.len()));
                ui.add_space(style::space::BLOCK);

                let arcball_active = matches!(*camera_mount, CameraMount::Arcball);
                let r = hybrid_select_row(
                    ui,
                    "arcball_mount",
                    "🎮  Arcball (free)",
                    None,
                    arcball_active,
                    arcball_active,
                    accent_col,
                );
                if r.body.clicked() || r.radio.clicked() {
                    *camera_mount = CameraMount::Arcball;
                }

                row_separator(ui);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for cam in &asset.cameras {
                        let mounted = matches!(
                            &*camera_mount,
                            CameraMount::Mounted { prim_path } if prim_path == &cam.path
                        );
                        let name = cam.path.rsplit('/').next().unwrap_or(&cam.path);
                        let focal = cam.data.focal_length_mm.unwrap_or(50.0);
                        let proj = match cam.data.projection {
                            Some(usd_bevy::read::camera::Projection::Orthographic) => "ortho",
                            _ => "persp",
                        };
                        let label = format!("📷  {name}");
                        let trailing = format!("{focal:.0}mm · {proj}");
                        let r = hybrid_select_row(
                            ui,
                            cam.path.as_str(),
                            &label,
                            Some(&trailing),
                            mounted,
                            mounted,
                            accent_col,
                        );
                        if r.body.clicked() || r.radio.clicked() {
                            *camera_mount = CameraMount::Mounted {
                                prim_path: cam.path.clone(),
                            };
                        }
                    }
                });
            });
        });
}

// ─── Materials panel ────────────────────────────────────────────────

fn draw_materials_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    materials: Res<Assets<StandardMaterial>>,
    asset_server: Res<AssetServer>,
    usd_mesh_mats: Query<&MeshMaterial3d<StandardMaterial>, With<UsdPrimRef>>,
) {
    if !is_panel_open(&open, RIB_MATERIALS) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;

    let mut bound: std::collections::HashSet<AssetId<StandardMaterial>> =
        std::collections::HashSet::new();
    for mm in usd_mesh_mats.iter() {
        bound.insert(mm.0.id());
    }

    // Stable presentation order: by asset path / id.
    let mut entries: Vec<(AssetId<StandardMaterial>, String)> = materials
        .iter()
        .filter(|(id, _)| bound.contains(id))
        .map(|(id, _)| {
            let label = asset_server
                .get_path(id)
                .map(|p| p.to_string())
                .unwrap_or_else(|| format!("{id:?}"));
            (id, label)
        })
        .collect();
    entries.sort_by(|a, b| a.1.cmp(&b.1));

    Pane::new(RIB_MATERIALS, "Materials", pane_anchor_for(RIB_MATERIALS), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(
                ui,
                "materials_overview",
                &format!("{} material(s)", entries.len()),
                accent_col,
                true,
                |ui| {
                    sub_caption(
                        ui,
                        "Read-only. Materials use the USD/default values; this panel does not tint colors or override metallic/roughness.",
                    );
                },
            );
            for (id, label) in &entries {
                let short = label
                    .rsplit('/')
                    .next()
                    .unwrap_or(label)
                    .chars()
                    .take(48)
                    .collect::<String>();
                let section_id = format!("mat_{:?}", id);
                section(ui, &section_id, &short, accent_col, false, |ui| {
                    let Some(mat) = materials.get(*id) else {
                        return;
                    };
                    ui.label(egui::RichText::new(label).small().monospace());
                    ui.add_space(style::space::BLOCK);
                    let texture_state = if mat.base_color_texture.is_some() {
                        "textured"
                    } else {
                        "constant color"
                    };
                    readout_row(ui, "Albedo", texture_state);
                    readout_row(ui, "Roughness", "USD/default");
                    readout_row(ui, "Metallic", "USD/default");
                });
            }
        });
}

// ─── Overlays panel ─────────────────────────────────────────────────

fn draw_overlays_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    mut toggles: ResMut<DisplayToggles>,
    mut loader_tuning: ResMut<LoaderTuning>,
) {
    if !is_panel_open(&open, RIB_OVERLAYS) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_OVERLAYS, "Overlays", pane_anchor_for(RIB_OVERLAYS), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "overlay_toggles", "World overlays", accent_col, true, |ui| {
                toggle(ui, "Ground grid (G)", &mut toggles.show_world_grid, accent_col);
                toggle(ui, "World axes (X)", &mut toggles.show_world_axes, accent_col);
                toggle(ui, "Prim markers (P)", &mut toggles.show_prim_markers, accent_col);
                let mut v = toggles.prim_marker_bias as f64;
                if pretty_slider(ui, "Prim marker bias", &mut v, 0.0..=5.0, 2, "×", accent_col)
                    .changed()
                {
                    toggles.prim_marker_bias = v as f32;
                }
                toggle(ui, "Skeleton bones (B)", &mut toggles.show_skeleton, accent_col);
                toggle(ui, "Physics gizmos (Y)", &mut toggles.show_physics, accent_col);
                toggle(ui, "Collider wireframes (C)", &mut toggles.show_colliders, accent_col);
            });

            section(ui, "overlay_render", "Render", accent_col, true, |ui| {
                toggle(ui, "Wireframe", &mut toggles.wireframe, accent_col);
                let mut s = toggles.light_intensity_scale as f64;
                if pretty_slider(ui, "Light intensity", &mut s, 0.0..=5.0, 2, "×", accent_col)
                    .changed()
                {
                    toggles.light_intensity_scale = s as f32;
                }
                sub_caption(ui, "Scales every authored light from its original value.");
            });

            section(ui, "overlay_curves", "Curves (tubes)", accent_col, true, |ui| {
                sub_caption(ui, "Default radius used when widths aren't authored");
                let mut r = loader_tuning.curves.default_radius as f64;
                if pretty_slider(ui, "Radius", &mut r, 0.001..=0.2, 3, " m", accent_col).changed() {
                    loader_tuning.curves.default_radius = r as f32;
                }
                let mut seg = loader_tuning.curves.ring_segments as f64;
                if pretty_slider(ui, "Ring segments", &mut seg, 3.0..=24.0, 0, "", accent_col)
                    .changed()
                {
                    loader_tuning.curves.ring_segments = seg.round() as u32;
                }
                let mut ps = loader_tuning.curves.point_scale as f64;
                if pretty_slider(ui, "Point scale", &mut ps, 0.05..=4.0, 2, "×", accent_col)
                    .changed()
                {
                    loader_tuning.curves.point_scale = ps as f32;
                }
                sub_caption(ui, "Sliders apply live — no reload needed.");
            });
        });
}

// ─── Timeline panel ─────────────────────────────────────────────────

fn draw_timeline_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    mut clock: ResMut<UsdStageTime>,
    usd_assets: Res<Assets<UsdAsset>>,
) {
    if !is_panel_open(&open, RIB_TIMELINE) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_TIMELINE, "Timeline", pane_anchor_for(RIB_TIMELINE), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "timeline_playback", "Playback", accent_col, true, |ui| {
                let asset = usd_assets.iter().next().map(|(_, a)| a);
                let animated_count = asset.map(|a| a.animated_prims.len()).unwrap_or(0);
                sub_caption(
                    ui,
                    &format!(
                        "{animated_count} animated prim(s) · {:.1} fps · {:.1}s total",
                        clock.time_codes_per_second,
                        clock.duration_seconds()
                    ),
                );
                ui.add_space(style::space::BLOCK);

                let play_label = if clock.playing { "⏸  Pause" } else { "▶  Play" };
                if wide_button(ui, play_label, accent_col).clicked() {
                    clock.playing = !clock.playing;
                }
                if wide_button(ui, "⏮  Rewind", accent_col).clicked() {
                    clock.seconds = 0.0;
                }

                ui.add_space(style::space::BLOCK);
                let dur = clock.duration_seconds().max(1e-3);
                let _ = pretty_slider(ui, "Seconds", &mut clock.seconds, 0.0..=dur, 3, " s", accent_col);

                readout_row(ui, "timeCode", &format!("{:.3}", clock.current_time_code()));
                readout_row(
                    ui,
                    "range",
                    &format!("{:.2} … {:.2}", clock.start_time_code, clock.end_time_code),
                );
                readout_row(ui, "fps", &format!("{:.2}", clock.time_codes_per_second));
            });
        });
}

// ─── Keys panel ─────────────────────────────────────────────────────

fn draw_keys_panel(mut contexts: EguiContexts, open: Res<RibbonOpen>, accent: Res<AccentColor>) {
    if !is_panel_open(&open, RIB_KEYS) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_KEYS, "Controls", pane_anchor_for(RIB_KEYS), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "keys_camera", "Camera", accent_col, true, |ui| {
                keybinding_row(ui, "L+R drag", "Orbit");
                keybinding_row(ui, "Middle", "Pan");
                keybinding_row(ui, "Scroll", "Zoom");
            });
            section(ui, "keys_panels", "Panels", accent_col, true, |ui| {
                keybinding_row(ui, "T", "Toggle prim tree");
                keybinding_row(ui, "I", "Toggle stage info");
                keybinding_row(ui, "O", "Toggle overlays");
                keybinding_row(ui, "?", "Toggle this panel");
            });
            section(ui, "keys_overlays", "Overlays", accent_col, true, |ui| {
                keybinding_row(ui, "G", "Ground grid");
                keybinding_row(ui, "X", "World axes");
                keybinding_row(ui, "P", "Prim markers");
                keybinding_row(ui, "B", "Skeleton bones");
            });
            section(ui, "keys_stage", "Stage", accent_col, true, |ui| {
                keybinding_row(ui, "R", "Reload stage from disk");
            });
        });
}

// ─── Log panel ──────────────────────────────────────────────────────

fn draw_log_panel(
    mut contexts: EguiContexts,
    open: Res<RibbonOpen>,
    accent: Res<AccentColor>,
    log: Res<crate::log_panel::LoaderLog>,
) {
    if !is_panel_open(&open, RIB_LOG) {
        return;
    }
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let accent_col = accent.0;
    Pane::new(RIB_LOG, "Log", pane_anchor_for(RIB_LOG), accent_col)
        .resize(PaneResize::SPAN)
        .show(ctx, |body| {
            let ui = body.ui();
            section(ui, "log_lines", "Loader log", accent_col, true, |ui| {
                let count = log.buffer.lock().map(|b| b.len()).unwrap_or(0);
                sub_caption(ui, &format!("{count} entries · capped at 500"));
                ui.horizontal(|ui| {
                    if ui.small_button("Clear").clicked()
                        && let Ok(mut buf) = log.buffer.lock()
                    {
                        buf.clear();
                    }
                });
                ui.add_space(style::space::TIGHT);

                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let snapshot: Vec<crate::log_panel::LogLine> = log
                            .buffer
                            .lock()
                            .map(|b| b.iter().cloned().collect())
                            .unwrap_or_default();
                        if snapshot.is_empty() {
                            sub_caption(ui, "(no events yet — load a stage)");
                            return;
                        }
                        for line in &snapshot {
                            let level_color = level_to_color(line.level);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.painter().rect_filled(
                                    egui::Rect::from_center_size(
                                        ui.cursor().min + egui::vec2(4.0, 8.0),
                                        egui::vec2(6.0, 6.0),
                                    ),
                                    egui::CornerRadius::same(1),
                                    level_color,
                                );
                                ui.add_space(10.0);
                                ui.label(
                                    egui::RichText::new(short_target(&line.target))
                                        .small()
                                        .monospace()
                                        .color(style::TEXT_SECONDARY),
                                );
                                ui.label(
                                    egui::RichText::new(&line.message)
                                        .small()
                                        .color(style::TEXT_PRIMARY),
                                );
                            });
                        }
                    });
            });
        });
}

fn level_to_color(level: bevy::log::Level) -> egui::Color32 {
    match level {
        bevy::log::Level::ERROR => style::DANGER,
        bevy::log::Level::WARN => style::WARNING,
        bevy::log::Level::INFO => style::SUCCESS,
        _ => style::TEXT_SECONDARY,
    }
}

fn short_target(target: &str) -> String {
    target.rsplit("::").next().unwrap_or(target).to_string()
}

// ─── Command palette (Ctrl+K) ───────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_palette_panel(
    mut contexts: EguiContexts,
    accent: Res<AccentColor>,
    mut palette: ResMut<ViewerCommandPalette>,
    mut ribbon: ResMut<RibbonOpen>,
    mut toggles: ResMut<DisplayToggles>,
    mut reload: ResMut<ReloadRequest>,
    mut load_req: ResMut<LoadRequest>,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        return;
    };
    let Some(id) = command_palette(ctx, &mut palette.0, PALETTE_ITEMS, accent.0) else {
        return;
    };
    match id {
        "open_selection" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_SELECTION);
        }
        "open_tree" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_TREE);
        }
        "open_info" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_INFO);
        }
        "open_variants" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_VARIANTS);
        }
        "open_cameras" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_CAMERAS);
        }
        "open_overlays" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_OVERLAYS);
        }
        "open_timeline" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_TIMELINE);
        }
        "open_keys" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_KEYS);
        }
        "open_log" => {
            ribbon.per_ribbon.insert(RIBBON_LEFT, RIB_LOG);
        }
        "toggle_grid" => {
            toggles.show_world_grid = !toggles.show_world_grid;
        }
        "toggle_axes" => {
            toggles.show_world_axes = !toggles.show_world_axes;
        }
        "toggle_markers" => {
            toggles.show_prim_markers = !toggles.show_prim_markers;
        }
        "toggle_wireframe" => {
            toggles.wireframe = !toggles.wireframe;
        }
        "reload_stage" => {
            reload.requested = true;
        }
        "browse_usd" => {
            if let Some(picked) = rfd::FileDialog::new()
                .add_filter("USD stages", &["usda", "usdc", "usd", "usdz"])
                .pick_file()
            {
                load_req.path = Some(PathBuf::from(picked));
            }
        }
        _ => {}
    }
    palette.0.open = false;
}
