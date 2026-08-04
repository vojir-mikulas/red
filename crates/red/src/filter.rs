//! The result filter bar: a small input strip above the grid that
//! narrows the whole result by pushing a predicate into the query. Distinct from
//! find-in-result (which only highlights loaded rows), a filter **re-opens** the
//! result under a new epoch with the predicate wrapped in, so the count, the
//! keyset seek key, sort, and export all operate on the filtered set, never
//! materializing it (the wrap keeps `SELECT *`, so the key column survives).
//!
//! The bar is the transient editing UI; the *applied* filter lives on the grid
//! (`ResultGrid::filter`) and rides every (re)open. Three modes:
//!
//! - **Contains** — a portable term, rendered per engine to a safe `LIKE`/`ILIKE`
//!   OR-chain.
//! - **WHERE** — a raw SQL expression for power users, trusted like editor SQL and
//!   given the editor's own highlighting, column completion, and diagnostics.
//! - **Column** — `column ▾ operator ▾ value`, for narrowing without writing SQL.
//!   It builds a `ResultFilter::Cmp`, which the driver renders and escapes, so no
//!   user text ever reaches the query as SQL. The cell menu's "Filter by" builds
//!   the same thing from the focused cell.
//!
//! The mode lives *on* the control: the bar is one combined
//! `[ mode ▾ │ input ] [Apply] [✕]` unit whose leading seamless `Select` always
//! names the active mode, the same shape as the Redis key browser's search field
//! (`kvbrowse::render`). An applied filter is also always visible as a chip in the
//! result toolbar (see `ResultGrid::render`), so a narrowed grid can never look
//! like an unfiltered one. A trailing clock recalls what was applied here before
//! (`filters.rs`); ↑/↓ in either text box walk the same list.

use flint::prelude::*;
use gpui::{AnyElement, App, Context, Entity, Focusable, Window, div, prelude::*, px};
use red_core::{CmpOp, Column as ResultColumn, ColumnPredicate, ColumnValue, ResultFilter, Value};

use crate::app::{AppState, Phase};

/// How many characters of a filter's text the toolbar chip shows before eliding
/// (the full text is in the chip's tooltip).
const CHIP_MAX_CHARS: usize = 44;

/// Whether the filter input is read as a portable "contains" term or a raw SQL
/// `WHERE` expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FilterMode {
    #[default]
    Contains,
    Where,
    /// `column ▾ operator ▾ value`, for narrowing without writing SQL. Lowers to
    /// the structured [`ResultFilter::Cmp`], never to typed SQL.
    Column,
}

impl FilterMode {
    /// The modes in dropdown order.
    pub(crate) const ALL: [FilterMode; 3] =
        [FilterMode::Contains, FilterMode::Where, FilterMode::Column];

    /// The dropdown label, also the prefix the toolbar chip reads with.
    pub(crate) fn label(self) -> &'static str {
        match self {
            FilterMode::Contains => "Contains",
            FilterMode::Where => "WHERE",
            FilterMode::Column => "Column",
        }
    }

    /// The input placeholder. `WHERE` is raw SQL, so the hint nudges toward an
    /// expression and reminds that an inline-expanded reference column is
    /// referenced by its quoted dotted name (`"tier_id.name"`).
    fn placeholder(self) -> &'static str {
        match self {
            FilterMode::Contains => "Text in any column…",
            FilterMode::Where => "amount > 100 AND \"tier_id.name\" = 'gold'",
            FilterMode::Column => "Value…",
        }
    }

    /// The stable on-disk tag for the recent-filters store (`filters.rs`).
    /// Independent of [`label`](Self::label), which is display text and free to
    /// change; a tag rename would orphan everyone's saved filters.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            FilterMode::Contains => "contains",
            FilterMode::Where => "where",
            FilterMode::Column => "column",
        }
    }

    /// The mode a stored [`tag`](Self::tag) names, or `None` for a tag this build
    /// doesn't know (a file written by a newer one).
    pub(crate) fn from_tag(tag: &str) -> Option<FilterMode> {
        FilterMode::ALL.iter().copied().find(|m| m.tag() == tag)
    }

    /// Whether the mode's filter is a piece of *text* that can be remembered and
    /// recalled. `Column` builds structure, so it has nothing to store in the
    /// recent-filters list (`filters.rs`).
    fn is_text(self) -> bool {
        !matches!(self, FilterMode::Column)
    }
}

/// The picker label for a comparison operator: the SQL spelling, which is what a
/// data-grid user recognizes, with the unary ones spelled out. `Contains` is the
/// exception — it has no operator of its own, so it reads as the pattern it
/// builds, which is also the clearest statement of what it does.
pub(crate) fn op_label(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Contains => "LIKE %…%",
        other => op_sql(other),
    }
}

/// State for the open filter bar (present iff the bar is showing). Both boxes are
/// built up front and the mode picks which one shows, so switching modes keeps
/// what you typed. Their event `Subscription`s are held here, not detached, so
/// closing the bar (nulling the owning `Option`) drops them with it rather than
/// orphaning them.
pub(crate) struct FilterBarState {
    /// The `Contains` box: a plain term, no SQL semantics to highlight.
    pub(crate) input: Entity<TextInput>,
    /// The `WHERE` box: a single-line `CodeEditor`, so a predicate gets the same
    /// SQL highlighting, column completion, and diagnostics as the query editor
    /// (installed by `AppState::refresh_filter_completions`).
    pub(crate) expr: Entity<CodeEditor>,
    /// The `Column` box: the literal being compared against, coerced by the
    /// chosen column's declared type (`red_core::coerce_edit_value`).
    pub(crate) value: Entity<TextInput>,
    /// `Column` mode's built conjunction as it stands, seeded from an applied
    /// `Cmp` filter and shown as chips. The pending term (the builder row) is
    /// *not* in here until it's added or applied.
    pub(crate) terms: Vec<ColumnPredicate>,
    /// The builder row's column: an index into the active result's columns.
    pub(crate) col_ix: usize,
    pub(crate) col_open: bool,
    /// The builder row's operator, narrowed by the column's declared type.
    pub(crate) op: CmpOp,
    pub(crate) op_open: bool,
    pub(crate) mode: FilterMode,
    /// Whether the leading mode dropdown is showing.
    pub(crate) mode_open: bool,
    /// Whether the trailing recall dropdown (recent filters) is showing.
    pub(crate) history_open: bool,
    /// ↑/↓ readline recall position: an index into the current mode's recent
    /// filters while walking them, `None` on a line the user typed themselves.
    pub(crate) recall: Option<usize>,
    /// RAII: held to keep these input subscriptions alive; never read.
    pub(crate) _sub: gpui::Subscription,
    pub(crate) _expr_sub: gpui::Subscription,
    pub(crate) _value_sub: gpui::Subscription,
}

impl FilterBarState {
    /// The focus handle of whichever box the current mode shows.
    pub(crate) fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        match self.mode {
            FilterMode::Contains => self.input.focus_handle(cx),
            FilterMode::Where => self.expr.focus_handle(cx),
            FilterMode::Column => self.value.focus_handle(cx),
        }
    }

    /// The current box's text.
    fn text(&self, cx: &gpui::App) -> String {
        match self.mode {
            FilterMode::Contains => self.input.read(cx).content().to_string(),
            FilterMode::Where => self.expr.read(cx).content(),
            FilterMode::Column => self.value.read(cx).content().to_string(),
        }
    }
}

impl AppState {
    /// ⌘⇧F / the toolbar chip: open the filter bar, or, when it's already open,
    /// focus it (a second ⌘⇧F *in* the bar closes it). Opening seeds the input +
    /// mode from the active result's current filter so it's editable.
    pub(crate) fn toggle_filter_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bar) = &self.filter_bar {
            // Open but focus is elsewhere (the grid, the editor): bring focus here
            // rather than closing a bar the user may not even be looking at.
            if bar.input.focus_handle(cx).is_focused(window) {
                self.close_filter_bar(cx);
            } else {
                self.focus_filter = true;
                cx.notify();
            }
            return;
        }
        let (mode, text, terms) =
            bar_seed(self.active_result_filter(cx).as_ref(), self.filter_mode);
        self.open_filter_bar_with(mode, text, terms, cx);
    }

    /// Build the bar on `mode` with `text` (or `terms`, in `Column` mode)
    /// pre-filled and focus it on the next render. Shared by ⌘⇧F, the toolbar
    /// chip, and any caller that seeds a filter.
    fn open_filter_bar_with(
        &mut self,
        mode: FilterMode,
        text: String,
        terms: Vec<ColumnPredicate>,
        cx: &mut Context<Self>,
    ) {
        // `bare()`: the box has no chrome of its own, the combined field around it
        // owns the border/background (see `render_filter_bar`).
        // `emit_nav`: a one-line field has no vertical motion of its own, so ↑/↓
        // surface as events and drive readline-style recall of recent filters.
        let input = cx.new(|cx| {
            TextInput::new(cx)
                .bare()
                .emit_nav()
                .with_placeholder(FilterMode::Contains.placeholder())
        });
        // A one-line SQL surface: no gutter, Enter applies instead of inserting a
        // newline, no wrap. Completion/diagnostics are installed below, once the
        // bar exists (they read the active result's columns).
        let expr = cx.new(|cx| {
            CodeEditor::new(cx)
                .highlighter(crate::sql::tokenize)
                .gutter(false)
                .submit_on_enter(true)
                .soft_wrap(false)
                .resting_border(false)
                // One line, so ↑/↓ are always "at the edge" and surface as
                // `CodeEditorEvent::Up`/`Down` for recall (a completion popup
                // still takes them first, to move its highlight).
                .emit_nav(true)
                // No vertical padding: the editor's default 8px top+bottom is
                // more than a 24px field has to give, which would push the line
                // out of view and raise a scrollbar over a one-line buffer.
                .vertical_padding(px(0.))
                .corner_radius(px(0.))
                .a11y_label(crate::i18n::tr!(
                    "filter.filter_expression",
                    "Filter expression"
                ))
                .placeholder(FilterMode::Where.placeholder())
        });
        // `Column` mode's literal box. Plain text; the chosen column's declared
        // type decides how it's coerced (`pending_filter_term`).
        let value = cx.new(|cx| {
            TextInput::new(cx)
                .bare()
                .with_placeholder(FilterMode::Column.placeholder())
        });
        if !text.is_empty() {
            match mode {
                FilterMode::Contains => {
                    input.update(cx, |i, cx| i.set_content(text, cx));
                }
                FilterMode::Where => {
                    expr.update(cx, |e, cx| e.set_content(text, cx));
                }
                // `Column` mode carries `terms`, not text.
                FilterMode::Column => {}
            }
        }
        // Enter applies; Esc first reverts an edited box to the applied filter,
        // then (unchanged) closes the bar.
        let sub = cx.subscribe(&input, |this, _input, evt: &TextInputEvent, cx| match evt {
            TextInputEvent::Submit => this.submit_filter(cx),
            TextInputEvent::Cancel => this.filter_escape(cx),
            // Readline recall: ↑ steps to older filters for this table, ↓ back.
            TextInputEvent::Up => this.recall_filter(true, cx),
            TextInputEvent::Down => this.recall_filter(false, cx),
            // Typing means the line is the user's again, so recall restarts from
            // the newest entry rather than continuing from where it left off.
            TextInputEvent::Change => this.reset_filter_recall(),
            TextInputEvent::Tab | TextInputEvent::BackTab => {}
        });
        let expr_sub = cx.subscribe(
            &expr,
            |this, _editor, evt: &CodeEditorEvent, cx| match evt {
                // ⌘↵ means the same thing as Enter here: apply this predicate.
                CodeEditorEvent::Submit | CodeEditorEvent::Run => this.submit_filter(cx),
                CodeEditorEvent::Escape => this.filter_escape(cx),
                // Same readline recall as the `Contains` box: the editor is one
                // line, so `emit_nav` hands every arrow over (see its docs).
                CodeEditorEvent::Up => this.recall_filter(true, cx),
                CodeEditorEvent::Down => this.recall_filter(false, cx),
                CodeEditorEvent::RunLine(_) => {}
            },
        );
        // Enter in the value box adds nothing new to learn: it applies, like the
        // other two boxes. Esc reverts / closes the same way.
        let value_sub = cx.subscribe(&value, |this, _input, evt: &TextInputEvent, cx| match evt {
            TextInputEvent::Submit => this.submit_filter(cx),
            TextInputEvent::Cancel => this.filter_escape(cx),
            TextInputEvent::Change
            | TextInputEvent::Tab
            | TextInputEvent::BackTab
            | TextInputEvent::Up
            | TextInputEvent::Down => {}
        });
        self.filter_mode = mode;
        self.filter_bar = Some(FilterBarState {
            input,
            expr,
            value,
            terms,
            col_ix: 0,
            col_open: false,
            op: CmpOp::Eq,
            op_open: false,
            mode,
            mode_open: false,
            history_open: false,
            recall: None,
            _sub: sub,
            _expr_sub: expr_sub,
            _value_sub: value_sub,
        });
        self.refresh_filter_completions(cx);
        // The Window isn't in hand here; focus the box on the next render.
        self.focus_filter = true;
        cx.notify();
    }

    /// Reset both boxes to whatever filter is actually applied (after a revert, or
    /// when Esc undoes an edit). An `Eq` filter seeds the `WHERE` box with its
    /// equivalent expression, since it has no text form of its own.
    pub(crate) fn seed_filter_bar(&mut self, cx: &mut Context<Self>) {
        let applied = self.active_result_filter(cx);
        let Some(bar) = &self.filter_bar else { return };
        let (input, expr, value) = (bar.input.clone(), bar.expr.clone(), bar.value.clone());
        let (mode, text, terms) = bar_seed(applied.as_ref(), bar.mode);
        let (contains, where_expr) = match mode {
            FilterMode::Contains => (text, String::new()),
            FilterMode::Where => (String::new(), text),
            FilterMode::Column => (String::new(), String::new()),
        };
        input.update(cx, |i, cx| i.set_content(contains, cx));
        expr.update(cx, |e, cx| e.set_content(where_expr, cx));
        // The builder row is scratch space, not part of what's applied.
        value.update(cx, |v, cx| v.set_content("", cx));
        if let Some(bar) = &mut self.filter_bar {
            bar.terms = terms;
        }
        self.set_filter_mode(mode, cx);
    }

    /// Close the bar, leaving any applied filter in place. Returns focus to root.
    pub(crate) fn close_filter_bar(&mut self, cx: &mut Context<Self>) {
        if self.filter_bar.take().is_some() {
            self.refocus_root = true;
            cx.notify();
        }
    }

    /// Esc in the box: an edited box first snaps back to whatever is actually
    /// applied (so Esc means "undo my typing", as in the editor), and only a
    /// second Esc closes the bar. Never clears an applied filter.
    fn filter_escape(&mut self, cx: &mut Context<Self>) {
        // An open recall dropdown is the innermost thing Esc dismisses.
        if let Some(bar) = &mut self.filter_bar
            && bar.history_open
        {
            bar.history_open = false;
            cx.notify();
            return;
        }
        let applied = self.active_result_filter(cx);
        let Some(bar) = &self.filter_bar else { return };
        let (mode, text, terms) = bar_seed(applied.as_ref(), bar.mode);
        // Unedited (the box, and in `Column` mode the chips, already match what's
        // applied): this Esc is the one that closes the bar.
        let unedited = match mode {
            FilterMode::Column => bar.terms == terms && bar.text(cx).is_empty(),
            _ => bar.text(cx) == text,
        };
        if bar.mode == mode && unedited {
            self.close_filter_bar(cx);
            return;
        }
        self.seed_filter_bar(cx);
    }

    /// Toggle the leading mode dropdown.
    pub(crate) fn toggle_filter_mode_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.filter_bar {
            bar.mode_open = !bar.mode_open;
            cx.notify();
        }
    }

    /// Switch the bar between contains / raw-`WHERE` modes (and dismiss the mode
    /// dropdown). The choice sticks for the session, so a `WHERE` user isn't
    /// thrown back to `Contains` on the next result. Switching doesn't re-run:
    /// the text means something different now, so the user presses Enter/Apply.
    pub(crate) fn set_filter_mode(&mut self, mode: FilterMode, cx: &mut Context<Self>) {
        self.filter_mode = mode;
        let Some(bar) = &self.filter_bar else { return };
        // Carry the text across the two text modes, so switching reinterprets what's
        // typed rather than throwing it away. `Column` builds structure, so there is
        // nothing to carry either way and its chips are left alone.
        if bar.mode != mode && bar.mode.is_text() && mode.is_text() {
            let text = bar.text(cx);
            let (input, expr) = (bar.input.clone(), bar.expr.clone());
            match mode {
                FilterMode::Contains => input.update(cx, |i, cx| i.set_content(text, cx)),
                FilterMode::Where => expr.update(cx, |e, cx| e.set_content(text, cx)),
                FilterMode::Column => {}
            }
        }
        if let Some(bar) = &mut self.filter_bar {
            bar.mode = mode;
            bar.mode_open = false;
            // Recall walks one mode's entries (see `recall_filter`), so a mode
            // switch starts a fresh walk.
            bar.recall = None;
        }
        self.focus_filter = true;
        cx.notify();
    }

    /// Apply the bar's current text as the result filter (Enter / the Apply
    /// button). An empty term clears the filter. The bar stays open (focus kept in
    /// the input) so the filter can be refined or re-run; Esc / the ✕ closes it.
    ///
    /// `Column` mode applies its chips plus whatever the builder row has
    /// completed, so a user who typed a value and hit Enter doesn't have to press
    /// "+" first to be taken seriously.
    pub(crate) fn submit_filter(&mut self, cx: &mut Context<Self>) {
        let Some(mode) = self.filter_bar.as_ref().map(|bar| bar.mode) else {
            return;
        };
        if mode == FilterMode::Column {
            let Some(mut terms) = self.filter_bar.as_ref().map(|b| b.terms.clone()) else {
                return;
            };
            if let Some(pending) = self.pending_filter_term(cx) {
                terms.push(pending);
            }
            let filter = (!terms.is_empty()).then(|| ResultFilter::Cmp(terms.clone()));
            if let Some(bar) = &mut self.filter_bar {
                // The pending term is now part of the conjunction, so it becomes a
                // chip and the builder row resets for the next one.
                bar.terms = terms;
                let value = bar.value.clone();
                value.update(cx, |v, cx| v.set_content("", cx));
            }
            self.apply_filter_and_refocus(filter, cx);
            return;
        }
        let text = self
            .filter_bar
            .as_ref()
            .map(|bar| bar.text(cx).trim().to_string())
            .unwrap_or_default();
        let filter = if text.is_empty() {
            None
        } else {
            // Remember what was applied, so the same table offers it again later
            // (recorded on apply, not on success: like the query history, what the
            // user asked for is what's worth recalling).
            if let Some((conn_id, scope)) = self.filter_scope(cx) {
                self.filter_history
                    .record(&conn_id, scope.as_deref(), mode, &text);
            }
            Some(match mode {
                FilterMode::Contains => ResultFilter::Contains(text),
                FilterMode::Where => ResultFilter::Where(text),
                FilterMode::Column => unreachable!("handled above"),
            })
        };
        self.apply_filter_and_refocus(filter, cx);
    }

    /// Apply `filter` and leave the bar open with focus back in its box (which
    /// covers Apply, where focus was on the button), so it can be tweaked and
    /// re-run. The tail every submit path shares.
    fn apply_filter_and_refocus(&mut self, filter: Option<ResultFilter>, cx: &mut Context<Self>) {
        self.apply_result_filter(filter, cx);
        if let Some(bar) = &mut self.filter_bar {
            bar.history_open = false;
            bar.recall = None;
        }
        self.focus_filter = true;
        cx.notify();
    }

    /// The active result's columns, in order: what `Column` mode's column picker
    /// offers and what a built predicate can name.
    pub(crate) fn filter_columns(&self, cx: &App) -> Vec<ResultColumn> {
        // See `row_edit_mode`: `cx` is taken ahead of the `Entity<ResultGrid>`
        // change so that change stays local instead of cascading.
        let _ = &cx;
        match &self.phase {
            Phase::Connected(active) => active
                .active_result()
                .map(|g| g.columns().to_vec())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// The operators offered for the builder row's column: `LIKE` only on a
    /// text-representable column (there is no cast in the rendered predicate by
    /// design, so `LIKE` against an integer errors on Postgres), the ordering ones
    /// only where an order means something.
    fn filter_ops(&self, column: Option<&ResultColumn>) -> Vec<CmpOp> {
        let decl = column.and_then(|c| c.decl_type.as_deref());
        // An unknown declared type (editor SQL, a computed column) offers
        // everything: the engine has the final word and we'd rather not hide an
        // operator the user actually needs.
        let numeric = red_core::is_numeric_type(decl);
        CmpOp::ALL
            .iter()
            .copied()
            .filter(|op| !(op.text_only() && numeric))
            .collect()
    }

    /// The term the builder row currently describes, or `None` while it's
    /// incomplete (no column, or a value-taking operator with a blank / uncoercible
    /// value). Never partially applied: an incomplete row is simply not a term.
    fn pending_filter_term(&self, cx: &gpui::App) -> Option<ColumnPredicate> {
        let bar = self.filter_bar.as_ref()?;
        let columns = self.filter_columns(cx);
        let column = columns.get(bar.col_ix)?;
        if !bar.op.takes_value() {
            return Some(ColumnPredicate {
                column: column.name.clone(),
                op: bar.op,
                value: None,
            });
        }
        let text = bar.value.read(cx).content().to_string();
        if text.trim().is_empty() {
            return None;
        }
        // Guided by the declared type, so a numeric column compares as a number
        // rather than as a string literal. A parse failure means "not a term yet".
        let value = red_core::coerce_edit_value(&text, column.decl_type.as_deref()).ok()?;
        Some(ColumnPredicate {
            column: column.name.clone(),
            op: bar.op,
            value: Some(value),
        })
    }

    /// Toggle the builder row's column picker.
    pub(crate) fn toggle_filter_column_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.filter_bar {
            bar.col_open = !bar.col_open;
            cx.notify();
        }
    }

    /// Pick the builder row's column. The operator is re-checked against the new
    /// column's type, so switching to a numeric column can't leave `LIKE` selected.
    pub(crate) fn set_filter_column(&mut self, ix: usize, cx: &mut Context<Self>) {
        let columns = self.filter_columns(cx);
        let ops = self.filter_ops(columns.get(ix));
        if let Some(bar) = &mut self.filter_bar {
            bar.col_ix = ix;
            bar.col_open = false;
            if !ops.contains(&bar.op) {
                bar.op = ops.first().copied().unwrap_or(CmpOp::Eq);
            }
            cx.notify();
        }
    }

    /// Toggle the builder row's operator picker.
    pub(crate) fn toggle_filter_op_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.filter_bar {
            bar.op_open = !bar.op_open;
            cx.notify();
        }
    }

    /// Pick the builder row's operator.
    pub(crate) fn set_filter_op(&mut self, op: CmpOp, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.filter_bar {
            bar.op = op;
            bar.op_open = false;
            cx.notify();
        }
    }

    /// "+": fold the builder row into the conjunction as another chip and clear it
    /// for the next term, *without* re-running. Building a multi-term filter
    /// shouldn't cost one query per term; Apply runs the finished conjunction.
    pub(crate) fn add_filter_term(&mut self, cx: &mut Context<Self>) {
        let Some(term) = self.pending_filter_term(cx) else {
            return;
        };
        if let Some(bar) = &mut self.filter_bar {
            bar.terms.push(term);
            let value = bar.value.clone();
            value.update(cx, |v, cx| v.set_content("", cx));
        }
        self.focus_filter = true;
        cx.notify();
    }

    /// A chip's ✕: drop one term and re-run, so the grid tracks what the chips
    /// say. Dropping the last term clears the filter rather than applying an empty
    /// conjunction.
    pub(crate) fn remove_filter_term(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(bar) = &mut self.filter_bar else {
            return;
        };
        if ix >= bar.terms.len() {
            return;
        }
        bar.terms.remove(ix);
        let terms = bar.terms.clone();
        let filter = (!terms.is_empty()).then_some(ResultFilter::Cmp(terms));
        self.apply_filter_and_refocus(filter, cx);
    }

    /// The focused cell as a filter target: its column and its value. `None` when
    /// no cell is focused or its row has been evicted from the resident window.
    pub(crate) fn cell_filter_target(&self, cx: &App) -> Option<(ResultColumn, Value)> {
        // See `row_edit_mode`: `cx` is taken ahead of the `Entity<ResultGrid>`
        // change so that change stays local instead of cascading.
        let _ = &cx;
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (row, col) = grid.cursor_cell(self.gutter())?;
        let column = grid.columns().get(col)?.clone();
        Some((column, grid.cell_value(row, col)?))
    }

    /// The cell menu's "Filter by": narrow to `column <op> value` from the focused
    /// cell. `and_join` ANDs onto an applied `Cmp` filter (the "add to filter"
    /// item) instead of replacing it.
    ///
    /// The predicate is built as *structure*, never as SQL: the driver renders and
    /// escapes it, so a value taken from a result cell can't inject.
    pub(crate) fn filter_by_cell(&mut self, op: CmpOp, and_join: bool, cx: &mut Context<Self>) {
        let Some((column, value)) = self.cell_filter_target(cx) else {
            return;
        };
        let term = ColumnPredicate {
            column: column.name,
            op,
            value: op.takes_value().then_some(value),
        };
        let mut terms = match (and_join, self.active_result_filter(cx)) {
            (true, Some(ResultFilter::Cmp(existing))) => existing,
            _ => Vec::new(),
        };
        terms.push(term);
        // Open (or re-seed) the bar in `Column` mode so the new filter is visible
        // and editable as chips, rather than appearing only as a toolbar chip.
        self.apply_result_filter(Some(ResultFilter::Cmp(terms.clone())), cx);
        match &mut self.filter_bar {
            Some(bar) => {
                bar.terms = terms;
                self.set_filter_mode(FilterMode::Column, cx);
            }
            None => self.open_filter_bar_with(FilterMode::Column, String::new(), terms, cx),
        }
        cx.notify();
    }

    /// Whether the cell menu's "Add to filter" item applies: there is a focused
    /// cell *and* an applied built filter to add to.
    pub(crate) fn can_add_cell_filter_term(&self, cx: &App) -> bool {
        // See the note on `row_edit_mode`: `cx` is taken ahead of the
        // `Entity<ResultGrid>` change so that change does not cascade.
        let _ = &cx;
        self.cell_filter_target(cx).is_some()
            && matches!(self.active_result_filter(cx), Some(ResultFilter::Cmp(_)))
    }

    /// Which bucket of the recent-filters store the active result reads and
    /// writes: `(conn_id, browsed table)`. `None` before a connection is up; a
    /// `None` table is the editor-results bucket (see `filters.rs`).
    fn filter_scope(&self, cx: &App) -> Option<(String, Option<String>)> {
        // See `row_edit_mode`: `cx` is taken ahead of the `Entity<ResultGrid>`
        // change so that change stays local instead of cascading.
        let _ = &cx;
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let table = active.active_result().and_then(|g| g.browsed_table());
        Some((active.conn_id.clone(), table))
    }

    /// This result's recent filters, newest-first (the recall dropdown's rows).
    pub(crate) fn recent_filters(&self, cx: &App) -> Vec<crate::filters::RecentFilter> {
        // See `row_edit_mode`: `cx` is taken ahead of the `Entity<ResultGrid>`
        // change so that change stays local instead of cascading.
        let _ = &cx;
        let Some((conn_id, scope)) = self.filter_scope(cx) else {
            return Vec::new();
        };
        self.filter_history.for_scope(&conn_id, scope.as_deref())
    }

    /// Toggle the trailing recall dropdown of recent filters.
    pub(crate) fn toggle_filter_history(&mut self, cx: &mut Context<Self>) {
        if let Some(bar) = &mut self.filter_bar {
            bar.history_open = !bar.history_open;
            cx.notify();
        }
    }

    /// Pick a remembered filter (a click in the recall dropdown): seed the bar's
    /// mode + text and focus it, but **don't** run it. A recalled predicate can be
    /// expensive and is often a starting point to edit; Enter/Apply runs it, the
    /// same seed-don't-run contract the history panel and console recall keep.
    pub(crate) fn seed_recent_filter(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.recent_filters(cx).get(ix).cloned() else {
            return;
        };
        let Some(mode) = entry.mode() else { return };
        self.set_filter_mode(mode, cx);
        self.set_filter_text(&entry.text, cx);
        if let Some(bar) = &mut self.filter_bar {
            bar.history_open = false;
            bar.recall = None;
        }
        cx.notify();
    }

    /// Forget one remembered filter (the dropdown row's ✕).
    pub(crate) fn forget_recent_filter(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.recent_filters(cx).get(ix).cloned() else {
            return;
        };
        let Some((conn_id, scope)) = self.filter_scope(cx) else {
            return;
        };
        self.filter_history
            .forget(&conn_id, scope.as_deref(), &entry.mode, &entry.text);
        cx.notify();
    }

    /// ↑/↓ in either text box: walk this table's remembered filters, `prev` (↑)
    /// toward older ones, `!prev` (↓) back toward newer, past the newest clearing
    /// the line — the shell recall the Redis console already has.
    ///
    /// The walk stays **within the current mode**: an entry carries the mode its
    /// text is read in, and switching modes mid-walk would swap the focused box
    /// out from under the arrow keys. The dropdown lists every mode, where a
    /// click can safely switch.
    fn recall_filter(&mut self, prev: bool, cx: &mut Context<Self>) {
        let Some(mode) = self.filter_bar.as_ref().map(|b| b.mode) else {
            return;
        };
        let entries: Vec<String> = self
            .recent_filters(cx)
            .into_iter()
            .filter(|e| e.mode() == Some(mode))
            .map(|e| e.text)
            .collect();
        if entries.is_empty() {
            return;
        }
        // A box whose text no longer matches the entry it was recalled from has
        // been edited, so the walk restarts from the newest. Derived from the text
        // rather than from a change event, because `CodeEditor` (the `WHERE` box)
        // has none — and this stays correct for the `Contains` box either way.
        let edited = self.filter_bar.as_ref().is_some_and(|bar| {
            bar.recall
                .is_some_and(|i| entries.get(i) != Some(&bar.text(cx)))
        });
        let Some(bar) = &mut self.filter_bar else {
            return;
        };
        if edited {
            bar.recall = None;
        }
        // Entries are newest-first, so index 0 is the most recent and ↑ walks up.
        let next = match (bar.recall, prev) {
            (None, true) => Some(0),
            (None, false) => return, // ↓ on a line the user typed: nothing newer
            (Some(i), true) => Some((i + 1).min(entries.len() - 1)),
            (Some(0), false) => None,
            (Some(i), false) => Some(i - 1),
        };
        bar.recall = next;
        let text = next.map(|i| entries[i].clone()).unwrap_or_default();
        self.set_filter_text(&text, cx);
    }

    /// Typing ends a recall walk, so the next ↑ starts again from the newest.
    fn reset_filter_recall(&mut self) {
        if let Some(bar) = &mut self.filter_bar {
            bar.recall = None;
        }
    }

    /// Replace the current mode's box contents (recall / a dropdown pick).
    /// Programmatic, so it doesn't echo back as a `Change` and clear the recall
    /// position it was just set from.
    fn set_filter_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(bar) = &self.filter_bar else { return };
        match bar.mode {
            FilterMode::Contains => {
                let input = bar.input.clone();
                input.update(cx, |i, cx| i.set_content(text, cx));
            }
            FilterMode::Where => {
                let expr = bar.expr.clone();
                expr.update(cx, |e, cx| e.set_content(text, cx));
            }
            // `Column` mode has no recallable text (see `FilterMode::is_text`), so
            // nothing routes here; the value box is only ever edited by hand.
            FilterMode::Column => {}
        }
        cx.notify();
    }

    /// Clear the applied filter and close the bar (the chip's ✕ / the Clear button).
    pub(crate) fn clear_result_filter(&mut self, cx: &mut Context<Self>) {
        self.apply_result_filter(None, cx);
        self.filter_bar = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// The recall dropdown: this table's remembered filters, newest first, hung
    /// under the clock that opens it. A click seeds (never runs); the per-row ✕
    /// forgets.
    ///
    /// `anchor` is the clock button's *measured* window rect. Anchoring to a
    /// layout-flow guess would place the list at the top-left of the whole filter
    /// field — the full width of the bar away from the button it belongs to — so
    /// this mirrors how Flint's `Select` positions its own menu.
    fn render_filter_recall(
        &self,
        recent: &[crate::filters::RecentFilter],
        anchor: gpui::Bounds<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, text, dim, hover) = (
            theme.bg_elevated,
            theme.border,
            theme.text,
            theme.text_dim,
            theme.bg_hover,
        );
        let size = theme.scale(11.);

        let mut list = div()
            .id("filter-recall-list")
            .max_h(px(240.))
            .overflow_y_scroll()
            .text_size(size)
            .text_color(text);
        for (ix, entry) in recent.iter().enumerate() {
            let Some(mode) = entry.mode() else { continue };
            let when = red_config::history::relative_time(entry.used_unix);
            list = list.child(
                div()
                    .id(("filter-recall-row", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2p5()
                    .py_1p5()
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.seed_recent_filter(ix, cx)))
                    // The mode is part of what's remembered, so it's on the row:
                    // the same text means different things in the two modes.
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(52.))
                            .text_color(dim)
                            .child(mode.label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(40.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(entry.text.replace(['\n', '\r'], " ")),
                    )
                    .child(div().flex_shrink_0().text_color(dim).child(when))
                    .child(
                        IconButton::new(
                            ("filter-recall-forget", ix),
                            crate::icons::icon("close", theme.scale(11.), dim),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip(crate::i18n::tr!(
                            "filter.forget_this_filter",
                            "Forget this filter"
                        ))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.forget_recent_filter(ix, cx)),
                        ),
                    ),
            );
        }

        let panel = div()
            .occlude()
            // Swallow any wheel the list didn't consume so it never reaches (and
            // scrolls) the data grid behind the popup.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .w(px(360.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .rounded_md()
            .shadow_lg()
            .py_1()
            .child(list);
        // Hang the list's top-*right* corner off the button's bottom-right, so it
        // opens leftward under the clock instead of running off the window edge.
        floating(panel)
            .anchor(gpui::Anchor::TopRight)
            .at(anchor.bottom_right())
            .offset(gpui::point(px(0.), px(4.)))
            .into_any_element()
    }

    /// The filter-bar editing strip, rendered above the grid when the bar is open.
    pub(crate) fn render_filter_bar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let bar = self.filter_bar.as_ref()?;
        // The recall clock's measured window rect, recorded by the canvas overlay
        // below and read back here to place the list under it. `None` on the very
        // first frame (nothing measured yet), where the list simply waits a frame.
        let clock_bounds = window.use_keyed_state(
            gpui::SharedString::from("filter-history-bounds"),
            cx,
            |_, _| None::<gpui::Bounds<gpui::Pixels>>,
        );
        let measured = *clock_bounds.read(cx);
        // Built up front: it needs `cx` mutably, which the borrow of `cx.theme()`
        // below rules out for the rest of this function.
        let recent = self.recent_filters(cx);
        let recall_panel = bar
            .history_open
            .then(|| measured.map(|b| self.render_filter_recall(&recent, b, cx)))
            .flatten();
        let theme = cx.theme();
        let (border, border_strong, muted, bg, bg_input, size) = (
            theme.border,
            theme.border_strong,
            theme.text_muted,
            theme.bg_bar,
            theme.bg_input,
            theme.scale(11.),
        );
        let radius = theme.radius;
        let ui_family = theme.font_family.clone();
        let has_filter = self.active_result_filter(cx).is_some();

        // The combined field: `[ mode ▾ │ filter… ]` as one bordered unit, so the
        // active mode is always legible and obviously switchable (the old pair of
        // Ghost buttons read as two unrelated actions). Mirrors the Redis browse
        // search box; the container owns the chrome, the `bare()` input fills it.
        let selected_ix = FilterMode::ALL
            .iter()
            .position(|m| *m == bar.mode)
            .unwrap_or(0);
        let mut mode_select = Select::new("filter-mode").accent(false).seamless();
        for m in FilterMode::ALL.iter() {
            mode_select = mode_select.option(m.label());
        }
        // `Select`'s handlers take the index by value, so they can't be `cx.listener`s;
        // they go through a weak handle like the Redis search field's do.
        let toggle_view = cx.entity().downgrade();
        let select_view = cx.entity().downgrade();
        let mode_select = mode_select
            .selected(selected_ix)
            .open(bar.mode_open)
            .on_toggle(move |_, cx| {
                toggle_view
                    .update(cx, |this, cx| this.toggle_filter_mode_menu(cx))
                    .ok();
            })
            .on_select(move |ix, _, cx| {
                let Some(mode) = FilterMode::ALL.get(ix).copied() else {
                    return;
                };
                select_view
                    .update(cx, |this, cx| this.set_filter_mode(mode, cx))
                    .ok();
            });
        // Whichever box the mode shows. The `CodeEditor` paints its own `bg_app`
        // background, so the field matches it in `WHERE` mode rather than showing
        // two tones inside one border.
        let (box_el, field_bg) = match bar.mode {
            FilterMode::Contains => (
                div()
                    .flex_1()
                    .min_w(px(80.))
                    .px_2()
                    // The `bare()` input inherits the ambient text size, so set it
                    // here to match the mode label.
                    .text_size(size)
                    .child(bar.input.clone())
                    .into_any_element(),
                bg_input,
            ),
            FilterMode::Where => (
                div()
                    .flex_1()
                    .min_w(px(80.))
                    .h_full()
                    .child(bar.expr.clone())
                    .into_any_element(),
                theme.bg_app,
            ),
            // `Column` mode replaces the single box with the term builder:
            // `column ▾ │ op ▾ │ value`, plus a "+" that banks the term as a chip.
            FilterMode::Column => {
                let columns = self.filter_columns(cx);
                let ops = self.filter_ops(columns.get(bar.col_ix));

                let mut col_select = Select::new("filter-column").accent(false).seamless();
                for c in &columns {
                    col_select = col_select.option(c.name.clone());
                }
                let col_toggle = cx.entity().downgrade();
                let col_pick = cx.entity().downgrade();
                let col_select = col_select
                    .selected(bar.col_ix.min(columns.len().saturating_sub(1)))
                    .open(bar.col_open)
                    .on_toggle(move |_, cx| {
                        col_toggle
                            .update(cx, |this, cx| this.toggle_filter_column_menu(cx))
                            .ok();
                    })
                    .on_select(move |ix, _, cx| {
                        col_pick
                            .update(cx, |this, cx| this.set_filter_column(ix, cx))
                            .ok();
                    });

                let mut op_select = Select::new("filter-op").accent(false).seamless();
                for op in &ops {
                    op_select = op_select.option(op_label(*op));
                }
                let op_toggle = cx.entity().downgrade();
                let op_pick = cx.entity().downgrade();
                let ops_for_pick = ops.clone();
                let op_select = op_select
                    .selected(ops.iter().position(|o| *o == bar.op).unwrap_or(0))
                    .open(bar.op_open)
                    .on_toggle(move |_, cx| {
                        op_toggle
                            .update(cx, |this, cx| this.toggle_filter_op_menu(cx))
                            .ok();
                    })
                    .on_select(move |ix, _, cx| {
                        let Some(op) = ops_for_pick.get(ix).copied() else {
                            return;
                        };
                        op_pick
                            .update(cx, |this, cx| this.set_filter_op(op, cx))
                            .ok();
                    });

                let divider = || div().flex_shrink_0().w(px(1.)).h(px(14.)).bg(border);
                let el = div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .min_w(px(80.))
                    .child(col_select)
                    .child(divider())
                    .child(op_select)
                    // A unary operator compares against nothing, so the value box
                    // would be a lie; it's simply absent.
                    .when(bar.op.takes_value(), |d| {
                        d.child(divider()).child(
                            div()
                                .flex_1()
                                .min_w(px(60.))
                                .px_2()
                                .text_size(size)
                                .child(bar.value.clone()),
                        )
                    })
                    .when(!bar.op.takes_value(), |d| d.child(div().flex_1()))
                    .child(
                        IconButton::new(
                            "filter-term-add",
                            crate::icons::icon("plus", theme.scale(13.), muted),
                        )
                        .size(IconButtonSize::Sm)
                        .tooltip(crate::i18n::tr!(
                            "filter.add_this_term_to_the_filter_and",
                            "Add this term to the filter (AND)"
                        ))
                        .a11y_label(crate::i18n::tr!(
                            "filter.add_filter_term",
                            "Add filter term"
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.add_filter_term(cx))),
                    );
                (el.into_any_element(), bg_input)
            }
        };
        // Recall: the trailing clock opens this table's remembered filters. Shown
        // only once there is something to remember, so a first-time bar stays as
        // plain as it was. ↑/↓ in the box walk the same list (`recall_filter`).
        let history_btn = (!recent.is_empty()).then(|| {
            div()
                .relative()
                .flex_shrink_0()
                .child(
                    IconButton::new(
                        "filter-history",
                        crate::icons::icon("history", theme.scale(13.), muted),
                    )
                    .size(IconButtonSize::Sm)
                    .tooltip(crate::i18n::tr!(
                        "filter.recent_filters_for_this_table_in_the_box",
                        "Recent filters for this table (↑/↓ in the box)"
                    ))
                    .a11y_label(crate::i18n::tr!("filter.recent_filters", "Recent filters"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_filter_history(cx))),
                )
                // Invisible overlay recording the button's window rect, so the
                // list can hang off it. Re-renders only when the rect moves.
                .child(
                    gpui::canvas(
                        move |bounds, _, cx| {
                            clock_bounds.update(cx, |stored, cx| {
                                if *stored != Some(bounds) {
                                    *stored = Some(bounds);
                                    cx.notify();
                                }
                            });
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
        });
        let field = div()
            .relative()
            .flex()
            .flex_1()
            .items_center()
            .min_w(px(180.))
            .h(px(24.))
            .rounded(radius)
            .bg(field_bg)
            .border_1()
            .border_color(if bar.mode_open { border_strong } else { border })
            .overflow_hidden()
            .child(mode_select)
            .child(div().flex_shrink_0().w(px(1.)).h(px(14.)).bg(border))
            .child(box_el)
            .children(history_btn)
            .children(recall_panel);

        let mut row = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(4.))
            .font_family(ui_family)
            .text_size(size)
            .child(
                div()
                    .text_color(muted)
                    .child(crate::i18n::tr!("filter.filter", "Filter")),
            )
            .child(field)
            .child(
                Button::new("filter-apply", crate::i18n::tr!("filter.apply", "Apply"))
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.submit_filter(cx))),
            );
        if has_filter {
            row = row.child(
                Button::new("filter-clear", crate::i18n::tr!("filter.clear", "Clear"))
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.clear_result_filter(cx))),
            );
        }

        // `Column` mode's banked terms, one chip each with its own ✕. The
        // conjunction is the thing being edited, so it's shown in full rather than
        // elided into the toolbar chip.
        let term_chips = (bar.mode == FilterMode::Column && !bar.terms.is_empty()).then(|| {
            let mut strip = div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .px_2()
                .pb(px(4.));
            for (ix, term) in bar.terms.iter().enumerate() {
                strip = strip.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl_2()
                        .rounded(radius)
                        .bg(bg_input)
                        .border_1()
                        .border_color(border)
                        .child(cmp_as_expression(std::slice::from_ref(term)))
                        .child(
                            IconButton::new(
                                ("filter-term-remove", ix),
                                crate::icons::icon("close", theme.scale(11.), muted),
                            )
                            .size(IconButtonSize::Sm)
                            .tooltip(crate::i18n::tr!(
                                "filter.remove_this_term",
                                "Remove this term"
                            ))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_filter_term(ix, cx)),
                            ),
                        ),
                );
            }
            strip
        });

        // A rejected predicate (bad SQL, unknown column) leaves the grid showing
        // its "Query failed" panel; the bar owns the recovery, so the engine's
        // message lands here next to a one-click way back to the last good filter.
        let error_strip = self.filter_error(cx).map(|message| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .pb(px(4.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(80.))
                        .text_color(theme.red)
                        .child(message),
                )
                .child(
                    Button::new(
                        "filter-revert",
                        crate::i18n::tr!("filter.revert", "Revert filter"),
                    )
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .tooltip(crate::i18n::tr!(
                        "filter.re_apply_the_last_filter_that_returned_rows",
                        "Re-apply the last filter that returned rows"
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.revert_filter(cx))),
                )
        });

        Some(
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(border)
                .bg(bg)
                .child(row)
                .children(term_chips)
                .children(error_strip)
                .into_any_element(),
        )
    }
}

/// How the bar should show `applied`: the mode that owns it, its text (empty
/// outside the two text modes), and its built terms (empty outside `Column`).
/// `fallback` is the mode to use when nothing is applied.
///
/// An FK-follow `Eq` filter has no text form of its own, so it seeds
/// the `WHERE` box with the equivalent expression; applying that replaces it.
fn bar_seed(
    applied: Option<&ResultFilter>,
    fallback: FilterMode,
) -> (FilterMode, String, Vec<ColumnPredicate>) {
    match applied {
        Some(ResultFilter::Contains(t)) => (FilterMode::Contains, t.clone(), Vec::new()),
        Some(ResultFilter::Where(t)) => (FilterMode::Where, t.clone(), Vec::new()),
        Some(ResultFilter::Eq(pairs)) => (FilterMode::Where, eq_as_expression(pairs), Vec::new()),
        Some(ResultFilter::Cmp(preds)) => (FilterMode::Column, String::new(), preds.clone()),
        None => (fallback, String::new(), Vec::new()),
    }
}

/// The toolbar chip's text for an applied filter: the mode, then the term,
/// elided to [`CHIP_MAX_CHARS`]. An `Eq` (FK follow) or `Cmp` (built) filter
/// reads as the equivalent expression, so a narrowing nobody typed is legible too.
pub(crate) fn filter_summary(filter: &ResultFilter) -> String {
    let (label, text) = match filter {
        ResultFilter::Contains(t) => (FilterMode::Contains.label(), format!("\"{t}\"")),
        ResultFilter::Where(t) => (FilterMode::Where.label(), t.clone()),
        ResultFilter::Eq(pairs) => (FilterMode::Where.label(), eq_as_expression(pairs)),
        ResultFilter::Cmp(preds) => (FilterMode::Column.label(), cmp_as_expression(preds)),
    };
    format!("{label} {}", elide(&text.replace(['\n', '\r'], " ")))
}

/// The full, un-elided chip text, for the chip's tooltip.
pub(crate) fn filter_tooltip(filter: &ResultFilter) -> String {
    match filter {
        ResultFilter::Contains(t) => format!("Rows containing \"{t}\" in any text column"),
        ResultFilter::Where(t) => format!("WHERE {t}"),
        ResultFilter::Eq(pairs) => format!("WHERE {}", eq_as_expression(pairs)),
        ResultFilter::Cmp(preds) => format!("WHERE {}", cmp_as_expression(preds)),
    }
}

/// `col = literal [AND …]` for an FK-follow filter, for display and for seeding
/// the bar in `WHERE` mode. Display-only SQL: the *applied* `Eq` filter is
/// rendered by the driver (`eq_predicate`), which owns the escaping. Re-applying
/// the seeded text goes through `ResultFilter::Where`, trusted like editor SQL.
fn eq_as_expression(pairs: &[ColumnValue]) -> String {
    pairs
        .iter()
        .map(|p| format!("{} = {}", quote_ident(&p.column), literal(&p.value)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// The same display-only rendering for a *built* filter. Mirrors the driver's
/// `cmp_clause` (including its `= NULL` → `IS NULL` normalization) so what the
/// chip reads is what the engine was asked, but it is never sent anywhere: the
/// applied `Cmp` travels as structure and the driver renders it.
pub(crate) fn cmp_as_expression(preds: &[ColumnPredicate]) -> String {
    preds
        .iter()
        .map(|p| {
            let col = quote_ident(&p.column);
            let null_valued = !p.op.takes_value() || matches!(p.value, None | Some(Value::Null));
            match (p.op, null_valued) {
                (CmpOp::IsNull, _) | (CmpOp::Eq, true) => format!("{col} IS NULL"),
                (CmpOp::IsNotNull, _) | (CmpOp::Ne, true) => format!("{col} IS NOT NULL"),
                // Reads as the pattern it becomes, so a chip says what will match.
                // The engine also casts the column, but that's noise in a chip.
                (CmpOp::Contains, _) => {
                    format!("{col} LIKE '%{}%'", plain_text(p.value.as_ref()))
                }
                (op, _) => format!(
                    "{col} {} {}",
                    op_sql(op),
                    literal(p.value.as_ref().unwrap_or(&Value::Null))
                ),
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// The SQL spelling of a [`CmpOp`] for display (the driver owns the real one).
fn op_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Like | CmpOp::Contains => "LIKE",
        CmpOp::IsNull => "IS NULL",
        CmpOp::IsNotNull => "IS NOT NULL",
    }
}

/// A value as the bare, unquoted text a `Contains` term searches for — the
/// display mirror of the driver's own `value_text`.
fn plain_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Text(t)) => t.replace('\'', "''"),
        Some(Value::Integer(i)) => i.to_string(),
        Some(Value::Real(r)) => r.to_string(),
        _ => String::new(),
    }
}

/// Double-quote an identifier for the seeded expression, doubling any embedded
/// quote. Every engine RED speaks accepts double-quoted identifiers here except
/// MySQL in its default mode, where the user edits the seed before applying.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A [`Value`] as a SQL literal for the seeded expression (single quotes doubled).
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(t) => format!("'{}'", t.replace('\'', "''")),
        Value::Blob(_) => "'<blob>'".to_string(),
        // Only a prefix of the value was ever read, so it can't be written as a
        // faithful literal; the seed is a starting point the user edits.
        Value::Capped(c) => format!("'{}'", c.head.replace('\'', "''")),
    }
}

/// A cell's value as it reads in the "Filter by" menu: the SQL literal it will
/// be compared against, elided so a long text cell can't stretch the menu.
pub(crate) fn value_label(value: &Value) -> String {
    elide(&literal(value).replace(['\n', '\r'], " "))
}

/// Elide `text` to [`CHIP_MAX_CHARS`] on a char boundary, with a trailing ellipsis.
fn elide(text: &str) -> String {
    if text.chars().count() <= CHIP_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(CHIP_MAX_CHARS - 1).collect();
    format!("{}…", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn mode_labels_are_stable() {
        assert_eq!(FilterMode::Contains.label(), "Contains");
        assert_eq!(FilterMode::Where.label(), "WHERE");
        assert_eq!(FilterMode::Column.label(), "Column");
        assert_eq!(FilterMode::ALL.len(), 3);
        assert_eq!(FilterMode::default(), FilterMode::Contains);
    }

    #[test]
    fn summary_names_the_mode() {
        assert_eq!(
            filter_summary(&ResultFilter::Contains("acme".into())),
            "Contains \"acme\""
        );
        assert_eq!(
            filter_summary(&ResultFilter::Where("amount > 100".into())),
            "WHERE amount > 100"
        );
    }

    #[test]
    fn summary_elides_and_flattens() {
        let long = ResultFilter::Where("x".repeat(200));
        let s = filter_summary(&long);
        // "WHERE " + CHIP_MAX_CHARS worth of text at most.
        assert!(s.chars().count() <= CHIP_MAX_CHARS + 6, "{s}");
        assert!(s.ends_with('…'));
        let multiline = ResultFilter::Where("a >\n1".into());
        assert_eq!(filter_summary(&multiline), "WHERE a > 1");
    }

    #[test]
    fn eq_filter_reads_as_an_expression() {
        let pairs = vec![
            ColumnValue {
                column: "tier_id".into(),
                value: Value::Integer(3),
                decl_type: None,
            },
            ColumnValue {
                column: "re\"gion".into(),
                value: Value::Text(Arc::from("it's north")),
                decl_type: None,
            },
        ];
        assert_eq!(
            eq_as_expression(&pairs),
            "\"tier_id\" = 3 AND \"re\"\"gion\" = 'it''s north'"
        );
        // Exactly `CHIP_MAX_CHARS` of expression, so the chip shows all of it.
        assert_eq!(
            filter_summary(&ResultFilter::Eq(pairs.clone())),
            "WHERE \"tier_id\" = 3 AND \"re\"\"gion\" = 'it''s north'"
        );
        // One column more and the chip elides, keeping the mode prefix legible.
        let mut longer = pairs;
        longer.push(ColumnValue {
            column: "status".into(),
            value: Value::Text(Arc::from("active")),
            decl_type: None,
        });
        let summary = filter_summary(&ResultFilter::Eq(longer));
        assert!(summary.starts_with("WHERE \"tier_id\" = 3"), "{summary}");
        assert!(summary.ends_with('…'), "{summary}");
        assert!(summary.chars().count() <= CHIP_MAX_CHARS + 6, "{summary}");
    }

    /// A `ColumnPredicate` shorthand for the `Cmp` display tests.
    fn cp(column: &str, op: CmpOp, value: Option<Value>) -> ColumnPredicate {
        ColumnPredicate {
            column: column.into(),
            op,
            value,
        }
    }

    #[test]
    fn built_filter_reads_as_an_expression() {
        let preds = vec![
            cp("amount", CmpOp::Gt, Some(Value::Integer(100))),
            cp("re\"gion", CmpOp::Like, Some(Value::Text(Arc::from("no%")))),
        ];
        assert_eq!(
            cmp_as_expression(&preds),
            "\"amount\" > 100 AND \"re\"\"gion\" LIKE 'no%'"
        );
        // The chip names `Column`, the mode that built it, not `WHERE`.
        assert!(
            filter_summary(&ResultFilter::Cmp(preds.clone())).starts_with("Column "),
            "{}",
            filter_summary(&ResultFilter::Cmp(preds))
        );
    }

    #[test]
    fn built_filter_display_normalizes_null_comparisons() {
        // Mirrors the driver's `cmp_clause`, so the chip can't claim `= NULL` while
        // the engine was asked `IS NULL`.
        for value in [None, Some(Value::Null)] {
            assert_eq!(
                cmp_as_expression(&[cp("a", CmpOp::Eq, value.clone())]),
                "\"a\" IS NULL"
            );
            assert_eq!(
                cmp_as_expression(&[cp("a", CmpOp::Ne, value)]),
                "\"a\" IS NOT NULL"
            );
        }
        assert_eq!(
            cmp_as_expression(&[cp("a", CmpOp::IsNotNull, None)]),
            "\"a\" IS NOT NULL"
        );
    }

    #[test]
    fn operator_metadata_matches_its_spelling() {
        // The unary pair takes no value; only `LIKE` is text-only, so a numeric
        // column keeps every other operator.
        assert!(!CmpOp::IsNull.takes_value() && !CmpOp::IsNotNull.takes_value());
        assert!(CmpOp::Eq.takes_value() && CmpOp::Like.takes_value());
        assert_eq!(
            CmpOp::ALL.iter().filter(|o| o.text_only()).count(),
            1,
            "only LIKE is withheld from numeric columns"
        );
        assert_eq!(op_label(CmpOp::Ne), "<>");
        assert_eq!(op_label(CmpOp::IsNull), "IS NULL");
    }

    #[test]
    fn value_label_elides_a_long_cell() {
        let short = value_label(&Value::Text(Arc::from("acme")));
        assert_eq!(short, "'acme'");
        let long = value_label(&Value::Text(Arc::from("x".repeat(200))));
        assert!(long.ends_with('…'), "{long}");
        assert!(long.chars().count() <= CHIP_MAX_CHARS, "{long}");
        // A multi-line cell stays on one line in a menu label.
        assert_eq!(value_label(&Value::Text(Arc::from("a\nb"))), "'a b'");
    }

    #[test]
    fn seeding_the_bar_picks_the_mode_that_owns_the_filter() {
        let preds = vec![cp("a", CmpOp::Eq, Some(Value::Integer(1)))];
        let (mode, text, terms) =
            bar_seed(Some(&ResultFilter::Cmp(preds.clone())), FilterMode::Where);
        assert_eq!(mode, FilterMode::Column);
        assert!(text.is_empty());
        assert_eq!(terms, preds);

        let (mode, text, terms) = bar_seed(
            Some(&ResultFilter::Contains("x".into())),
            FilterMode::Column,
        );
        assert_eq!((mode, text.as_str()), (FilterMode::Contains, "x"));
        assert!(terms.is_empty());

        // An FK-follow `Eq` has no text form, so it seeds `WHERE` with the
        // equivalent expression rather than being silently dropped.
        let pairs = vec![ColumnValue {
            column: "tier_id".into(),
            value: Value::Integer(3),
            decl_type: None,
        }];
        let (mode, text, _) = bar_seed(Some(&ResultFilter::Eq(pairs)), FilterMode::Contains);
        assert_eq!(
            (mode, text.as_str()),
            (FilterMode::Where, "\"tier_id\" = 3")
        );

        // Nothing applied falls back to the caller's mode.
        assert_eq!(bar_seed(None, FilterMode::Column).0, FilterMode::Column);
    }

    #[test]
    fn only_text_modes_are_recallable() {
        // The recent-filters store keys on the mode tag, and `Column` has no text
        // to store, so it must never be recorded (see `submit_filter`).
        assert!(FilterMode::Contains.is_text() && FilterMode::Where.is_text());
        assert!(!FilterMode::Column.is_text());
        // Tags round-trip, so a saved filter still resolves after a restart.
        for m in FilterMode::ALL {
            assert_eq!(FilterMode::from_tag(m.tag()), Some(m));
        }
        assert_eq!(FilterMode::from_tag("nope"), None);
    }
}
