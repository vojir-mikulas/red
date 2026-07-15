//! Connection-import wizard + commit (`docs/plans/connection-import.md`).
//!
//! The parse/decrypt core lives in [`crate::import`]; this is the app-side glue.
//! A two-step wizard: pick which tools to import from (DBeaver / DBGate), scan
//! them, then tick which discovered connections to include. On confirm the chosen
//! connections are committed into RED's store the same way a form save does
//! (keychain-routed secrets, never the config file).

use flint::prelude::*;
use gpui::{Context, Div, ElementId, Stateful, div, prelude::*, px};
use red_core::ConnectionConfig;

use crate::config::StoredConnection;
use crate::import::discover::{Found, detect};
use crate::import::{ImportSource, ImportedConnection};

use super::AppState;

/// The tools the wizard can import from, probed in this order. Only the ones
/// actually detected on this machine make it into the wizard — a tool that isn't
/// installed here is never shown.
const PROVIDERS: [ImportSource; 5] = [
    ImportSource::DBeaver,
    ImportSource::DBGate,
    ImportSource::DataGrip,
    ImportSource::RedisInsight,
    ImportSource::CredentialFiles,
];

/// A provider row in the wizard's source step: the tool, the on-disk sources
/// auto-detection found for it (never empty — undetected tools are dropped), and
/// whether the user picked it to scan.
pub(crate) struct WizardProvider {
    pub(crate) source: ImportSource,
    pub(crate) found: Vec<Found>,
    pub(crate) selected: bool,
}

/// One discovered connection in the select step, carrying its include flag.
pub(crate) struct WizardItem {
    /// The source group it came from (e.g. a DBeaver project label), for the
    /// grouped list header.
    pub(crate) group: String,
    pub(crate) imported: ImportedConnection,
    /// Whether to import it (defaults on; forced off + locked for a duplicate).
    pub(crate) include: bool,
    /// Already present in RED — shown locked so a re-import can't double-add it.
    pub(crate) duplicate: bool,
}

/// A connection the scan couldn't map, surfaced read-only so nothing is dropped
/// silently.
pub(crate) struct WizardSkip {
    pub(crate) name: String,
    pub(crate) reason: String,
}

/// Which step of the import wizard is on screen.
pub(crate) enum WizardStep {
    /// Choose which providers to scan.
    Source,
    /// Pick which discovered connections to import.
    Select {
        items: Vec<WizardItem>,
        skipped: Vec<WizardSkip>,
    },
}

/// The connection-import wizard: pick a source (DBeaver/DBGate), scan it, then
/// choose which discovered connections to import. Held on [`AppState`] while open.
pub(crate) struct ImportWizard {
    pub(crate) providers: Vec<WizardProvider>,
    pub(crate) step: WizardStep,
}

impl AppState {
    /// Open the import wizard: probe both providers and show the source step.
    /// When neither tool is installed, an info toast instead of an empty wizard.
    pub(crate) fn open_import_wizard(&mut self, cx: &mut Context<Self>) {
        let all = detect();
        // Only surface tools that were actually detected here — a tool that isn't
        // installed never appears in the wizard.
        let providers: Vec<WizardProvider> = PROVIDERS
            .iter()
            .filter_map(|&source| {
                let found: Vec<Found> =
                    all.iter().filter(|f| f.source == source).cloned().collect();
                (!found.is_empty()).then_some(WizardProvider {
                    source,
                    // Everything shown is detected, so it starts ticked.
                    selected: true,
                    found,
                })
            })
            .collect();
        if providers.is_empty() {
            self.notify(
                ToastVariant::Info,
                "No connections from other database tools were found on this machine.",
                cx,
            );
            return;
        }
        self.import_wizard = Some(ImportWizard {
            providers,
            step: WizardStep::Source,
        });
        self.focus_modal = true;
        cx.notify();
    }

    /// Source step: toggle whether provider `index` is included in the scan.
    pub(crate) fn set_import_provider(&mut self, index: usize, on: bool, cx: &mut Context<Self>) {
        if let Some(w) = self.import_wizard.as_mut()
            && let Some(p) = w.providers.get_mut(index)
            && !p.found.is_empty()
        {
            p.selected = on;
            cx.notify();
        }
    }

    /// Scan every selected provider and advance to the connection-selection step.
    /// A source that can't be read at all surfaces as a toast; per-connection
    /// problems land in the skipped list, never aborting the scan.
    pub(crate) fn import_wizard_scan(&mut self, cx: &mut Context<Self>) {
        // Snapshot the selected sources first, releasing the wizard borrow so the
        // scan can read `self.connections` and toast through `&mut self`.
        let sources: Vec<Found> = {
            let Some(w) = self.import_wizard.as_ref() else {
                return;
            };
            w.providers
                .iter()
                .filter(|p| p.selected)
                .flat_map(|p| p.found.iter().cloned())
                .collect()
        };

        let mut items: Vec<WizardItem> = Vec::new();
        let mut skipped: Vec<WizardSkip> = Vec::new();
        let mut read_errors: Vec<String> = Vec::new();
        for found in &sources {
            match crate::import::run(found.source, &found.dir) {
                Ok(report) => {
                    for imported in report.imported {
                        let duplicate = self
                            .connections
                            .iter()
                            .any(|c| same_target(&c.config, &imported.config));
                        items.push(WizardItem {
                            group: found.label.clone(),
                            imported,
                            include: !duplicate,
                            duplicate,
                        });
                    }
                    for (name, reason) in report.skipped {
                        skipped.push(WizardSkip { name, reason });
                    }
                }
                Err(e) => read_errors.push(format!("{}: {e}", found.label)),
            }
        }

        for err in read_errors {
            self.notify(ToastVariant::Error, format!("Couldn't read {err}"), cx);
        }
        if items.is_empty() && skipped.is_empty() {
            self.notify(
                ToastVariant::Info,
                "No connections found in the selected sources.",
                cx,
            );
            return;
        }
        if let Some(w) = self.import_wizard.as_mut() {
            w.step = WizardStep::Select { items, skipped };
        }
        self.focus_modal = true;
        cx.notify();
    }

    /// Select step: toggle whether discovered connection `index` is imported.
    pub(crate) fn set_import_include(&mut self, index: usize, on: bool, cx: &mut Context<Self>) {
        if let Some(ImportWizard {
            step: WizardStep::Select { items, .. },
            ..
        }) = self.import_wizard.as_mut()
            && let Some(item) = items.get_mut(index)
            && !item.duplicate
        {
            item.include = on;
            cx.notify();
        }
    }

    /// Select step: include (or exclude) every non-duplicate connection at once —
    /// the "select all" master toggle. Duplicates stay locked.
    pub(crate) fn set_import_include_all(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(ImportWizard {
            step: WizardStep::Select { items, .. },
            ..
        }) = self.import_wizard.as_mut()
        {
            for item in items.iter_mut().filter(|it| !it.duplicate) {
                item.include = on;
            }
            cx.notify();
        }
    }

    /// Return from the select step to the source step (to re-pick providers).
    pub(crate) fn import_wizard_back(&mut self, cx: &mut Context<Self>) {
        if let Some(w) = self.import_wizard.as_mut() {
            w.step = WizardStep::Source;
            cx.notify();
        }
    }

    /// Apply the wizard's selection: add each ticked, non-duplicate connection,
    /// routing its secrets to the keychain, then persist and summarize.
    pub(crate) fn commit_import(&mut self, cx: &mut Context<Self>) {
        // Only the select step has anything to commit; anything else closes cleanly.
        let items = match self.import_wizard.take() {
            Some(ImportWizard {
                step: WizardStep::Select { items, .. },
                ..
            }) => items,
            other => {
                self.import_wizard = other;
                return;
            }
        };

        let mut added = 0usize;
        for item in items {
            if !item.include || item.duplicate {
                continue;
            }
            let mut config = item.imported.config;
            // Defensive: two ticked entries could target the same coordinates, or
            // the list could be stale — never double-add.
            if self
                .connections
                .iter()
                .any(|c| same_target(&c.config, &config))
            {
                continue;
            }
            // Split secrets off before the config is stored: they go to the OS
            // keychain, never `connections.toml` (mirrors `save_form`).
            let password = std::mem::take(&mut config.password);
            let ssh_secrets = config.ssh.as_mut().map(|s| {
                (
                    std::mem::take(&mut s.password),
                    std::mem::take(&mut s.passphrase),
                )
            });
            let is_file = config.kind.is_file();
            self.connections.push(StoredConnection {
                id: crate::config::new_id(),
                config,
                last_accessed: None,
                pinned: false,
            });
            let index = self.connections.len() - 1;
            self.store_credential(index, &password, is_file, cx);
            self.store_ssh_credentials(index, ssh_secrets, cx);
            added += 1;
        }

        self.persist(cx);
        self.rebuild_switcher(cx);
        self.refocus_root = true;

        let (variant, msg) = if added == 0 {
            (
                ToastVariant::Info,
                "Nothing selected to import.".to_string(),
            )
        } else {
            (
                ToastVariant::Success,
                format!("Imported {added} connection{}.", plural(added)),
            )
        };
        self.notify(variant, msg, cx);
        cx.notify();
    }

    /// Dismiss the wizard without importing.
    pub(crate) fn cancel_import(&mut self, cx: &mut Context<Self>) {
        self.import_wizard = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// Render the import wizard's current step as a modal.
    pub(crate) fn render_import_wizard(
        &self,
        wizard: &ImportWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        match &wizard.step {
            WizardStep::Source => self.render_import_source(wizard, cx).into_any_element(),
            WizardStep::Select { items, skipped } => self
                .render_import_select(items, skipped, cx)
                .into_any_element(),
        }
    }

    /// Step 1: pick which detected tools to scan. Each is a dense, clickable row
    /// with a checkbox; only tools found on this machine reach this list.
    fn render_import_source(
        &self,
        wizard: &ImportWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();

        let any_selected = wizard.providers.iter().any(|p| p.selected);

        let mut list = div().flex().flex_col();
        for (i, provider) in wizard.providers.iter().enumerate() {
            let hint = if provider.found.len() == 1 {
                provider.found[0].dir.display().to_string()
            } else {
                format!("{} sources found", provider.found.len())
            };
            let selected = provider.selected;
            list = list.child(
                dense_row(("import-provider-row", i), i, true, theme)
                    .child(
                        import_checkbox(("import-provider", i), selected, theme)
                            .label(provider.source.label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(div().text_color(theme.text).child(provider.source.label()))
                            .child(
                                div()
                                    .text_size(theme.scale(11.5))
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(hint),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_import_provider(i, !selected, cx)
                    })),
            );
        }

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child("Choose which tools to import saved connections from."),
            )
            .child(list_frame(theme).child(list));

        let footer = wizard_footer(theme).child(div()).child(
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("import-cancel", "Cancel")
                        .variant(ButtonVariant::Secondary)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_import(cx))),
                )
                .child(
                    Button::new("import-scan", "Continue")
                        .variant(ButtonVariant::Primary)
                        .disabled(!any_selected)
                        .on_click(cx.listener(|this, _, _, cx| this.import_wizard_scan(cx))),
                ),
        );

        Modal::new("import-wizard")
            .title("Import connections")
            .width(px(520.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_import(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.import_wizard_scan(cx))
                    .ok();
            })
            .child(body)
    }

    /// Step 2: tick which discovered connections to import, grouped by source.
    /// Duplicates already in RED are shown locked; unmappable ones are listed
    /// under a "Skipped" section so nothing vanishes silently.
    fn render_import_select(
        &self,
        items: &[WizardItem],
        skipped: &[WizardSkip],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();

        let selected = items
            .iter()
            .filter(|it| it.include && !it.duplicate)
            .count();
        // "Select all" only governs the importable (non-duplicate) rows; it reads
        // as ticked once every one of them is in, and is dead when there are none.
        let selectable = items.iter().filter(|it| !it.duplicate).count();
        let all_selected = selectable > 0 && selected == selectable;

        let mut list = div().flex().flex_col();
        let mut current_group: Option<&str> = None;
        for (i, item) in items.iter().enumerate() {
            if current_group != Some(item.group.as_str()) {
                current_group = Some(item.group.as_str());
                list = list.child(section_header(&item.group, theme));
            }
            let name_color = if item.duplicate {
                theme.text_faint
            } else {
                theme.text
            };
            let mut info = div().flex().flex_col().gap_0p5().min_w_0().flex_1().child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(name_color)
                            .child(item.imported.source_name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.scale(11.))
                            .text_color(theme.text_faint)
                            .child(item.imported.config.kind.to_string()),
                    ),
            );
            let target = target_summary(&item.imported.config);
            if !target.is_empty() {
                info = info.child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_muted)
                        .truncate()
                        .child(target),
                );
            }
            if item.duplicate {
                info = info.child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_faint)
                        .child("Already added"),
                );
            } else if let Some(warning) = &item.imported.warning {
                info = info.child(
                    div()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.orange)
                        .child(format!("⚠ {warning}")),
                );
            }
            // Duplicates are locked: the box shows off + disabled and the row can't
            // be toggled. Everything else is a clickable dense row.
            let checked = item.include && !item.duplicate;
            let mut row = dense_row(("import-item-row", i), i, !item.duplicate, theme).child(
                import_checkbox(("import-item", i), checked, theme)
                    .disabled(item.duplicate)
                    .label(item.imported.source_name.clone()),
            );
            if !item.duplicate {
                row = row.on_click(
                    cx.listener(move |this, _, _, cx| this.set_import_include(i, !checked, cx)),
                );
            }
            list = list.child(row.child(info));
        }

        // Skipped connections, each with its reason: the "nothing is dropped
        // silently" contract, made visible.
        if !skipped.is_empty() {
            list = list.child(section_header(
                &format!("Skipped ({})", skipped.len()),
                theme,
            ));
            for s in skipped {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_faint)
                        .child(div().min_w_0().truncate().child(s.name.clone()))
                        .child(div().flex_shrink_0().child(s.reason.clone())),
                );
            }
        }

        // A master row above the list: a "Select all" checkbox on the left, the
        // running count on the right.
        let mut select_all = div()
            .id("import-select-all")
            .flex()
            .items_center()
            .gap_2()
            .rounded(theme.radius_sm)
            .child(
                import_checkbox("import-select-all-box", all_selected, theme)
                    .disabled(selectable == 0),
            )
            .child(div().text_color(theme.text_muted).child("Select all"));
        if selectable > 0 {
            select_all =
                select_all
                    .cursor_pointer()
                    .tab_index(0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_import_include_all(!all_selected, cx)
                    }));
        }
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(select_all)
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("{selected} of {} selected.", items.len())),
            );

        let body = div().flex().flex_col().gap_2().child(header).child(
            list_frame(theme)
                .id("import-list")
                .max_h(px(420.))
                .overflow_y_scroll()
                .child(list),
        );

        let footer = wizard_footer(theme)
            .child(
                Button::new("import-back", "Back")
                    .variant(ButtonVariant::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.import_wizard_back(cx))),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("import-cancel", "Cancel")
                            .variant(ButtonVariant::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_import(cx))),
                    )
                    .child(
                        Button::new("import-confirm", format!("Import {selected}"))
                            .variant(ButtonVariant::Primary)
                            .disabled(selected == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.commit_import(cx))),
                    ),
            );

        Modal::new("import-wizard")
            .title("Select connections")
            .width(px(560.))
            .focus_handle(self.modal_focus.clone())
            .footer(footer)
            .on_close(move |_, cx| {
                close_view
                    .update(cx, |this, cx| this.cancel_import(cx))
                    .ok();
            })
            .on_confirm(move |_, cx| {
                confirm_view
                    .update(cx, |this, cx| this.commit_import(cx))
                    .ok();
            })
            .child(body)
    }
}

/// A dense, file-row style list row: checkbox + content on one compact line,
/// hairline-separated from the row above, the whole row a click target when
/// `clickable`. Callers add the checkbox, the content, and (if clickable) an
/// `on_click`.
fn dense_row(
    id: impl Into<ElementId>,
    index: usize,
    clickable: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let row = div()
        .id(id)
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_1p5()
        // A hairline between rows (not before the first), so the list reads as
        // stacked rows without a heavy grid.
        .when(index > 0, |d| d.border_t_1().border_color(theme.border));
    if clickable {
        row.cursor_pointer()
            .tab_index(0)
            .hover(|s| s.bg(theme.bg_hover))
            .focus(|s| s.bg(theme.bg_hover))
    } else {
        row
    }
}

/// A wizard-list checkbox with RED's Lucide "check" mark (masked to `on_accent`),
/// so the tick matches the rest of the app's line icons rather than a font glyph.
fn import_checkbox(id: impl Into<ElementId>, checked: bool, theme: &Theme) -> Checkbox {
    Checkbox::new(id, checked).mark(crate::icons::icon("check", px(12.), theme.on_accent))
}

/// The framed container the dense rows sit in: a quiet inset panel with a
/// hairline border, matching the app's other list surfaces.
fn list_frame(theme: &Theme) -> Div {
    div()
        .rounded(theme.radius_sm)
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_input)
        .overflow_hidden()
}

/// The wizard's footer bar: full width so the leading control (Back / a spacer)
/// sits hard left and the action group hard right, with one uniform gap in each
/// cluster. Callers add exactly two children (left, right).
fn wizard_footer(_theme: &Theme) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
}

/// A bold, uppercase group header for the select step's connection list, sitting
/// flush inside the framed list with the rows' horizontal padding.
fn section_header(label: &str, theme: &Theme) -> gpui::Div {
    div()
        .px_3()
        .pt_2p5()
        .pb_1()
        .bg(theme.bg_bar)
        .text_size(theme.scale(10.5))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
}

/// Two connections address the same target; the dedupe key for import. File
/// engines compare by path (host/port/user are empty); network engines by the
/// full coordinate tuple.
fn same_target(a: &ConnectionConfig, b: &ConnectionConfig) -> bool {
    a.kind == b.kind
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.port == b.port
        && a.database == b.database
        && a.user == b.user
}

/// A one-line "where does it point" summary for a preview row.
fn target_summary(config: &ConnectionConfig) -> String {
    if config.kind.is_file() {
        return config.database.clone();
    }
    let mut s = config.host.clone();
    if let Some(port) = config.port {
        s.push_str(&format!(":{port}"));
    }
    if !config.database.is_empty() {
        s.push('/');
        s.push_str(&config.database);
    }
    s
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
