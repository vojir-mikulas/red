//! The read-only ER (entity-relationship) diagram (parity roadmap): a pannable,
//! zoomable map of one database, where every table is a box (name + columns,
//! PK/FK marked) and every foreign key is a curved connector running from the
//! referring column's row to the row it references, marked with crow's-foot
//! cardinality (many at the child end, one at the parent end).
//!
//! **Read-only, always.** It visualizes the schema; it never builds a query or a
//! join. Double-clicking a table opens a plain browse (the existing read path);
//! dragging a box only repositions it. See `docs/plans/todo/er-diagram-tabs.md`.
//!
//! It lives in a **tab** (`QueryTab::er`), not an overlay, so several databases
//! can be mapped at once and a diagram stays put while you write SQL next to it.
//! An ER tab replaces its half entirely rather than sharing the slot with the
//! grid the way `QueryTab::plan` does: there is no query behind a diagram, so an
//! editor above it would be dead space.
//!
//! It draws **one namespace** (the database that was right-clicked), or the whole
//! connection when given `None`, which is the honest answer on SQLite and on
//! Postgres connections where every schema is in play. Drawing every database of
//! a MySQL server at once was both wrong (unrelated databases share no FK edges,
//! so the layering interleaves disconnected components into one column stack) and
//! ruinously slow.
//!
//! All the data is already resident after connect: table names live in
//! `active.schema.schemas`, columns in `active.schema.details` (eagerly prefetched),
//! and the relation graph in `active.fk_graph`. So opening the diagram costs no new
//! backend round-trip beyond topping up any missing table details.
//!
//! Boxes are absolutely-positioned divs; the connectors are painted into a single
//! `canvas` beneath them with `paint_path`. They started out as axis-aligned divs
//! (three per edge), which a dense schema turned into a thicket: dozens of straight
//! lines landing on the same few pixel rows overlap exactly, and there is nowhere
//! to hang a cardinality mark. Curves separate visually and carry their marks.
//!
//! **Only what is on screen is built.** `render_er` runs every frame, and
//! `cx.notify()` fires on each mouse-move of a pan, so building all N boxes
//! unconditionally made the one interaction a large schema needs (dragging the
//! canvas around) the one that rebuilt everything. Nodes and edges outside the
//! viewport are skipped, and below [`LABEL_MIN_ZOOM`] boxes paint as bare
//! rectangles, since text shaping dominates once the whole schema is fitted.
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
    AnyElement, Context, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PathBuilder, Pixels, Point, ScrollDelta, ScrollWheelEvent, Window, canvas, div, prelude::*, px,
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
/// Gap between two independent component blocks packed onto the canvas.
const COMPONENT_GAP: f32 = 120.0;
/// Width:height the packed canvas aims for. Without it the layout is one tall
/// ribbon, and `Fit` has to scale it down so far that nothing is legible.
const TARGET_ASPECT: f32 = 16.0 / 9.0;
/// Barycentre ordering sweeps. Crossings fall steeply over the first few passes
/// and then plateau, so more than this is spent work.
const ORDER_PASSES: usize = 4;
/// Alignment passes over the y coordinates once the ordering is fixed. Each pulls a
/// box towards the boxes it connects to, which is what makes a connector run
/// straight across instead of doglegging.
const STRAIGHTEN_PASSES: usize = 4;
/// Gap between a column row's marker / name / type, in world units. Scaled by zoom
/// at the div boundary like every other measurement, so a box looks the same at any
/// zoom rather than filling with padding as it shrinks.
const COL_GAP: f32 = 4.0;
/// Crow's-foot geometry in world units: how far the foot's tip sits from the box,
/// where the "one" bar crosses the connector, and how far the marks spread.
const FOOT_LEN: f32 = 11.0;
const BAR_OFFSET: f32 = 9.0;
const MARK_SPREAD: f32 = 5.0;
/// Below this zoom, boxes render as bare headers (columns would be unreadable).
const COLUMNS_MIN_ZOOM: f32 = 0.55;
/// Below this zoom, boxes render as bare rectangles with no text at all. Fitting a
/// few hundred tables lands around 0.2, where a table name is a smear anyway and
/// shaping one string per box is the dominant cost.
const LABEL_MIN_ZOOM: f32 = 0.3;
/// Culling margin (screen px) around the viewport, covering the one-frame staleness
/// of the captured rect and half-visible boxes at the edges.
const CULL_MARGIN: f32 = 200.0;

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
/// `from` is the referring (child, "many") side, `to` the referenced (parent,
/// "one") side — the direction the crow's-foot marks read from.
struct ErEdge {
    from: usize,
    to: usize,
    /// The referring column in `from` and the referenced column in `to`, lowercased.
    /// The connector anchors on *these rows* rather than the box centres, which is
    /// the difference between "these tables are related somehow" and "this column
    /// points at that one". A composite FK anchors on its first pair; the rest of
    /// the pairs join the same two boxes.
    from_col: String,
    to_col: String,
}

/// A live pointer drag: either panning the canvas or moving one box.
enum Drag {
    Pan { last: Vec2 },
    Node { idx: usize, last: Vec2 },
}

/// The ER diagram held by one tab (see [`crate::app::QueryTab::er`]).
pub(crate) struct ErView {
    /// The namespace this diagram maps, or `None` for the whole connection. Kept so
    /// reopening the same database focuses this tab instead of building a second.
    pub namespace: Option<String>,
    /// Pan offset in viewport-local pixels (added after world→screen scaling).
    pan: Vec2,
    zoom: f32,
    nodes: Vec<ErNode>,
    edges: Vec<ErEdge>,
    /// How the boxes are ordered into components and layers. Kept because it
    /// survives a height change, so [`ErView::remeasure`] can re-flow without
    /// redoing the graph walk or the crossing-reduction sweeps.
    plan: LayoutPlan,
    /// Undirected neighbours per node, kept alongside `plan` because the alignment
    /// pass in [`position`] needs them on every re-flow.
    adj: Vec<Vec<usize>>,
    selected: Option<usize>,
    drag: Option<Drag>,
    /// Set once the user drags a box: after that the automatic layout stops
    /// re-running (it would yank their arrangement back), so late-arriving column
    /// details only resize boxes in place. See [`ErView::remeasure`].
    hand_placed: bool,
    /// Tables whose `DescribeTable` this view has already requested, so scrolling
    /// back over the same region doesn't re-ask every frame. See
    /// [`ErView::missing_details`].
    requested: RefCell<HashSet<(String, String)>>,
    /// Viewport rect (window space), captured each frame by a `canvas` overlay so
    /// zoom can anchor on the cursor / centre and Fit can measure.
    viewport: Rc<RefCell<Option<Rect>>>,
}

impl ErView {
    /// Build the diagram from the connection's resident schema + FK graph: create a
    /// box per table in `namespace` (or in every schema when `None`), resolve FK
    /// edges to node indices, and lay it out.
    fn build(active: &ActiveConn, namespace: Option<String>) -> Self {
        let mut nodes: Vec<ErNode> = Vec::new();
        for sc in &active.schema.schemas {
            if !in_namespace(&sc.name, namespace.as_deref()) {
                continue;
            }
            for obj in &sc.objects {
                if obj.kind != ObjectKind::Table {
                    continue;
                }
                let ncols = active
                    .schema
                    .details
                    .get(&(sc.name.clone(), obj.name.clone()))
                    .map(|d| d.columns.len());
                nodes.push(ErNode {
                    schema: sc.name.clone(),
                    table: obj.name.clone(),
                    pos: Vec2::default(),
                    w: NODE_W,
                    h: node_height(ncols),
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
            if let Some(s) = schema
                && let Some(&i) = by_exact.get(&(s.to_lowercase(), t.clone()))
            {
                return Some(i);
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
                let (from_col, to_col) = e.columns.first().cloned().unwrap_or_default();
                edges.push(ErEdge {
                    from: a,
                    to: b,
                    from_col: from_col.to_lowercase(),
                    to_col: to_col.to_lowercase(),
                });
            }
        }

        // Undirected adjacency: the component split and the barycentre ordering both
        // care that two tables are related, not which way the FK points.
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for e in &edges {
            adj[e.from].push(e.to);
            adj[e.to].push(e.from);
        }
        let plan = plan_layout(nodes.len(), &parents, &adj);
        position(&mut nodes, &plan, &adj);

        Self {
            namespace,
            pan: Vec2 { x: 40., y: 40. },
            zoom: 1.0,
            nodes,
            edges,
            plan,
            adj,
            selected: None,
            drag: None,
            hand_placed: false,
            requested: RefCell::new(HashSet::new()),
            viewport: Rc::new(RefCell::new(None)),
        }
    }

    /// Adopt a table's freshly-arrived column count: resize its box and re-stack, so
    /// the layout stops depending on which details happened to be resident when the
    /// diagram was opened. A user who has dragged a box keeps their arrangement (the
    /// box still resizes, it just isn't moved). Returns whether anything changed, so
    /// the caller can skip a repaint.
    pub(crate) fn remeasure(&mut self, schema: &str, table: &str, ncols: usize) -> bool {
        let h = node_height(Some(ncols));
        let Some(node) = self
            .nodes
            .iter_mut()
            .find(|n| n.schema == schema && n.table == table)
        else {
            return false;
        };
        if node.h == h {
            return false;
        }
        node.h = h;
        if !self.hand_placed {
            position(&mut self.nodes, &self.plan, &self.adj);
        }
        true
    }

    /// Tables on screen whose columns aren't resident yet, so the caller can ask for
    /// just those rather than pre-describing a whole large schema. Records what it
    /// hands back, so a later frame over the same region asks once, not every frame.
    ///
    /// Empty below [`COLUMNS_MIN_ZOOM`]: those boxes don't draw columns, so fetching
    /// them would buy nothing.
    fn missing_details(
        &self,
        details: &HashMap<(String, String), red_core::TableDetail>,
    ) -> Vec<(String, String)> {
        if self.zoom < COLUMNS_MIN_ZOOM {
            return Vec::new();
        }
        let Some(vp) = *self.viewport.borrow() else {
            return Vec::new();
        };
        let mut requested = self.requested.borrow_mut();
        let mut out = Vec::new();
        for node in &self.nodes {
            let key = (node.schema.clone(), node.table.clone());
            if details.contains_key(&key) || requested.contains(&key) {
                continue;
            }
            if !self.on_screen(node, &vp) {
                continue;
            }
            requested.insert(key.clone());
            out.push(key);
        }
        out
    }

    /// Whether `node`'s screen-space box intersects the viewport, with
    /// [`CULL_MARGIN`] of slack for the rect's one-frame staleness.
    fn on_screen(&self, node: &ErNode, vp: &Rect) -> bool {
        let (z, pan) = (self.zoom, self.pan);
        let (x0, y0) = (node.pos.x * z + pan.x, node.pos.y * z + pan.y);
        rect_visible(x0, y0, node.w * z, node.h * z, vp)
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
        // The floor is low on purpose: clamped at 0.2 a few-hundred-table schema
        // simply didn't fit, and "Fit" left most of it off screen. Far out the boxes
        // are bare rectangles (see [`LABEL_MIN_ZOOM`]), which is the point — you're
        // reading the shape of the schema, then zooming into a region.
        let z = zx.min(zy).clamp(0.05, 1.5);
        self.zoom = z;
        let (wcx, wcy) = ((minx + maxx) / 2., (miny + maxy) / 2.);
        self.pan.x = vb.w / 2. - wcx * z;
        self.pan.y = vb.h / 2. - wcy * z;
    }
}

/// Whether schema `name` belongs in a diagram scoped to `namespace`. `None` means
/// the whole connection, so every schema is in. Compared exactly, not case-folded:
/// the name comes from the same schema list the tree renders, so it round-trips.
fn in_namespace(name: &str, namespace: Option<&str>) -> bool {
    namespace.is_none_or(|ns| name == ns)
}

/// Where a connector meets a box, as a world-space offset from the box's top: the
/// vertical centre of `col`'s row when that row is actually drawn, else the box's
/// own centre.
///
/// The row geometry here has to agree exactly with what `render_er` lays out —
/// header band, then [`PAD`], then one [`ROW_H`] per column — or the line would
/// point next to its column instead of at it. That's why the header renders at a
/// fixed height rather than sizing to its text.
fn anchor_offset(node: &ErNode, col: &str, detail: Option<&red_core::TableDetail>) -> f32 {
    if let Some(detail) = detail
        && let Some(idx) = detail
            .columns
            .iter()
            .take(MAX_ROWS)
            .position(|c| c.name.to_lowercase() == col)
    {
        return HEADER_H + PAD + idx as f32 * ROW_H + ROW_H / 2.0;
    }
    node.h / 2.0
}

/// One connector, resolved to canvas-local screen coordinates and ready to paint.
/// `a` is the child ("many") end, `b` the parent ("one") end; `*_dir` is +1 when
/// the line leaves that box to the right, -1 to the left.
struct EdgePaint {
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    a_dir: f32,
    b_dir: f32,
    color: Hsla,
}

/// Paint one connector: a cubic with horizontal tangents from the child's FK row to
/// the parent's referenced row, a crow's foot at the child end and a single bar at
/// the parent end, so the line reads as "many of these, one of those".
///
/// Curves rather than the old right-angle segments because a dense schema drew
/// dozens of axis-aligned lines onto the same few pixel rows, which is what made
/// the diagram unreadable: parallel straights overlap exactly, curves don't.
fn paint_edge(
    window: &mut Window,
    origin: Point<Pixels>,
    e: &EdgePaint,
    thickness: Pixels,
    z: f32,
    marks: bool,
) {
    let pt = |x: f32, y: f32| Point {
        x: origin.x + px(x),
        y: origin.y + px(y),
    };
    let (foot, bar, spread) = (FOOT_LEN * z, BAR_OFFSET * z, MARK_SPREAD * z);
    // With marks on, the curve spans tip-to-bar so it doesn't run through them.
    let (sx, ex) = if marks {
        (e.ax + e.a_dir * foot, e.bx + e.b_dir * bar)
    } else {
        (e.ax, e.bx)
    };
    let bend = ((ex - sx).abs() * 0.5).clamp(24.0 * z, 180.0 * z);
    let mut pb = PathBuilder::stroke(thickness);
    pb.move_to(pt(sx, e.ay));
    pb.cubic_bezier_to(
        pt(ex, e.by),
        pt(sx + e.a_dir * bend, e.ay),
        pt(ex + e.b_dir * bend, e.by),
    );
    if let Ok(path) = pb.build() {
        window.paint_path(path, e.color);
    }
    if !marks {
        return;
    }

    // Crow's foot: three prongs from the tip back onto the child's box edge.
    let mut foot_pb = PathBuilder::stroke(thickness);
    for dy in [-spread, 0.0, spread] {
        foot_pb.move_to(pt(e.ax + e.a_dir * foot, e.ay));
        foot_pb.line_to(pt(e.ax, e.ay + dy));
    }
    if let Ok(path) = foot_pb.build() {
        window.paint_path(path, e.color);
    }

    // "Exactly one" at the parent end: a single bar across the connector.
    let mut bar_pb = PathBuilder::stroke(thickness);
    bar_pb.move_to(pt(e.bx + e.b_dir * bar, e.by - spread));
    bar_pb.line_to(pt(e.bx + e.b_dir * bar, e.by + spread));
    if let Ok(path) = bar_pb.build() {
        window.paint_path(path, e.color);
    }
}

/// A box's world height for `ncols` columns, or the bare-header height when the
/// table's columns aren't resident yet (`None`). Shared by `build` and
/// [`ErView::remeasure`] so a late arrival lands on the same number.
fn node_height(ncols: Option<usize>) -> f32 {
    let ncols = ncols.unwrap_or(0);
    let rows = ncols.min(MAX_ROWS) + usize::from(ncols > MAX_ROWS);
    HEADER_H + rows as f32 * ROW_H + PAD * 2.0
}

/// Longest-path layering by FK direction: `layer[i]` is node `i`'s column, with
/// referenced tables to the left of their referrers. Cycle-safe: a back-edge in a
/// cyclic schema contributes layer 0 rather than looping.
fn assign_layers(n: usize, parents: &[Vec<usize>]) -> Vec<usize> {
    let mut layer = vec![0usize; n];
    let mut state = vec![0u8; n]; // 0 unseen, 1 in-progress, 2 done
    for i in 0..n {
        assign_layer(i, parents, &mut layer, &mut state);
    }
    layer
}

/// Split the graph into connected components, treating FK edges as undirected.
///
/// Unrelated tables share no edges, so layering them together is meaningless: the
/// old single-stack layout dropped every FK-less table into layer 0, which on a
/// real schema is most of them, producing one column thousands of pixels tall with
/// unrelated islands interleaved through it.
fn components(n: usize, adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    for start in 0..n {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut comp = Vec::new();
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            comp.push(i);
            for &j in &adj[i] {
                if !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        comp.sort_unstable();
        out.push(comp);
    }
    out
}

/// Reorder each layer by the barycentre (mean slot) of a node's neighbours in the
/// adjacent layer, sweeping down and back up. This is the step that turns FK
/// connectors from a thicket into something followable: neighbours end up beside
/// each other, so edges run mostly straight across instead of criss-crossing the
/// full height of a layer. A node with no neighbour in the fixed layer keeps its
/// slot, so it doesn't get flung to the top.
fn order_layers(layers: &mut [Vec<usize>], adj: &[Vec<usize>]) {
    if layers.len() < 2 {
        return;
    }
    let mut slot: HashMap<usize, f32> = HashMap::new();
    refresh_slots(layers, &mut slot);
    for pass in 0..ORDER_PASSES {
        let down = pass % 2 == 0;
        let sweep: Vec<usize> = if down {
            (1..layers.len()).collect()
        } else {
            (0..layers.len() - 1).rev().collect()
        };
        for l in sweep {
            let fixed: HashSet<usize> = layers[if down { l - 1 } else { l + 1 }]
                .iter()
                .copied()
                .collect();
            let mut keyed: Vec<(f32, usize)> = layers[l]
                .iter()
                .enumerate()
                .map(|(k, &i)| {
                    let (mut sum, mut count) = (0f32, 0f32);
                    for &j in &adj[i] {
                        if fixed.contains(&j) {
                            sum += slot.get(&j).copied().unwrap_or(0.);
                            count += 1.;
                        }
                    }
                    let key = if count > 0. { sum / count } else { k as f32 };
                    (key, i)
                })
                .collect();
            // Stable, so equal barycentres keep the order the previous sweep chose.
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            layers[l] = keyed.into_iter().map(|(_, i)| i).collect();
            refresh_slots(layers, &mut slot);
        }
    }
}

/// Record each node's index within its layer, the input to the next barycentre pass.
fn refresh_slots(layers: &[Vec<usize>], slot: &mut HashMap<usize, f32>) {
    slot.clear();
    for layer in layers {
        for (k, &i) in layer.iter().enumerate() {
            slot.insert(i, k as f32);
        }
    }
}

/// The ordering the layout settled on: `plan[c][l]` is component `c`'s layer `l`,
/// listing node indices top to bottom.
///
/// Positions are *derived* from it by [`position`] rather than stored, so a
/// late-arriving column count re-flows the diagram ([`ErView::remeasure`]) without
/// redoing the component split and the crossing-reduction sweeps.
type LayoutPlan = Vec<Vec<Vec<usize>>>;

/// Layer the graph, split it into components, and order each component's layers to
/// cut edge crossings. Components come out biggest-first, so the schema's main
/// structure packs into the top-left where the eye lands.
fn plan_layout(n: usize, parents: &[Vec<usize>], adj: &[Vec<usize>]) -> LayoutPlan {
    let layer = assign_layers(n, parents);
    let mut comps = components(n, adj);
    comps.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    let mut plan = Vec::with_capacity(comps.len());
    for comp in comps {
        // Re-base to column 0: a component whose shallowest node sits at layer 3
        // would otherwise carry three empty columns of dead space.
        let base = comp.iter().map(|&i| layer[i]).min().unwrap_or(0);
        let depth = comp.iter().map(|&i| layer[i] - base).max().unwrap_or(0);
        let mut layers = vec![Vec::new(); depth + 1];
        for &i in &comp {
            layers[layer[i] - base].push(i);
        }
        order_layers(&mut layers, adj);
        plan.push(layers);
    }
    plan
}

/// Lay one connected block out in its own coordinate space: layers become columns,
/// each column is centred on the block's midline rather than hanging from the top,
/// and then [`straighten`] pulls boxes towards the boxes they connect to.
fn layout_block(nodes: &mut [ErNode], comp: &[Vec<usize>], adj: &[Vec<usize>]) {
    let column_span = |nodes: &[ErNode], layer: &[usize]| -> f32 {
        let total: f32 = layer.iter().map(|&i| nodes[i].h + V_GAP).sum();
        (total - V_GAP).max(0.)
    };
    let tallest = comp
        .iter()
        .map(|layer| column_span(nodes, layer))
        .fold(0f32, f32::max);
    for (l, layer) in comp.iter().enumerate() {
        let x = l as f32 * (NODE_W + H_GAP);
        // Centre the column: a two-box layer beside a ten-box one used to sit at the
        // top with a long diagonal reaching down to it.
        let mut y = (tallest - column_span(nodes, layer)) / 2.0;
        for &i in layer {
            nodes[i].pos = Vec2 { x, y };
            y += nodes[i].h + V_GAP;
        }
    }
    straighten(nodes, comp, adj);
    // Re-base to the block's own origin so packing can place it anywhere.
    let top = comp
        .iter()
        .flatten()
        .map(|&i| nodes[i].pos.y)
        .fold(f32::MAX, f32::min);
    if top.is_finite() {
        for &i in comp.iter().flatten() {
            nodes[i].pos.y -= top;
        }
    }
}

/// Pull each box towards the mean of the boxes it connects to in the neighbouring
/// column, sweeping right then left. The pass walks a column in its fixed order and
/// never lets a box rise above the one before it, so the crossing-reduction ordering
/// survives while the boxes slide into line.
fn straighten(nodes: &mut [ErNode], comp: &[Vec<usize>], adj: &[Vec<usize>]) {
    if comp.len() < 2 {
        return;
    }
    for pass in 0..STRAIGHTEN_PASSES {
        let down = pass % 2 == 0;
        let sweep: Vec<usize> = if down {
            (1..comp.len()).collect()
        } else {
            (0..comp.len() - 1).rev().collect()
        };
        for l in sweep {
            let fixed: HashSet<usize> = comp[if down { l - 1 } else { l + 1 }]
                .iter()
                .copied()
                .collect();
            // Where each box would like to sit: level with its neighbours' centres.
            let wanted: Vec<f32> = comp[l]
                .iter()
                .map(|&i| {
                    let (mut sum, mut count) = (0f32, 0f32);
                    for &j in &adj[i] {
                        if fixed.contains(&j) {
                            sum += nodes[j].pos.y + nodes[j].h / 2.0;
                            count += 1.0;
                        }
                    }
                    if count > 0.0 {
                        sum / count - nodes[i].h / 2.0
                    } else {
                        nodes[i].pos.y
                    }
                })
                .collect();
            let mut floor = f32::MIN;
            for (k, &i) in comp[l].iter().enumerate() {
                let y = wanted[k].max(floor);
                nodes[i].pos.y = y;
                floor = y + nodes[i].h + V_GAP;
            }
        }
    }
}

/// A laid-out block's extent from its own origin.
fn block_size(nodes: &[ErNode], comp: &[Vec<usize>]) -> (f32, f32) {
    let (mut w, mut h) = (0f32, 0f32);
    for &i in comp.iter().flatten() {
        w = w.max(nodes[i].pos.x + nodes[i].w);
        h = h.max(nodes[i].pos.y + nodes[i].h);
    }
    (w, h)
}

/// Place every box from `plan` and the current heights. Related tables are laid out
/// as blocks and packed into rows at a target width so the canvas ends up roughly
/// screen-shaped; standalone tables go into a tidy grid underneath. Re-run whenever
/// a height changes ([`ErView::remeasure`]).
fn position(nodes: &mut [ErNode], plan: &LayoutPlan, adj: &[Vec<usize>]) {
    let mut blocks: Vec<(usize, f32, f32)> = Vec::new();
    let mut islands: Vec<usize> = Vec::new();
    for (c, comp) in plan.iter().enumerate() {
        // A table with no FK at all is not a one-table "diagram"; it's an entry in a
        // list. Packing it as a block left a [`COMPONENT_GAP`] moat around every one
        // of them, and on a real schema they're the majority.
        if comp.iter().map(Vec::len).sum::<usize>() == 1 {
            islands.push(comp[0][0]);
            continue;
        }
        layout_block(nodes, comp, adj);
        let (w, h) = block_size(nodes, comp);
        blocks.push((c, w, h));
    }

    // The target width comes from the total area, so a schema of any size lands near
    // [`TARGET_ASPECT`] rather than growing in one direction only. It counts each
    // item's gap, not just the item: a schema that's mostly small blocks or islands
    // is mostly gap, and measuring the bare boxes made the target far too narrow —
    // which packed everything into the tall ribbon this exists to avoid.
    let block_area: f32 = blocks
        .iter()
        .map(|(_, w, h)| (w + COMPONENT_GAP) * (h + COMPONENT_GAP))
        .sum();
    let island_area: f32 = islands
        .iter()
        .map(|&i| (NODE_W + H_GAP) * (nodes[i].h + V_GAP))
        .sum();
    let widest = blocks.iter().map(|(_, w, _)| *w).fold(NODE_W, f32::max);
    let target_w = ((block_area + island_area) * TARGET_ASPECT)
        .sqrt()
        .max(widest);

    let (mut ox, mut oy, mut row_h) = (0f32, 0f32, 0f32);
    for &(c, w, h) in &blocks {
        if ox > 0. && ox + w > target_w {
            ox = 0.;
            oy += row_h + COMPONENT_GAP;
            row_h = 0.;
        }
        for &i in plan[c].iter().flatten() {
            nodes[i].pos.x += ox;
            nodes[i].pos.y += oy;
        }
        ox += w + COMPONENT_GAP;
        row_h = row_h.max(h);
    }

    if islands.is_empty() {
        return;
    }
    // Standalone tables last, in a grid as wide as the blocks above it. Sorted by
    // node index, which is the schema tree's own order, so the grid reads
    // alphabetically instead of in whatever order the component walk emitted.
    islands.sort_unstable();
    let below = if blocks.is_empty() {
        0.
    } else {
        oy + row_h + COMPONENT_GAP
    };
    let cols = ((target_w / (NODE_W + H_GAP)).floor() as usize).max(1);
    let (mut y, mut row_h) = (below, 0f32);
    for (k, &i) in islands.iter().enumerate() {
        let col = k % cols;
        if col == 0 && k > 0 {
            y += row_h + V_GAP;
            row_h = 0.;
        }
        nodes[i].pos = Vec2 {
            x: col as f32 * (NODE_W + H_GAP),
            y,
        };
        row_h = row_h.max(nodes[i].h);
    }
}

/// Whether a screen-space box intersects `vp`, inflated by [`CULL_MARGIN`].
fn rect_visible(x: f32, y: f32, w: f32, h: f32, vp: &Rect) -> bool {
    x + w >= -CULL_MARGIN
        && y + h >= -CULL_MARGIN
        && x <= vp.w + CULL_MARGIN
        && y <= vp.h + CULL_MARGIN
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
    /// Open an ER diagram tab for `namespace` (the right-clicked database), or for
    /// the whole connection when `None`.
    ///
    /// A database that already has a diagram open **focuses** that tab rather than
    /// building a second: the view carries hand-dragged box positions and a pan/zoom
    /// the user chose, and silently discarding those on a second right-click would be
    /// the worse surprise.
    pub(crate) fn open_er_diagram(&mut self, namespace: Option<String>, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        if let Some(i) = active
            .tabs
            .iter()
            .position(|t| t.er.as_ref().is_some_and(|er| er.namespace == namespace))
        {
            self.set_active_tab(i, cx);
            return;
        }

        let title = match &namespace {
            Some(ns) => format!("ER: {ns}"),
            None => format!("ER: {}", active.config.name),
        };
        let mut tab = crate::app::QueryTab::new(title, cx);
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        tab.er = Some(ErView::build(active, namespace));
        self.push_tab(tab, cx);
        cx.notify();
    }

    /// Which database the keybinding / menu / palette entry should map, in
    /// decreasing order of "what the user is visibly looking at": the schema tree's
    /// selected node, then the connection's active namespace, then the sole schema
    /// if there is exactly one.
    ///
    /// `None` means the whole connection, which is right for SQLite (one schema
    /// anyway) and for a Postgres connection where the user hasn't singled one out.
    /// Unlike the tree's right-click item, these entry points carry no database with
    /// them, so they have to infer one or draw everything.
    pub(crate) fn er_target_namespace(&self) -> Option<String> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let from_tree = active.schema.selected.as_ref().map(|node| match node {
            crate::schema::NodeId::Schema(s) => s.clone(),
            crate::schema::NodeId::Object { schema, .. } => schema.clone(),
            crate::schema::NodeId::Column { schema, .. } => schema.clone(),
        });
        from_tree.or_else(|| active.namespace.clone()).or_else(|| {
            match active.schema.schemas.as_slice() {
                [only] => Some(only.name.clone()),
                _ => None,
            }
        })
    }

    /// The ER view held by tab `tab_idx`, if that tab is a diagram.
    fn er_mut(&mut self, tab_idx: usize) -> Option<&mut ErView> {
        match &mut self.phase {
            Phase::Connected(active) => active.tabs.get_mut(tab_idx)?.er.as_mut(),
            _ => None,
        }
    }

    /// Resize a box and re-stack the diagrams that contain `schema.table`, once its
    /// columns land. Without this the layout keeps the spacing it was built with,
    /// which on a schema larger than the detail prefetch cap means every box past the
    /// cap is one header tall and its columns clip when they finally arrive.
    pub(crate) fn er_table_described(&mut self, schema: &str, table: &str, ncols: usize) {
        if let Phase::Connected(active) = &mut self.phase {
            for tab in &mut active.tabs {
                if let Some(er) = tab.er.as_mut() {
                    er.remeasure(schema, table, ncols);
                }
            }
        }
    }

    /// Zoom the diagram around its centre (the +/− buttons).
    fn er_zoom(&mut self, tab_idx: usize, factor: f32, cx: &mut Context<Self>) {
        if let Some(er) = self.er_mut(tab_idx) {
            let c = er.center();
            er.zoom_at(factor, c);
            cx.notify();
        }
    }

    /// Reset zoom to 100% around the centre.
    fn er_reset_zoom(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        if let Some(er) = self.er_mut(tab_idx) {
            let c = er.center();
            let factor = 1.0 / er.zoom;
            er.zoom_at(factor, c);
            cx.notify();
        }
    }

    /// Fit the whole diagram into view.
    fn er_fit(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        if let Some(er) = self.er_mut(tab_idx) {
            er.fit();
            cx.notify();
        }
    }

    /// Render the ER diagram overlay: a header (title · counts · zoom · close) over a
    /// pannable/zoomable canvas of boxes and FK connectors. `active` is the connection
    /// whose `er` is `Some` (the caller guarantees it).
    pub(crate) fn render_er(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let Some(er) = active.tabs.get(tab_idx).and_then(|t| t.er.as_ref()) else {
            return div().into_any_element();
        };
        let z = er.zoom;
        let pan = er.pan;
        let sx = move |wx: f32| wx * z + pan.x;
        let sy = move |wy: f32| wy * z + pan.y;
        // One frame stale (the `canvas` below captures it during paint), which is
        // what `CULL_MARGIN` absorbs. `None` on the very first frame: draw everything
        // once, then cull from the second frame on.
        let vp = *er.viewport.borrow();

        // --- toolbar ---
        // The tab strip already says which database this is and carries the close
        // button, so the bar is only the counts and the zoom controls.
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
            .px_2()
            .py_1()
            .bg(theme.bg_panel)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(theme.scale(11.))
                    .text_color(theme.text_muted)
                    .child(counts),
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
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.er_zoom(tab_idx, 0.9, cx)),
                            ),
                    )
                    .child(
                        Button::new("er-zoom-pct", pct)
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.er_reset_zoom(tab_idx, cx)),
                            ),
                    )
                    .child(
                        Button::new("er-zoom-in", "+")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.er_zoom(tab_idx, 1.1, cx)),
                            ),
                    )
                    .child(
                        Button::new("er-fit", "Fit")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(move |this, _, _, cx| this.er_fit(tab_idx, cx))),
                    ),
            );

        // --- edges (curved, column-anchored, crow's-foot; painted under the boxes) ---
        let show_cols = z >= COLUMNS_MIN_ZOOM;
        let edge_thickness = px((1.4 * z).max(0.75));
        // Below the label tier the marks would be a few sub-pixel smudges, so the
        // connectors go back to plain lines and only the shape of the graph reads.
        let show_marks = z >= LABEL_MIN_ZOOM;
        let mut edge_paints: Vec<EdgePaint> = Vec::new();
        for e in &er.edges {
            let (a, b) = (&er.nodes[e.from], &er.nodes[e.to]);
            let (aw, bw) = (a.w * z, b.w * z);
            let (ax0, ay0) = (sx(a.pos.x), sy(a.pos.y));
            let (bx0, by0) = (sx(b.pos.x), sy(b.pos.y));
            // Anchor on the FK column and the column it references, but only while
            // the rows are actually drawn; below that tier the box centre is the
            // only honest anchor.
            let (a_off, b_off) = if show_cols {
                let key_a = (a.schema.clone(), a.table.clone());
                let key_b = (b.schema.clone(), b.table.clone());
                (
                    anchor_offset(a, &e.from_col, active.schema.details.get(&key_a)),
                    anchor_offset(b, &e.to_col, active.schema.details.get(&key_b)),
                )
            } else {
                (a.h / 2., b.h / 2.)
            };
            let (a_y, b_y) = (ay0 + a_off * z, by0 + b_off * z);
            // Leave from the sides that face each other.
            let (a_x, b_x, a_dir, b_dir) = if bx0 + bw / 2. >= ax0 + aw / 2. {
                (ax0 + aw, bx0, 1.0, -1.0)
            } else {
                (ax0, bx0 + bw, -1.0, 1.0)
            };
            // Cull by the connector's bounding box, widened by the bend so a curve
            // that bows into view from two off-screen boxes still paints.
            if let Some(vp) = &vp {
                let slack = 180.0 * z;
                let (x0, x1) = (a_x.min(b_x) - slack, a_x.max(b_x) + slack);
                let (y0, y1) = (a_y.min(b_y), a_y.max(b_y));
                if !rect_visible(x0, y0, x1 - x0, y1 - y0, vp) {
                    continue;
                }
            }
            let highlit = er.selected == Some(e.from) || er.selected == Some(e.to);
            edge_paints.push(EdgePaint {
                ax: a_x,
                ay: a_y,
                bx: b_x,
                by: b_y,
                a_dir,
                b_dir,
                color: if highlit {
                    theme.accent
                } else {
                    theme.border_strong
                },
            });
        }

        // --- boxes ---
        let show_label = z >= LABEL_MIN_ZOOM;
        // Text scales with the boxes, unclamped. Clamping it looked like the diagram
        // was zooming in two parts: past the clamp the boxes kept scaling while the
        // text stopped, so glyphs crept out of their rows going down and shrank
        // relative to the box going up. The LOD tiers below already handle
        // "too small to read" by dropping the text entirely.
        let header_size = px(12.0 * z);
        let row_size = px(11.0 * z);
        // Padding in world units too, so a box holds its proportions at any zoom.
        let pad = px(PAD * z);
        let mut boxes: Vec<AnyElement> = Vec::new();
        for (i, node) in er.nodes.iter().enumerate() {
            if vp.as_ref().is_some_and(|vp| !er.on_screen(node, vp)) {
                continue;
            }
            let (left, top) = (sx(node.pos.x), sy(node.pos.y));
            let (w, h) = (node.w * z, node.h * z);
            let selected = er.selected == Some(i);
            let detail = active
                .schema
                .details
                .get(&(node.schema.clone(), node.table.clone()));

            let mut inner = div().flex().flex_col().size_full();
            if show_label {
                inner = inner.child(
                    div()
                        .flex_shrink_0()
                        // Fixed height, not text-sized: `anchor_offset` measures rows
                        // from `HEADER_H`, so a header that sized to its own text
                        // would slide every connector off its column.
                        .h(px(HEADER_H * z))
                        .flex()
                        .items_center()
                        .px(pad)
                        .bg(theme.bg_panel_2)
                        .text_size(header_size)
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .overflow_hidden()
                        .child(node.table.clone()),
                );
            } else {
                // Fitted-to-view: the name would be an unreadable smear, and shaping
                // one string per box is what makes a large schema stutter. Keep the
                // header band as a shape so the boxes still read as tables.
                inner = inner.child(
                    div()
                        .flex_shrink_0()
                        .h(px((HEADER_H * z).max(1.)))
                        .bg(theme.bg_panel_2),
                );
            }

            if show_cols {
                if let Some(detail) = detail {
                    // `PAD` above the first row and below the last, matching
                    // `node_height` and so `anchor_offset`.
                    let mut col_list = div().flex().flex_col().px(pad).py(pad);
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
                            .gap(px(COL_GAP * z))
                            .h(px(ROW_H * z))
                            .text_size(row_size)
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w(px(18. * z))
                                    .text_size(px(9.0 * z))
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
                            .px(pad)
                            .py(pad)
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
                    .rounded(px(6. * z))
                    .shadow_sm()
                    .overflow_hidden()
                    .child(inner)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            // Double-click opens the table as a browse (read-only) in
                            // its own tab. The diagram stays open beside it now that
                            // it's a tab rather than an overlay in the way.
                            if ev.click_count >= 2 {
                                this.open_table_browse(
                                    schema_name.clone(),
                                    table_name.clone(),
                                    None,
                                    cx,
                                );
                                return;
                            }
                            if let Some(er) = this.er_mut(tab_idx) {
                                er.selected = Some(i);
                                er.hand_placed = true;
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
                    // The connectors paint here rather than as child divs so they can
                    // be curves with cardinality marks at all. Being the first child
                    // also puts them under the boxes, where a relation line belongs.
                    move |bounds, _, window, _| {
                        for e in &edge_paints {
                            paint_edge(window, bounds.origin, e, edge_thickness, z, show_marks);
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(boxes)
            // Background press starts a pan; an empty-space press also clears selection.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    if let Some(er) = this.er_mut(tab_idx) {
                        er.selected = None;
                        er.drag = Some(Drag::Pan {
                            last: pos_of(ev.position),
                        });
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                let p = pos_of(ev.position);
                if let Some(er) = this.er_mut(tab_idx) {
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
                cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    if let Some(er) = this.er_mut(tab_idx)
                        && er.drag.take().is_some()
                    {
                        cx.notify();
                    }
                }),
            )
            // Scroll zooms around the cursor (scroll up / away = zoom in); panning is
            // by dragging the background. Exponential so pixel-precise trackpad deltas
            // feel smooth and coarse mouse-wheel notches still move meaningfully.
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, _, cx| {
                let p = pos_of(ev.position);
                if let Some(er) = this.er_mut(tab_idx) {
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
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_app)
            .child(header)
            .child(viewport)
            .into_any_element()
    }

    /// Ask the backend to describe the tables now visible in tab `tab_idx`'s diagram
    /// whose columns aren't resident yet.
    ///
    /// Called after the frame is built, not during it: `render_er` takes `&self` and
    /// this needs `&mut`. Nothing is lost by being a frame late, since the boxes it
    /// fills in were already going to be drawn empty on this frame.
    pub(crate) fn er_fetch_visible_details(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(er) = active.tabs.get(tab_idx).and_then(|t| t.er.as_ref()) else {
            return;
        };
        let wanted = er.missing_details(&active.schema.details);
        if wanted.is_empty() {
            return;
        }
        for (schema, table) in wanted {
            self.send_active(red_service::Command::DescribeTable { schema, table });
        }
        cx.notify();
    }
}

/// Convert a window-space pointer position to an `f32` [`Vec2`].
fn pos_of(p: Point<gpui::Pixels>) -> Vec2 {
    Vec2 {
        x: f32::from(p.x),
        y: f32::from(p.y),
    }
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

    /// A diagram scoped to a database keeps that database's schemas and drops every
    /// other one: the point of P1 is that a MySQL server's unrelated databases never
    /// share a canvas.
    #[test]
    fn namespace_filter_keeps_only_the_named_schema() {
        assert!(in_namespace("shop", Some("shop")));
        assert!(!in_namespace("analytics", Some("shop")));
        // Exact, not case-folded or prefix-matched: `shop` must not pull in `shop_v2`.
        assert!(!in_namespace("shop_v2", Some("shop")));
        assert!(!in_namespace("SHOP", Some("shop")));
    }

    /// `None` is the whole-connection diagram (SQLite, or a Postgres connection where
    /// every schema is in play), so every schema passes.
    #[test]
    fn no_namespace_keeps_every_schema() {
        assert!(in_namespace("public", None));
        assert!(in_namespace("anything", None));
    }

    /// A table whose columns haven't arrived yet is a bare header, and one past the
    /// row cap stops growing (plus a row for the "+N more" line), so a 500-column
    /// table can't produce a box taller than the canvas.
    #[test]
    fn node_height_is_bounded_by_the_row_cap() {
        let empty = node_height(None);
        assert_eq!(empty, node_height(Some(0)));
        assert!(node_height(Some(3)) > empty);
        // Past the cap: exactly one extra row for the "+N more" line, and no more
        // after that however many columns the table really has.
        let overflow = node_height(Some(MAX_ROWS + 1)) - node_height(Some(MAX_ROWS));
        assert!((overflow - ROW_H).abs() < 0.01, "one overflow row");
        assert_eq!(node_height(Some(MAX_ROWS + 1)), node_height(Some(1_000)));
    }

    /// Culling keeps what overlaps the viewport (and the margin around it) and drops
    /// what is far outside, in either axis.
    #[test]
    fn culling_keeps_visible_and_drops_far_boxes() {
        let vp = Rect {
            x: 0.,
            y: 0.,
            w: 800.,
            h: 600.,
        };
        assert!(rect_visible(100., 100., 200., 120., &vp), "inside");
        assert!(rect_visible(-100., 300., 200., 120., &vp), "straddles left");
        assert!(
            rect_visible(700., 550., 200., 120., &vp),
            "straddles corner"
        );
        // Just outside the viewport but inside the margin: kept, since the captured
        // rect is one frame stale.
        assert!(rect_visible(-250., 300., 200., 120., &vp), "within margin");
        assert!(!rect_visible(-1000., 300., 200., 120., &vp), "far left");
        assert!(!rect_visible(300., -1000., 200., 120., &vp), "far above");
        assert!(!rect_visible(2000., 300., 200., 120., &vp), "far right");
        assert!(!rect_visible(300., 2000., 200., 120., &vp), "far below");
    }

    /// Build `n` uniform boxes, the shape the layout functions operate on.
    fn nodes(n: usize) -> Vec<ErNode> {
        (0..n)
            .map(|i| ErNode {
                schema: "s".into(),
                table: format!("t{i}"),
                pos: Vec2::default(),
                w: NODE_W,
                h: node_height(Some(4)),
                fk_cols: HashSet::new(),
            })
            .collect()
    }

    /// Undirected adjacency from a parent list, the way `build` derives it.
    fn adjacency(parents: &[Vec<usize>]) -> Vec<Vec<usize>> {
        let mut adj = vec![Vec::new(); parents.len()];
        for (i, ps) in parents.iter().enumerate() {
            for &p in ps {
                adj[i].push(p);
                adj[p].push(i);
            }
        }
        adj
    }

    /// Whether any two boxes overlap: the layout's one hard invariant.
    fn overlaps(nodes: &[ErNode]) -> bool {
        nodes.iter().enumerate().any(|(i, a)| {
            nodes.iter().skip(i + 1).any(|b| {
                a.pos.x < b.pos.x + b.w
                    && b.pos.x < a.pos.x + a.w
                    && a.pos.y < b.pos.y + b.h
                    && b.pos.y < a.pos.y + a.h
            })
        })
    }

    /// Unrelated tables are separate components, so they're laid out as separate
    /// blocks rather than interleaved into one stack.
    #[test]
    fn components_split_unrelated_tables() {
        // 0↔1 related; 2 and 3 are islands.
        let parents = vec![vec![1], vec![], vec![], vec![]];
        let comps = components(4, &adjacency(&parents));
        assert_eq!(comps.len(), 3);
        assert!(comps.contains(&vec![0, 1]));
        assert!(comps.contains(&vec![2]));
        assert!(comps.contains(&vec![3]));
    }

    /// The pathological case from the bug report: a schema of FK-less tables used to
    /// land in one column thousands of pixels tall. They should pack into a block
    /// that's wider than it is tall, and never overlap.
    #[test]
    fn islands_pack_into_a_grid_not_one_tall_column() {
        let parents = vec![Vec::new(); 60];
        let adj = adjacency(&parents);
        let plan = plan_layout(60, &parents, &adj);
        let mut ns = nodes(60);
        position(&mut ns, &plan, &adj);

        assert!(!overlaps(&ns), "packed boxes must not overlap");
        let width = ns.iter().map(|n| n.pos.x + n.w).fold(0f32, f32::max);
        let height = ns.iter().map(|n| n.pos.y + n.h).fold(0f32, f32::max);
        assert!(
            width > height,
            "60 islands should pack wide, got {width}x{height}"
        );
        // More than one row and more than one column, i.e. an actual grid.
        assert!(ns.iter().any(|n| n.pos.x > 0.));
        assert!(ns.iter().any(|n| n.pos.y > 0.));
    }

    fn detail(cols: &[&str]) -> red_core::TableDetail {
        red_core::TableDetail {
            columns: cols
                .iter()
                .map(|n| red_core::ColumnMeta {
                    name: (*n).into(),
                    type_name: None,
                    not_null: false,
                    primary_key: false,
                    default: None,
                    auto_increment: false,
                })
                .collect(),
            foreign_keys: Vec::new(),
            indexes: Vec::new(),
        }
    }

    /// A connector meets the box on its column's row, not the box centre. The
    /// arithmetic has to track what `render_er` lays out — header band, `PAD`, then
    /// one `ROW_H` per column — so this pins it.
    #[test]
    fn anchor_lands_on_the_column_row() {
        let ns = nodes(1);
        let d = detail(&["id", "owner_id", "note"]);
        assert_eq!(
            anchor_offset(&ns[0], "owner_id", Some(&d)),
            HEADER_H + PAD + ROW_H + ROW_H / 2.0
        );
        // First row, and case-folded to match how `ErEdge` stores the name.
        assert_eq!(
            anchor_offset(&ns[0], "id", Some(&d)),
            HEADER_H + PAD + ROW_H / 2.0
        );
    }

    /// With no resident columns, a column that isn't drawn, or one past the row cap,
    /// the box centre is the only honest anchor.
    #[test]
    fn anchor_falls_back_to_the_box_centre() {
        let ns = nodes(1);
        let mid = ns[0].h / 2.0;
        assert_eq!(anchor_offset(&ns[0], "id", None), mid);
        assert_eq!(
            anchor_offset(&ns[0], "nope", Some(&detail(&["id"]))),
            mid,
            "unknown column"
        );
        let many: Vec<String> = (0..MAX_ROWS + 5).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let past = format!("c{}", MAX_ROWS + 2);
        assert_eq!(
            anchor_offset(&ns[0], &past, Some(&detail(&refs))),
            mid,
            "past the row cap, so not drawn"
        );
    }

    /// A box with one neighbour ends up level with it, so the connector runs
    /// straight across instead of doglegging. Here 0 and 1 both reference 2, and 3
    /// references nothing — without the alignment pass 2 would sit wherever the
    /// stack put it.
    #[test]
    fn alignment_levels_a_box_with_its_only_neighbour() {
        // 0 → 2 and 1 → 2, so layer 1 = [0, 1] and layer 0 = [2].
        let parents = vec![vec![2], vec![2], vec![]];
        let adj = adjacency(&parents);
        let plan = plan_layout(3, &parents, &adj);
        let mut ns = nodes(3);
        position(&mut ns, &plan, &adj);

        assert!(!overlaps(&ns));
        // The shared parent sits level with the midpoint of the two children.
        let centre = |n: &ErNode| n.pos.y + n.h / 2.0;
        let kids = (centre(&ns[0]) + centre(&ns[1])) / 2.0;
        assert!(
            (centre(&ns[2]) - kids).abs() < 1.0,
            "parent should be centred on its children: {} vs {kids}",
            centre(&ns[2])
        );
    }

    /// Standalone tables land in a grid below the related blocks, not scattered
    /// among them with a component gap around each.
    #[test]
    fn islands_sit_in_a_grid_below_the_related_blocks() {
        // One chain 0→1, plus 12 unrelated tables.
        let mut parents = vec![Vec::new(); 14];
        parents[0] = vec![1];
        let adj = adjacency(&parents);
        let plan = plan_layout(14, &parents, &adj);
        let mut ns = nodes(14);
        position(&mut ns, &plan, &adj);

        assert!(!overlaps(&ns));
        let block_bottom = ns[0].pos.y.max(ns[1].pos.y) + ns[0].h;
        for (i, island) in ns.iter().enumerate().skip(2) {
            assert!(
                island.pos.y >= block_bottom,
                "island {i} should sit below the related block"
            );
        }
        // Sorted into rows: the islands share a small set of x positions.
        let mut xs: Vec<i32> = (2..14).map(|i| ns[i].pos.x as i32).collect();
        xs.sort_unstable();
        xs.dedup();
        assert!(
            xs.len() > 1 && xs.len() < 12,
            "a grid, got {} columns",
            xs.len()
        );
    }

    /// A large mixed schema — the case that was "not much use" — stays roughly
    /// screen-shaped and collision-free, so `Fit` lands somewhere legible instead of
    /// scaling a tall ribbon into nothing.
    #[test]
    fn large_mixed_schema_stays_roughly_screen_shaped() {
        // 200 tables: 30 four-deep FK chains (120 tables) and 80 islands.
        let n = 200;
        let mut parents = vec![Vec::new(); n];
        for c in 0..30 {
            let b = c * 4;
            parents[b] = vec![b + 1];
            parents[b + 1] = vec![b + 2];
            parents[b + 2] = vec![b + 3];
        }
        let adj = adjacency(&parents);
        let plan = plan_layout(n, &parents, &adj);
        let mut ns = nodes(n);
        position(&mut ns, &plan, &adj);

        assert!(!overlaps(&ns), "no two boxes overlap");
        let width = ns.iter().map(|x| x.pos.x + x.w).fold(0f32, f32::max);
        let height = ns.iter().map(|x| x.pos.y + x.h).fold(0f32, f32::max);
        let aspect = width / height;
        assert!(
            (0.8..4.0).contains(&aspect),
            "canvas should be broadly landscape, got {width}x{height} (aspect {aspect})"
        );
    }

    /// A layered component still reads left-to-right by FK direction, and its boxes
    /// don't collide.
    #[test]
    fn layered_component_keeps_referenced_tables_left() {
        // 0 → 1 → 2, plus two islands to exercise packing alongside.
        let parents = vec![vec![1], vec![2], vec![], vec![], vec![]];
        let adj = adjacency(&parents);
        let plan = plan_layout(5, &parents, &adj);
        let mut ns = nodes(5);
        position(&mut ns, &plan, &adj);

        assert!(!overlaps(&ns));
        assert!(ns[2].pos.x < ns[1].pos.x, "referenced table sits left");
        assert!(ns[1].pos.x < ns[0].pos.x);
    }

    /// Barycentre ordering pulls a node next to its neighbour rather than leaving it
    /// wherever node order happened to put it. Here layer 1 arrives as [a, b] while
    /// their partners in layer 0 are ordered so the crossing-free answer is [b, a].
    #[test]
    fn ordering_reduces_crossings() {
        // Layer 0: nodes 0,1. Layer 1: nodes 2,3. Edges 2→1 and 3→0 cross if layer 1
        // keeps its natural order.
        let adj = vec![vec![3], vec![2], vec![1], vec![0]];
        let mut layers = vec![vec![0, 1], vec![2, 3]];
        order_layers(&mut layers, &adj);
        // 2 partners with 1 (slot 1) and 3 with 0 (slot 0), so layer 1 flips.
        assert_eq!(layers[1], vec![3, 2]);
    }

    /// A component whose shallowest node isn't at layer 0 is re-based, so it doesn't
    /// carry empty columns of dead space in front of it.
    #[test]
    fn components_are_rebased_to_column_zero() {
        // One chain 0→1→2 (layers 2,1,0) and a separate pair 3→4 (layers 1,0).
        let parents = vec![vec![1], vec![2], vec![], vec![4], vec![]];
        let adj = adjacency(&parents);
        let plan = plan_layout(5, &parents, &adj);
        let mut ns = nodes(5);
        position(&mut ns, &plan, &adj);
        // Every component starts a column at its own origin, so some node in each
        // block sits at that block's left edge.
        for comp in &plan {
            assert!(!comp[0].is_empty(), "layer 0 of a component is populated");
        }
        assert!(!overlaps(&ns));
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
