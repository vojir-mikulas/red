//! The read-only ER (entity-relationship) diagram (parity roadmap). A schema-wide,
//! pannable/zoomable overlay: every table is a box (name + columns, PK/FK marked)
//! and every foreign key is an orthogonal connector between two boxes.
//!
//! **Read-only, always.** It visualizes the schema; it never builds a query or a
//! join. Double-clicking a table opens a plain browse (the existing read path);
//! dragging a box only repositions it. See `docs/plans/er-diagram.md`.
//!
//! All the data is already resident after connect: table names live in
//! `active.schema.schemas`, columns in `active.schema.details` (eagerly prefetched),
//! and the relation graph in `active.fk_graph`. So opening the diagram costs no new
//! backend round-trip beyond topping up any missing table details.
//!
//! Rendering is deliberately div-only: boxes are absolutely-positioned divs and FK
//! edges are routed as axis-aligned (right-angle) segments, each a thin div. That
//! keeps the whole feature on well-trodden primitives; a curved-edge canvas layer
//! (gpui `paint_path`) is a drop-in swap later if wanted.
//!
//! World coordinates (box positions/sizes, pan) are plain `f32`; they're converted
//! to `Pixels` only at the div boundary, and screen positions are `world * zoom +
//! pan`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant};
use gpui::{
    canvas, div, prelude::*, px, AnyElement, Context, Hsla, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Point, ScrollDelta, ScrollWheelEvent,
};
use red_core::ObjectKind;

use crate::app::{ActiveConn, AppState, Phase};

// World-space box metrics (pre pan/zoom).
const NODE_W: f32 = 200.0;
const HEADER_H: f32 = 28.0;
const ROW_H: f32 = 18.0;
const PAD: f32 = 6.0;
/// Columns shown before collapsing the rest into a "+N more" row.
const MAX_ROWS: usize = 16;
/// Horizontal gap between layout layers, vertical gap between stacked boxes.
const H_GAP: f32 = 88.0;
const V_GAP: f32 = 30.0;
/// Below this zoom, boxes render as bare headers (columns would be unreadable).
const COLUMNS_MIN_ZOOM: f32 = 0.55;

/// A 2D point / vector in world (or screen) space.
#[derive(Clone, Copy, Default)]
struct Vec2 {
    x: f32,
    y: f32,
}

/// The viewport rectangle in window space, captured from the `canvas` overlay.
#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// One table box on the canvas. `pos`/`size` are world-space (before pan/zoom).
pub(crate) struct ErNode {
    pub schema: String,
    pub table: String,
    pos: Vec2,
    w: f32,
    h: f32,
    /// Column names (lowercased) that are foreign keys out of this table, for the
    /// per-row FK marker.
    fk_cols: HashSet<String>,
}

/// A resolved FK edge between two nodes (both endpoints exist in the diagram).
struct ErEdge {
    from: usize,
    to: usize,
}

/// A live pointer drag: either panning the canvas or moving one box.
enum Drag {
    Pan { last: Vec2 },
    Node { idx: usize, last: Vec2 },
}

/// The open ER diagram, hung off the connection (schema-wide). `None` when closed.
pub(crate) struct ErView {
    /// Pan offset in viewport-local pixels (added after world→screen scaling).
    pan: Vec2,
    zoom: f32,
    nodes: Vec<ErNode>,
    edges: Vec<ErEdge>,
    selected: Option<usize>,
    drag: Option<Drag>,
    /// Viewport rect (window space), captured each frame by a `canvas` overlay so
    /// zoom can anchor on the cursor / centre and Fit can measure.
    viewport: Rc<RefCell<Option<Rect>>>,
}

impl ErView {
    /// Build the diagram from the connection's resident schema + FK graph: create a
    /// box per table, resolve FK edges to node indices, and lay it out.
    fn build(active: &ActiveConn) -> Self {
        let mut nodes: Vec<ErNode> = Vec::new();
        for sc in &active.schema.schemas {
            for obj in &sc.objects {
                if obj.kind != ObjectKind::Table {
                    continue;
                }
                let ncols = active
                    .schema
                    .details
                    .get(&(sc.name.clone(), obj.name.clone()))
                    .map(|d| d.columns.len())
                    .unwrap_or(0);
                let rows = ncols.min(MAX_ROWS) + usize::from(ncols > MAX_ROWS);
                let h = HEADER_H + rows as f32 * ROW_H + PAD * 2.0;
                nodes.push(ErNode {
                    schema: sc.name.clone(),
                    table: obj.name.clone(),
                    pos: Vec2::default(),
                    w: NODE_W,
                    h,
                    fk_cols: HashSet::new(),
                });
            }
        }

        // Resolve a (schema, table) reference from an FK edge to a node index:
        // prefer an exact schema+name match, else fall back to a unique table name
        // (SQLite FK edges carry no schema).
        let mut by_exact: HashMap<(String, String), usize> = HashMap::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            by_exact.insert((n.schema.to_lowercase(), n.table.to_lowercase()), i);
            by_name.entry(n.table.to_lowercase()).or_default().push(i);
        }
        let resolve = |schema: &Option<String>, table: &str| -> Option<usize> {
            let t = table.to_lowercase();
            if let Some(s) = schema {
                if let Some(&i) = by_exact.get(&(s.to_lowercase(), t.clone())) {
                    return Some(i);
                }
            }
            match by_name.get(&t) {
                Some(v) if v.len() == 1 => Some(v[0]),
                _ => None,
            }
        };

        // `parents[i]` = tables that node i references (its FK targets); drives the
        // left→right layering (referenced tables sit to the left of their referrers).
        let mut parents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut edges: Vec<ErEdge> = Vec::new();
        for e in &active.fk_graph {
            let (Some(a), Some(b)) = (
                resolve(&e.from_schema, &e.from_table),
                resolve(&e.to_schema, &e.to_table),
            ) else {
                continue;
            };
            for (from_col, _) in &e.columns {
                nodes[a].fk_cols.insert(from_col.to_lowercase());
            }
            if a != b {
                parents[a].push(b);
                edges.push(ErEdge { from: a, to: b });
            }
        }

        layout(&mut nodes, &parents);

        Self {
            pan: Vec2 { x: 40., y: 40. },
            zoom: 1.0,
            nodes,
            edges,
            selected: None,
            drag: None,
            viewport: Rc::new(RefCell::new(None)),
        }
    }

    /// Rescale around `anchor` (viewport-local) so the world point under it stays put.
    fn zoom_at(&mut self, factor: f32, anchor: Vec2) {
        let old = self.zoom;
        let new = (old * factor).clamp(0.2, 2.5);
        let wx = (anchor.x - self.pan.x) / old;
        let wy = (anchor.y - self.pan.y) / old;
        self.pan.x = anchor.x - wx * new;
        self.pan.y = anchor.y - wy * new;
        self.zoom = new;
    }

    /// Viewport-local centre (falls back to origin before the first paint).
    fn center(&self) -> Vec2 {
        match *self.viewport.borrow() {
            Some(r) => Vec2 {
                x: r.w / 2.,
                y: r.h / 2.,
            },
            None => Vec2::default(),
        }
    }

    /// Fit every box into the viewport with a margin (zoom + centre).
    fn fit(&mut self) {
        let Some(vb) = *self.viewport.borrow() else {
            return;
        };
        if self.nodes.is_empty() {
            return;
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for n in &self.nodes {
            minx = minx.min(n.pos.x);
            miny = miny.min(n.pos.y);
            maxx = maxx.max(n.pos.x + n.w);
            maxy = maxy.max(n.pos.y + n.h);
        }
        let (bw, bh) = ((maxx - minx).max(1.), (maxy - miny).max(1.));
        let pad = 48.;
        let zx = (vb.w - 2. * pad) / bw;
        let zy = (vb.h - 2. * pad) / bh;
        let z = zx.min(zy).clamp(0.2, 1.5);
        self.zoom = z;
        let (wcx, wcy) = ((minx + maxx) / 2., (miny + maxy) / 2.);
        self.pan.x = vb.w / 2. - wcx * z;
        self.pan.y = vb.h / 2. - wcy * z;
    }
}

/// Longest-path layering by FK direction, then stack boxes per layer. Cycle-safe:
/// a back-edge in a cyclic schema contributes layer 0 rather than looping.
fn layout(nodes: &mut [ErNode], parents: &[Vec<usize>]) {
    let n = nodes.len();
    let mut layer = vec![0usize; n];
    let mut state = vec![0u8; n]; // 0 unseen, 1 in-progress, 2 done
    for i in 0..n {
        assign_layer(i, parents, &mut layer, &mut state);
    }
    // Stack nodes within each layer, in node order, top-down.
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut y_cursor = vec![0f32; max_layer + 1];
    for i in 0..n {
        let l = layer[i];
        nodes[i].pos = Vec2 {
            x: l as f32 * (NODE_W + H_GAP),
            y: y_cursor[l],
        };
        y_cursor[l] += nodes[i].h + V_GAP;
    }
}

fn assign_layer(i: usize, parents: &[Vec<usize>], layer: &mut [usize], state: &mut [u8]) -> usize {
    if state[i] == 2 {
        return layer[i];
    }
    if state[i] == 1 {
        return 0; // cycle: break here
    }
    state[i] = 1;
    let mut l = 0;
    for k in 0..parents[i].len() {
        let p = parents[i][k];
        l = l.max(assign_layer(p, parents, layer, state) + 1);
    }
    layer[i] = l;
    state[i] = 2;
    l
}

impl AppState {
    /// Open the read-only ER diagram overlay for the current connection. Tops up any
    /// missing table details first so the boxes can show columns.
    pub(crate) fn open_er_diagram(&mut self, cx: &mut Context<Self>) {
        if !matches!(&self.phase, Phase::Connected(_)) {
            return;
        }
        self.prefetch_table_details();
        if let Phase::Connected(active) = &mut self.phase {
            active.er = Some(ErView::build(active));
        }
        // Route Esc/focus through the shared modal handle (see `render_er`).
        self.focus_modal = true;
        cx.notify();
    }

    /// Close the ER diagram overlay.
    pub(crate) fn close_er_diagram(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.er = None;
        }
        cx.notify();
    }

    fn er_mut(&mut self) -> Option<&mut ErView> {
        match &mut self.phase {
            Phase::Connected(active) => active.er.as_mut(),
            _ => None,
        }
    }

    /// Zoom the diagram around its centre (the +/− buttons).
    fn er_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        if let Some(er) = self.er_mut() {
            let c = er.center();
            er.zoom_at(factor, c);
            cx.notify();
        }
    }

    /// Reset zoom to 100% around the centre.
    fn er_reset_zoom(&mut self, cx: &mut Context<Self>) {
        if let Some(er) = self.er_mut() {
            let c = er.center();
            let factor = 1.0 / er.zoom;
            er.zoom_at(factor, c);
            cx.notify();
        }
    }

    /// Fit the whole diagram into view.
    fn er_fit(&mut self, cx: &mut Context<Self>) {
        if let Some(er) = self.er_mut() {
            er.fit();
            cx.notify();
        }
    }

    /// Render the ER diagram overlay: a header (title · counts · zoom · close) over a
    /// pannable/zoomable canvas of boxes and FK connectors. `active` is the connection
    /// whose `er` is `Some` (the caller guarantees it).
    pub(crate) fn render_er(&self, active: &ActiveConn, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(er) = active.er.as_ref() else {
            return div().into_any_element();
        };
        let z = er.zoom;
        let pan = er.pan;
        let sx = move |wx: f32| wx * z + pan.x;
        let sy = move |wy: f32| wy * z + pan.y;

        // --- header chrome ---
        let title = format!("ER Diagram — {}", active.config.name);
        let counts = format!(
            "{} table{} · {} relation{}",
            er.nodes.len(),
            if er.nodes.len() == 1 { "" } else { "s" },
            er.edges.len(),
            if er.edges.len() == 1 { "" } else { "s" },
        );
        let pct = format!("{}%", (er.zoom * 100.).round() as i32);
        let header = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            // The overlay covers the whole window, so the header's top-left sits under
            // the macOS traffic lights; inset the left edge to clear them (same as the
            // main top bar).
            .pl(px(crate::shell::TITLEBAR_LEFT_INSET))
            .pr_3()
            .py_2()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_color(theme.text)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_muted)
                            .child(counts),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("er-zoom-out", "−")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.er_zoom(0.9, cx))),
                    )
                    .child(
                        Button::new("er-zoom-pct", pct)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.er_reset_zoom(cx))),
                    )
                    .child(
                        Button::new("er-zoom-in", "+")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.er_zoom(1.1, cx))),
                    )
                    .child(
                        Button::new("er-fit", "Fit")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.er_fit(cx))),
                    )
                    .child(
                        Button::new("er-close", "Close")
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.close_er_diagram(cx))),
                    ),
            );

        // --- edges (orthogonal connectors, drawn under the boxes) ---
        let edge_thickness = (1.5 * z).max(1.0);
        let mut edge_segments: Vec<AnyElement> = Vec::new();
        for e in &er.edges {
            let (a, b) = (&er.nodes[e.from], &er.nodes[e.to]);
            let (aw, ah) = (a.w * z, a.h * z);
            let (bw, bh) = (b.w * z, b.h * z);
            let (ax0, ay0) = (sx(a.pos.x), sy(a.pos.y));
            let (bx0, by0) = (sx(b.pos.x), sy(b.pos.y));
            let a_cy = ay0 + ah / 2.;
            let b_cy = by0 + bh / 2.;
            // Exit/enter on the sides that face each other.
            let (ax, bx) = if bx0 + bw / 2. >= ax0 + aw / 2. {
                (ax0 + aw, bx0) // A exits right, B enters left
            } else {
                (ax0, bx0 + bw) // A exits left, B enters right
            };
            let mid_x = (ax + bx) / 2.;
            let highlit = er.selected == Some(e.from) || er.selected == Some(e.to);
            let color = if highlit {
                theme.accent
            } else {
                theme.border_strong
            };
            // H segment from A, V segment across, H segment into B.
            edge_segments.push(h_seg(ax, mid_x, a_cy, edge_thickness, color));
            edge_segments.push(v_seg(mid_x, a_cy, b_cy, edge_thickness, color));
            edge_segments.push(h_seg(mid_x, bx, b_cy, edge_thickness, color));
        }

        // --- boxes ---
        let show_cols = z >= COLUMNS_MIN_ZOOM;
        let header_size = px((12.0 * z).clamp(8.0, 15.0));
        let row_size = px((11.0 * z).clamp(7.0, 14.0));
        let mut boxes: Vec<AnyElement> = Vec::new();
        for (i, node) in er.nodes.iter().enumerate() {
            let (left, top) = (sx(node.pos.x), sy(node.pos.y));
            let (w, h) = (node.w * z, node.h * z);
            let selected = er.selected == Some(i);
            let detail = active
                .schema
                .details
                .get(&(node.schema.clone(), node.table.clone()));

            let mut inner = div().flex().flex_col().size_full().child(
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .py_1()
                    .bg(theme.bg_panel_2)
                    .text_size(header_size)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .overflow_hidden()
                    .child(node.table.clone()),
            );

            if show_cols {
                if let Some(detail) = detail {
                    let mut col_list = div().flex().flex_col().px_1p5().py_0p5();
                    for col in detail.columns.iter().take(MAX_ROWS) {
                        let is_pk = col.primary_key;
                        let is_fk = node.fk_cols.contains(&col.name.to_lowercase());
                        let marker = if is_pk {
                            Some(("PK", theme.yellow))
                        } else if is_fk {
                            Some(("FK", theme.blue))
                        } else {
                            None
                        };
                        let name_color = if is_pk { theme.text } else { theme.text_muted };
                        let row = div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .h(px(ROW_H * z))
                            .text_size(row_size)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w(px(18. * z))
                                    .text_size(px((9.0 * z).clamp(6.0, 11.0)))
                                    .text_color(marker.map(|m| m.1).unwrap_or(theme.text_faint))
                                    .child(marker.map(|m| m.0).unwrap_or("").to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .text_color(name_color)
                                    .child(col.name.clone()),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(theme.text_faint)
                                    .child(col.type_name.clone().unwrap_or_default()),
                            );
                        col_list = col_list.child(row);
                    }
                    if detail.columns.len() > MAX_ROWS {
                        col_list = col_list.child(
                            div()
                                .h(px(ROW_H * z))
                                .text_size(row_size)
                                .text_color(theme.text_faint)
                                .child(format!("+{} more", detail.columns.len() - MAX_ROWS)),
                        );
                    }
                    inner = inner.child(col_list);
                } else {
                    inner = inner.child(
                        div()
                            .px_1p5()
                            .py_1()
                            .text_size(row_size)
                            .text_color(theme.text_faint)
                            .child("loading…"),
                    );
                }
            }

            let (schema_name, table_name) = (node.schema.clone(), node.table.clone());
            boxes.push(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(w))
                    .h(px(h))
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(if selected { theme.accent } else { theme.border })
                    .rounded(px(6.))
                    .shadow_sm()
                    .overflow_hidden()
                    .child(inner)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            // Double-click opens the table as a browse (read-only) and
                            // closes the diagram.
                            if ev.click_count >= 2 {
                                this.close_er_diagram(cx);
                                this.open_table_browse(
                                    schema_name.clone(),
                                    table_name.clone(),
                                    None,
                                    cx,
                                );
                                return;
                            }
                            if let Some(er) = this.er_mut() {
                                er.selected = Some(i);
                                er.drag = Some(Drag::Node {
                                    idx: i,
                                    last: pos_of(ev.position),
                                });
                                cx.notify();
                            }
                        }),
                    )
                    .into_any_element(),
            );
        }

        // --- viewport (pan/zoom surface) ---
        let vp_cell = er.viewport.clone();
        let viewport = div()
            .relative()
            .flex_1()
            .overflow_hidden()
            .bg(theme.bg_app)
            // Capture the viewport's window-space rect for cursor-anchored zoom / Fit.
            .child(
                canvas(
                    move |bounds, _, _| {
                        *vp_cell.borrow_mut() = Some(Rect {
                            x: f32::from(bounds.origin.x),
                            y: f32::from(bounds.origin.y),
                            w: f32::from(bounds.size.width),
                            h: f32::from(bounds.size.height),
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(edge_segments)
            .children(boxes)
            // Background press starts a pan; an empty-space press also clears selection.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    if let Some(er) = this.er_mut() {
                        er.selected = None;
                        er.drag = Some(Drag::Pan {
                            last: pos_of(ev.position),
                        });
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                let p = pos_of(ev.position);
                if let Some(er) = this.er_mut() {
                    match &mut er.drag {
                        Some(Drag::Pan { last }) => {
                            er.pan.x += p.x - last.x;
                            er.pan.y += p.y - last.y;
                            *last = p;
                            cx.notify();
                        }
                        Some(Drag::Node { idx, last }) => {
                            let (idx, dx, dy) =
                                (*idx, (p.x - last.x) / er.zoom, (p.y - last.y) / er.zoom);
                            *last = p;
                            if let Some(n) = er.nodes.get_mut(idx) {
                                n.pos.x += dx;
                                n.pos.y += dy;
                            }
                            cx.notify();
                        }
                        None => {}
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if let Some(er) = this.er_mut() {
                        if er.drag.take().is_some() {
                            cx.notify();
                        }
                    }
                }),
            )
            // Scroll zooms around the cursor (scroll up / away = zoom in); panning is
            // by dragging the background. Exponential so pixel-precise trackpad deltas
            // feel smooth and coarse mouse-wheel notches still move meaningfully.
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let p = pos_of(ev.position);
                if let Some(er) = this.er_mut() {
                    let dy = match ev.delta {
                        ScrollDelta::Pixels(d) => f32::from(d.y),
                        ScrollDelta::Lines(d) => d.y * 20.,
                    };
                    if dy == 0. {
                        return;
                    }
                    let anchor = match *er.viewport.borrow() {
                        Some(r) => Vec2 {
                            x: p.x - r.x,
                            y: p.y - r.y,
                        },
                        None => er.center(),
                    };
                    er.zoom_at(1.0015f32.powf(dy), anchor);
                    cx.notify();
                }
            }));

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .occlude()
            .track_focus(&self.modal_focus)
            .key_context("Modal")
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.close_er_diagram(cx);
                    cx.stop_propagation();
                }
            }))
            .child(header)
            .child(viewport)
            .into_any_element()
    }
}

/// Convert a window-space pointer position to an `f32` [`Vec2`].
fn pos_of(p: Point<gpui::Pixels>) -> Vec2 {
    Vec2 {
        x: f32::from(p.x),
        y: f32::from(p.y),
    }
}

/// A horizontal edge segment as a thin absolutely-positioned div.
fn h_seg(x1: f32, x2: f32, y: f32, thickness: f32, color: Hsla) -> AnyElement {
    let (left, w) = (x1.min(x2), (x1 - x2).abs());
    div()
        .absolute()
        .left(px(left))
        .top(px(y - thickness / 2.))
        .w(px(w))
        .h(px(thickness))
        .bg(color)
        .into_any_element()
}

/// A vertical edge segment as a thin absolutely-positioned div.
fn v_seg(x: f32, y1: f32, y2: f32, thickness: f32, color: Hsla) -> AnyElement {
    let (top, h) = (y1.min(y2), (y1 - y2).abs());
    div()
        .absolute()
        .left(px(x - thickness / 2.))
        .top(px(top))
        .w(px(thickness))
        .h(px(h))
        .bg(color)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layering is the longest reference-chain length: a referenced table sits one
    /// layer left of the table that references it.
    #[test]
    fn layering_is_longest_path() {
        // parents[i] = tables i references. 0 → 1 → 2.
        let parents = vec![vec![1], vec![2], vec![]];
        let mut layer = vec![0usize; 3];
        let mut state = vec![0u8; 3];
        for i in 0..3 {
            assign_layer(i, &parents, &mut layer, &mut state);
        }
        assert_eq!(layer, vec![2, 1, 0]);
    }

    /// A diamond (0 references 1 and 2; both reference 3) puts the shared parent at
    /// the deepest layer via the longest path.
    #[test]
    fn layering_takes_the_longest_of_two_paths() {
        let parents = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let mut layer = vec![0usize; 4];
        let mut state = vec![0u8; 4];
        for i in 0..4 {
            assign_layer(i, &parents, &mut layer, &mut state);
        }
        assert_eq!(layer[3], 0);
        assert_eq!(layer[1], 1);
        assert_eq!(layer[2], 1);
        assert_eq!(layer[0], 2);
    }

    /// A reference cycle terminates (a back-edge contributes layer 0) instead of
    /// recursing forever.
    #[test]
    fn layering_is_cycle_safe() {
        let parents = vec![vec![1], vec![0]];
        let mut layer = vec![0usize; 2];
        let mut state = vec![0u8; 2];
        for i in 0..2 {
            assign_layer(i, &parents, &mut layer, &mut state);
        }
        // The point is termination (no infinite recursion); layers stay bounded.
        assert!(layer.iter().all(|&l| l <= parents.len()));
    }
}
