//! The read-only DDL view: one schema object's `CREATE` statement.
//!
//! Red could describe an object's columns long before it could show you what the
//! object *is*. For a view or a routine that distinction is the whole object, so
//! this fills a real hole rather than adding a convenience: the definition is the
//! thing being browsed (see `docs/plans/todo/object-ddl-and-schema-diff.md`).
//!
//! The driver does the engine-specific work (`DatabaseDriver::object_ddl`):
//! `SHOW CREATE` verbatim on MySQL / SQLite / ClickHouse, a catalog assembly with
//! a scope header on Postgres, which has no such statement. This module is purely
//! the UI: the tab body, its lifecycle, and the two terminal-event handlers.
//!
//! Nothing here executes anything. "Open as query" pastes the text into an
//! ordinary query tab, so a `CREATE`/`DROP` that a user chooses to run passes
//! through the same risk assessment, destructive confirm, and read-only lock as
//! any hand-typed statement. A DDL tab is a reader, and the safety rails stay
//! where they already are.

use flint::prelude::*;
use flint::{Button, ButtonSize, ButtonVariant, ToastVariant};
use gpui::{Context, SharedString, div, prelude::*, px};
use red_core::ObjectKind;
use red_service::Command;

use crate::app::{AppState, Phase, TabView};

/// One open DDL view, owned by its [`crate::app::QueryTab`].
pub(crate) struct DdlView {
    /// Identifies this request; a reply for a different epoch (the tab was
    /// closed and another opened) is dropped.
    pub epoch: red_service::Epoch,
    pub namespace: String,
    pub name: String,
    pub kind: ObjectKind,
    pub state: DdlState,
}

pub(crate) enum DdlState {
    Loading,
    /// The definition, as the engine (or the Postgres assembler) rendered it.
    /// Held both as text (for Copy / Open as query) and as a read-only
    /// `CodeEditor`, which is what makes the body selectable and SQL-highlighted;
    /// the same seam the cell inspector uses for a shown value.
    Ready {
        text: String,
        editor: gpui::Entity<CodeEditor>,
    },
    /// No privilege, object dropped, or a kind this engine cannot define.
    Failed(String),
}

impl AppState {
    /// Open (or re-focus) the DDL tab for one object.
    ///
    /// Re-focusing rather than re-opening keeps a second right-click from
    /// stacking duplicate tabs of the same definition, matching how the ER
    /// diagram treats its namespace.
    pub(crate) fn open_object_ddl(
        &mut self,
        namespace: String,
        name: String,
        kind: ObjectKind,
        cx: &mut Context<Self>,
    ) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        if let Some(i) = active.tabs.iter().position(|t| {
            t.ddl()
                .is_some_and(|d| d.namespace == namespace && d.name == name && d.kind == kind)
        }) {
            self.set_active_tab(i, cx);
            return;
        }

        let epoch = crate::result::new_epoch();
        let mut tab = crate::app::QueryTab::new(format!("DDL: {name}"), cx);
        tab.view = Some(TabView::Ddl(DdlView {
            epoch,
            namespace: namespace.clone(),
            name: name.clone(),
            kind,
            state: DdlState::Loading,
        }));
        self.push_tab(tab, cx);
        self.send_active(Command::ObjectDdl {
            epoch,
            namespace,
            name,
            kind,
        });
        cx.notify();
    }

    /// A definition arrived: route it to the tab holding that epoch.
    pub(crate) fn on_object_ddl_ready(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        ddl: String,
        cx: &mut Context<Self>,
    ) {
        // Built once here, not per frame: a wide table's DDL is a real string and
        // an editor entity rebuilt every render would churn selection too.
        let editor = cx.new(|cx| {
            let mut e = CodeEditor::new(cx)
                .highlighter(crate::sql::tokenize)
                .gutter(false)
                .resting_border(false)
                .corner_radius(px(0.))
                .a11y_label("Object definition")
                .with_content(ddl.clone());
            e.set_read_only(true, cx);
            e
        });
        if let Some(view) = self.ddl_by_epoch(session, epoch) {
            view.state = DdlState::Ready { text: ddl, editor };
        }
    }

    /// The fetch failed: shown in the tab, not as a toast. The user asked this
    /// specific question, so the answer belongs where they asked it.
    pub(crate) fn on_object_ddl_failed(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        message: String,
    ) {
        if let Some(view) = self.ddl_by_epoch(session, epoch) {
            view.state = DdlState::Failed(message);
        }
    }

    fn ddl_by_epoch(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
    ) -> Option<&mut DdlView> {
        self.conn_mut(session)?
            .tabs
            .iter_mut()
            .find_map(|t| match &mut t.view {
                Some(TabView::Ddl(d)) if d.epoch == epoch => Some(d),
                _ => None,
            })
    }

    /// The DDL tab body: a header with the object's identity and the actions,
    /// then the definition in a monospaced, selectable, scrollable pane.
    pub(crate) fn render_ddl(
        &self,
        active: &crate::app::ActiveConn,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme().clone();
        let Some(view) = active.tabs.get(tab_idx).and_then(|t| t.ddl()) else {
            return div().into_any_element();
        };

        let title: SharedString = format!("{}.{}", view.namespace, view.name).into();
        let (icon, color) = crate::schema::object_icon(view.kind, cx);
        let ddl_text = match &view.state {
            DdlState::Ready { text, .. } => Some(text.clone()),
            _ => None,
        };

        let mut header = div()
            .flex_shrink_0()
            .h(px(30.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(crate::icons::icon(icon, theme.scale(13.), color))
            .child(
                div()
                    .font_family(theme.mono_family.clone())
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(title.clone()),
            )
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_faint)
                    .child(view.kind.as_str()),
            );

        if let Some(ddl) = ddl_text.clone() {
            let copy = ddl.clone();
            header = header.child(
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("ddl-copy", "Copy")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.copy_to_clipboard(copy.clone(), "DDL copied", cx);
                            })),
                    )
                    .child(
                        Button::new("ddl-open-query", "Open as query")
                            .size(ButtonSize::Sm)
                            .variant(ButtonVariant::Ghost)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ddl_open_as_query(ddl.clone(), cx);
                            })),
                    ),
            );
        }

        let body = match &view.state {
            DdlState::Loading => div()
                .p_3()
                .text_size(theme.scale(11.5))
                .text_color(theme.text_faint)
                .child("loading…")
                .into_any_element(),
            DdlState::Failed(message) => div()
                .p_3()
                .text_size(theme.scale(11.5))
                .text_color(theme.red)
                .child(message.clone())
                .into_any_element(),
            // The read-only editor owns its own scrolling and selection, so this
            // is a plain full-size container rather than a scroll wrapper.
            DdlState::Ready { editor, .. } => {
                div().size_full().child(editor.clone()).into_any_element()
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_panel)
            .child(header)
            .child(div().flex_1().min_h(px(0.)).child(body))
            .into_any_element()
    }

    /// Paste a definition into a fresh query tab. Deliberately does not run it:
    /// everything destructive about DDL is governed by the rails on the run path.
    fn ddl_open_as_query(&mut self, ddl: String, cx: &mut Context<Self>) {
        self.new_query(cx);
        let editor = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => tab.editor.clone(),
                None => return,
            },
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(ddl, cx));
        self.notify(
            ToastVariant::Info,
            "Definition opened in a query tab. Nothing has run.",
            cx,
        );
        cx.notify();
    }
}

// --- schema comparison -------------------------------------------------------

/// A finished schema comparison, shown as a whole-half tab body.
///
/// The data-diff report ([`crate::diff_view`]) hangs off the connection because
/// it is about two tables' rows; this is about two schemas' shape, and lives in a
/// tab for the same reason the ER diagram does: you keep it open and read it.
pub(crate) struct SchemaDiffView {
    pub left: String,
    pub right: String,
    pub delta: red_core::schema_diff::SchemaDelta,
    /// Whether the generated script includes destructive statements. Off by
    /// default, and the script comments them out until it is on.
    pub include_drops: bool,
    pub scroll: gpui::ScrollHandle,
}

impl AppState {
    /// A comparison finished: open it in a tab.
    pub(crate) fn on_schema_diff(
        &mut self,
        left: String,
        right: String,
        delta: red_core::schema_diff::SchemaDelta,
        cx: &mut Context<Self>,
    ) {
        let title = format!("Diff: {left} ↔ {right}");
        let mut tab = crate::app::QueryTab::new(title, cx);
        tab.view = Some(TabView::SchemaDiff(SchemaDiffView {
            left,
            right,
            delta,
            include_drops: false,
            scroll: gpui::ScrollHandle::new(),
        }));
        self.push_tab(tab, cx);
        cx.notify();
    }

    pub(crate) fn on_schema_diff_failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.notify(ToastVariant::Error, message, cx);
    }

    pub(crate) fn toggle_schema_diff_drops(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && let Some(TabView::SchemaDiff(v)) = active.active_mut().and_then(|t| t.view.as_mut())
        {
            v.include_drops = !v.include_drops;
        }
        cx.notify();
    }

    /// The comparison report.
    pub(crate) fn render_schema_diff(
        &self,
        active: &crate::app::ActiveConn,
        tab_idx: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use red_core::schema_diff::{Confidence, ScriptScope};

        let theme = cx.theme().clone();
        let Some(TabView::SchemaDiff(v)) = active.tabs.get(tab_idx).and_then(|t| t.view.as_ref())
        else {
            return div().into_any_element();
        };
        let size_11 = theme.scale(11.);

        let header = div()
            .flex_shrink_0()
            .h(px(30.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(theme.scale(12.))
                    .text_color(theme.text)
                    .child(SharedString::from(format!("{} ↔ {}", v.left, v.right))),
            )
            .child(
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_faint)
                    .child(format!("{} difference(s)", v.delta.count())),
            )
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new(
                            "schema-diff-drops",
                            if v.include_drops {
                                "Drops included"
                            } else {
                                "Additive only"
                            },
                        )
                        .variant(if v.include_drops {
                            ButtonVariant::Secondary
                        } else {
                            ButtonVariant::Ghost
                        })
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_schema_diff_drops(cx))),
                    )
                    .child(
                        Button::new("schema-diff-script", "Open script as query")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.open_schema_diff_script(cx)),
                            ),
                    ),
            );

        // Cross-engine comparisons compare type *classes*, not spellings. Saying so
        // is the difference between a report you can trust and one you cannot.
        let banner = v.delta.cross_engine.then(|| {
            div()
                .px_2()
                .py_1()
                .text_size(theme.scale(10.5))
                .text_color(theme.yellow)
                .child(
                    "Different engines: columns were compared by type class, not spelling. \
                     Check types before applying anything.",
                )
        });

        // Collected as (text, colour, monospaced) and rendered below, rather than
        // pushed from a closure that would hold a borrow for the whole block.
        let mut lines: Vec<(String, gpui::Hsla, bool)> = Vec::new();
        for obj in &v.delta.objects_added {
            lines.push((
                format!("+ {} ({})", obj.name, obj.kind.as_str()),
                theme.green,
                true,
            ));
        }
        for obj in &v.delta.objects_removed {
            lines.push((
                format!("- {} ({})", obj.name, obj.kind.as_str()),
                theme.red,
                true,
            ));
        }
        for t in &v.delta.tables_changed {
            lines.push((t.name.clone(), theme.text, true));
            for c in &t.columns_added {
                lines.push((format!("    + {}", c.name), theme.green, true));
            }
            for c in &t.columns_removed {
                lines.push((format!("    - {}", c.name), theme.red, true));
            }
            for c in &t.columns_changed {
                let suffix = match c.confidence {
                    Confidence::Certain => String::new(),
                    Confidence::Uncertain => "  (uncertain)".to_string(),
                };
                lines.push((
                    format!("    ~ {}: {}{suffix}", c.left.name, c.summary),
                    theme.yellow,
                    true,
                ));
            }
            for i in &t.indexes_added {
                lines.push((format!("    + index {}", i.name), theme.green, true));
            }
            for i in &t.indexes_removed {
                lines.push((format!("    - index {}", i.name), theme.red, true));
            }
            for f in &t.fks_added {
                lines.push((
                    format!("    + fk {} → {}", f.column, f.ref_table),
                    theme.green,
                    true,
                ));
            }
            for f in &t.fks_removed {
                lines.push((
                    format!("    - fk {} → {}", f.column, f.ref_table),
                    theme.red,
                    true,
                ));
            }
        }
        if lines.is_empty() {
            lines.push(("The two schemas match.".to_string(), theme.green, false));
        }
        let rows: Vec<gpui::AnyElement> = lines
            .into_iter()
            .map(|(text, colour, mono)| {
                let mut d = div()
                    .px_2()
                    .py(px(1.))
                    .text_size(size_11)
                    .text_color(colour);
                if mono {
                    d = d.font_family(theme.mono_family.clone());
                }
                d.child(text).into_any_element()
            })
            .collect();
        let _ = ScriptScope::Additive;

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg_panel)
            .child(header)
            .children(banner)
            .child(
                div()
                    .id("schema-diff-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .track_scroll(&v.scroll)
                    .py_1()
                    .children(rows),
            )
            .into_any_element()
    }

    /// Generate the reconciling DDL and drop it in a query tab. As everywhere else
    /// in this module: text, not execution.
    fn open_schema_diff_script(&mut self, cx: &mut Context<Self>) {
        use red_core::schema_diff::ScriptScope;

        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let Some(TabView::SchemaDiff(v)) = active.active().and_then(|t| t.view.as_ref()) else {
            return;
        };
        let scope = if v.include_drops {
            ScriptScope::IncludeDrops
        } else {
            ScriptScope::Additive
        };
        // Quoted for the *left* engine: the script moves the left schema toward the
        // right, so it is the left server that would run it.
        let kind = active.config.kind;
        let quote = move |ident: &str| match kind {
            red_core::DbKind::Mysql => format!("`{}`", ident.replace('`', "``")),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        };
        let sql = v.delta.to_sql(&v.left, scope, &quote);
        self.new_query(cx);
        let editor = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => tab.editor.clone(),
                None => return,
            },
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql, cx));
        self.notify(
            ToastVariant::Info,
            "Script opened in a query tab. Nothing has run.",
            cx,
        );
        cx.notify();
    }
}
