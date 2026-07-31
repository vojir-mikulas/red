//! The RedisJSON inspector's render layer: the tree, the raw view, and the
//! action bar. A second `impl AppState` block split out of the state half, the
//! same way `kvbrowse/render.rs` is split from `kvbrowse/mod.rs`. Items come
//! from the parent module (`use super::*`).

use flint::prelude::*;
use gpui::{Context, SharedString, div, prelude::*, px};
use red_core::kv::{JsonDoc, JsonPath};
use red_service::SessionId;

use crate::app::AppState;

use super::*;

impl AppState {
    /// The JSON value body: the tree or the raw text of the selected node, plus
    /// the breadcrumb and the action bar.
    pub(crate) fn render_kv_json(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        doc: &JsonDoc,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let json = &inspector.json;
        let text_size = theme.scale(11.5);
        let dim = theme.text_muted;
        let selected = json.selected.clone().unwrap_or_default();
        let view = cx.entity().downgrade();

        let header = {
            let (tree_view, raw_view) = (view.clone(), view.clone());
            let raw = json.raw;
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(theme.scale(10.5))
                        .text_color(dim)
                        .child(json_breadcrumb(&selected)),
                )
                .child(
                    div()
                        .text_size(theme.scale(10.))
                        .text_color(dim)
                        .child(crate::fmt::human_bytes(doc.bytes())),
                )
                .child(
                    Button::new("kv-json-tree", "Tree")
                        .variant(if raw {
                            ButtonVariant::Secondary
                        } else {
                            ButtonVariant::Primary
                        })
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            tree_view
                                .update(cx, |this, cx| this.kv_json_set_raw(session, false, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-json-raw", "Raw")
                        .variant(if raw {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        })
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            raw_view
                                .update(cx, |this, cx| this.kv_json_set_raw(session, true, cx))
                                .ok();
                        }),
                )
        };

        let body = if json.raw {
            self.render_kv_json_raw(session, inspector, theme, cx)
        } else {
            self.render_kv_json_tree(session, inspector, theme, cx)
        };

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(header)
            .when_some(json.error.clone(), |d, err| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .px_2()
                        .py_1()
                        .text_size(text_size)
                        .text_color(theme.red)
                        .child(err),
                )
            })
            .child(body)
            .child(self.render_kv_json_actions(session, inspector, writable, theme, cx))
            .into_any_element()
    }

    /// The lazy tree itself: one row per read child, indented by depth.
    fn render_kv_json_tree(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let json = &inspector.json;
        let rows = self.json_rows(json);
        let text_size = theme.scale(11.5);
        let dim = theme.text_muted;
        let mono = theme.mono_family.clone();
        let selected = json.selected.clone().unwrap_or_default();
        let view = cx.entity().downgrade();

        // The root level is requested on open; until it lands there is nothing
        // to draw but the reason why.
        if !json.nodes.contains_key(&JsonPath::root()) {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(text_size)
                .text_color(dim)
                .child(if json.loading.contains(&JsonPath::root()) {
                    "Reading the document's root…"
                } else {
                    "This document has no structure to show"
                })
                .into_any_element();
        }

        let mut list = div()
            .id("kv-json-tree")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .py_1();
        for (i, row) in rows.iter().enumerate() {
            if row.more {
                let (more_view, more_path) = (view.clone(), row.path.clone());
                list = list.child(
                    div().pl(px(20. + row.depth as f32 * 14.)).py(px(2.)).child(
                        Button::new(
                            SharedString::from(format!("kv-json-more-{i}")),
                            row.label.clone(),
                        )
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .disabled(row.loading)
                        .on_click(move |_, _, cx| {
                            more_view
                                .update(cx, |this, cx| {
                                    this.kv_json_load_more(session, more_path.clone(), cx)
                                })
                                .ok();
                        }),
                    ),
                );
                continue;
            }
            let (toggle_view, select_view) = (view.clone(), view.clone());
            let (toggle_path, select_path) = (row.path.clone(), row.path.clone());
            let is_selected = row.path == selected;
            let color = if row.kind.is_container() {
                theme.text
            } else {
                theme.blue
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("kv-json-row-{i}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .h(px(20.))
                    .pl(px(6. + row.depth as f32 * 14.))
                    .pr_2()
                    .when(is_selected, |d| d.bg(theme.accent.opacity(0.12)))
                    .hover(|d| d.bg(theme.bg_elevated))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        select_view
                            .update(cx, |this, cx| {
                                this.kv_json_select(session, select_path.clone(), cx)
                            })
                            .ok();
                    })
                    .child(
                        // The chevron is its own hit target so opening a node
                        // and selecting it stay separate gestures.
                        div()
                            .id(SharedString::from(format!("kv-json-chevron-{i}")))
                            .w(px(14.))
                            .flex()
                            .justify_center()
                            .text_size(theme.scale(9.))
                            .text_color(dim)
                            .when(row.expandable, |d| {
                                d.cursor_pointer().on_click(move |_, _, cx| {
                                    toggle_view
                                        .update(cx, |this, cx| {
                                            this.kv_json_toggle(session, toggle_path.clone(), cx)
                                        })
                                        .ok();
                                })
                            })
                            .child(match (row.expandable, row.expanded, row.loading) {
                                (_, _, true) => "\u{2026}",
                                (true, true, _) => "\u{25be}",
                                (true, false, _) => "\u{25b8}",
                                _ => "",
                            }),
                    )
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(text_size)
                            .text_color(color)
                            .child(row.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(mono.clone())
                            .text_size(text_size)
                            .text_color(dim)
                            .truncate()
                            .child(row.detail.clone()),
                    ),
            );
        }
        list.into_any_element()
    }

    /// The raw view: the selected node's serialized JSON, read-only or in the
    /// editor.
    fn render_kv_json_raw(
        &self,
        _session: SessionId,
        inspector: &KvInspector,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let json = &inspector.json;
        let text_size = theme.scale(11.5);
        let mono = theme.mono_family.clone();
        let _ = cx;
        if json.editing.is_some() {
            return div()
                .id("kv-json-edit")
                .flex_1()
                .min_h(px(0.))
                .font_family(mono)
                .text_size(text_size)
                .line_height(text_size * 1.5)
                .child(inspector.value_editor.clone())
                .into_any_element();
        }
        let Some(value) = &json.raw_text else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(text_size)
                .text_color(theme.text_muted)
                .child("Loading\u{2026}")
                .into_any_element();
        };
        let (body, summary, _wrap) =
            crate::inspector::format_value_body(value, crate::inspector::ValueFormat::Json);
        div()
            .id("kv-json-raw-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_2()
            .child(div().font_family(mono).text_size(text_size).child(body))
            .when(matches!(value, red_core::Value::Capped(_)), |d| {
                d.child(
                    div()
                        .pt_1()
                        .text_size(theme.scale(10.))
                        .text_color(theme.text_muted)
                        .child(summary),
                )
            })
            .into_any_element()
    }

    /// The JSON action bar: copy, edit, delete, scoped to the selected node.
    fn render_kv_json_actions(
        &self,
        session: SessionId,
        inspector: &KvInspector,
        writable: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let json = &inspector.json;
        let view = cx.entity().downgrade();
        let editing = json.editing.is_some();
        // Only an uncapped body can be edited: saving a truncated head back
        // would replace the node with its own prefix.
        let editable = matches!(json.raw_text, Some(red_core::Value::Text(_)));
        let selected = json.selected.clone().unwrap_or_default();

        let bar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border);

        if editing {
            let (save_view, cancel_view) = (view.clone(), view.clone());
            return bar
                .child(
                    Button::new("kv-json-save", "Save")
                        .variant(ButtonVariant::Primary)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            save_view
                                .update(cx, |this, cx| this.kv_json_submit_edit(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-json-cancel", "Cancel")
                        .variant(ButtonVariant::Secondary)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_json_cancel_edit(session, cx))
                                .ok();
                        }),
                )
                .into_any_element();
        }

        let (path_view, value_view, edit_view, del_view) =
            (view.clone(), view.clone(), view.clone(), view.clone());
        bar.child(
            Button::new("kv-json-copy-path", "Copy path")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    path_view
                        .update(cx, |this, cx| this.kv_json_copy_path(session, cx))
                        .ok();
                }),
        )
        .child(
            Button::new("kv-json-copy-value", "Copy value")
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Sm)
                .disabled(json.raw_text.is_none())
                .on_click(move |_, _, cx| {
                    value_view
                        .update(cx, |this, cx| this.kv_json_copy_value(session, cx))
                        .ok();
                }),
        )
        .when(writable, |d| {
            d.child(
                Button::new("kv-json-edit", "Edit")
                    .size(ButtonSize::Sm)
                    .disabled(!editable)
                    .on_click(move |_, _, cx| {
                        edit_view
                            .update(cx, |this, cx| this.kv_json_start_edit(session, cx))
                            .ok();
                    }),
            )
            .child(
                Button::new(
                    "kv-json-delete",
                    if selected.is_root() {
                        "Delete key"
                    } else {
                        "Delete node"
                    },
                )
                .variant(ButtonVariant::Danger)
                .size(ButtonSize::Sm)
                .on_click(move |_, _, cx| {
                    del_view
                        .update(cx, |this, cx| this.kv_json_delete_node(session, cx))
                        .ok();
                }),
            )
        })
        .into_any_element()
    }
}
