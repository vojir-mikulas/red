//! Connection-import wizard + commit (`docs/plans/connection-import.md`).
//!
//! The parse/decrypt core lives in [`crate::import`]; this is the app-side glue.
//! A two-step wizard: pick which tools to import from (DBeaver / DBGate), scan
//! them, then tick which discovered connections to include. On confirm the chosen
//! connections are committed into RED's store the same way a form save does
//! (keychain-routed secrets, never the config file).

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context};
use red_core::ConnectionConfig;

use crate::config::StoredConnection;
use crate::import::discover::{detect, Found};
use crate::import::{ImportSource, ImportedConnection};

use super::AppState;

/// The tools the wizard offers, always shown in the source step so the user sees
/// what RED can import from even when a tool isn't installed here.
const PROVIDERS: [ImportSource; 2] = [ImportSource::DBeaver, ImportSource::DBGate];

/// A provider row in the wizard's source step: the tool, the on-disk sources
/// auto-detection found for it (empty = not installed here), and whether the user
/// picked it to scan.
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
        let providers: Vec<WizardProvider> = PROVIDERS
            .iter()
            .map(|&source| {
                let found: Vec<Found> =
                    all.iter().filter(|f| f.source == source).cloned().collect();
                WizardProvider {
                    source,
                    // Detected providers start ticked; a missing one can't be picked.
                    selected: !found.is_empty(),
                    found,
                }
            })
            .collect();
        if providers.iter().all(|p| p.found.is_empty()) {
            self.notify(
                ToastVariant::Info,
                "No DBeaver or DBGate connections found on this machine.",
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
        if let Some(w) = self.import_wizard.as_mut() {
            if let Some(p) = w.providers.get_mut(index) {
                if !p.found.is_empty() {
                    p.selected = on;
                    cx.notify();
                }
            }
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
        {
            if let Some(item) = items.get_mut(index) {
                if !item.duplicate {
                    item.include = on;
                    cx.notify();
                }
            }
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
    ) -> impl IntoElement {
        match &wizard.step {
            WizardStep::Source => self.render_import_source(wizard, cx).into_any_element(),
            WizardStep::Select { items, skipped } => self
                .render_import_select(items, skipped, cx)
                .into_any_element(),
        }
    }

    /// Step 1: pick which tools to import from. Each provider is a toggle row,
    /// disabled (and left unticked) when that tool isn't installed here.
    fn render_import_source(
        &self,
        wizard: &ImportWizard,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();

        let any_selected = wizard
            .providers
            .iter()
            .any(|p| p.selected && !p.found.is_empty());

        let mut list = div().flex().flex_col().gap_2();
        for (i, provider) in wizard.providers.iter().enumerate() {
            let installed = !provider.found.is_empty();
            let hint = if !installed {
                "Not found on this machine".to_string()
            } else if provider.found.len() == 1 {
                provider.found[0].dir.display().to_string()
            } else {
                format!("{} sources found", provider.found.len())
            };
            let name_color = if installed {
                theme.text
            } else {
                theme.text_faint
            };
            let row = div()
                .flex()
                .items_center()
                .gap_3()
                .py_1()
                .px_2()
                .rounded(theme.radius_sm)
                .when(installed, |d| d.bg(theme.bg_input))
                .child(
                    Toggle::new(("import-provider", i), provider.selected)
                        .disabled(!installed)
                        .label(provider.source.label())
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            this.set_import_provider(i, *checked, cx)
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(div().text_color(name_color).child(provider.source.label()))
                        .child(
                            div()
                                .text_size(theme.scale(11.5))
                                .text_color(theme.text_muted)
                                .child(hint),
                        ),
                );
            list = list.child(row);
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
            .child(list);

        let footer = div()
            .flex()
            .justify_end()
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
            );

        Modal::new("import-wizard")
            .title("Import connections")
            .width(px(460.))
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
    ) -> impl IntoElement {
        let theme = cx.theme();
        let close_view = cx.entity().downgrade();
        let confirm_view = cx.entity().downgrade();

        let selected = items
            .iter()
            .filter(|it| it.include && !it.duplicate)
            .count();

        let mut list = div().flex().flex_col().gap_1();
        let mut current_group: Option<&str> = None;
        for (i, item) in items.iter().enumerate() {
            if current_group != Some(item.group.as_str()) {
                current_group = Some(item.group.as_str());
                list = list.child(section_header(&item.group, theme));
            }
            let mut info = div().flex().flex_col().gap_0p5().child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_color(if item.duplicate {
                                theme.text_faint
                            } else {
                                theme.text
                            })
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
            let row = div()
                .flex()
                .items_start()
                .gap_3()
                .py_1()
                .child(
                    Toggle::new(("import-item", i), item.include && !item.duplicate)
                        .disabled(item.duplicate)
                        .label(item.imported.source_name.clone())
                        .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                            this.set_import_include(i, *checked, cx)
                        })),
                )
                .child(info);
            list = list.child(row);
        }

        // Skipped connections, each with its reason: the "nothing is dropped
        // silently" contract, made visible.
        if !skipped.is_empty() {
            list = list.child(
                div()
                    .pt_2()
                    .text_size(theme.scale(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_faint)
                    .child(format!("SKIPPED ({})", skipped.len())),
            );
            for s in skipped {
                list = list.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .py_0p5()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text_faint)
                        .child(div().child(s.name.clone()))
                        .child(div().flex_shrink_0().child(s.reason.clone())),
                );
            }
        }

        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("{selected} of {} selected.", items.len())),
            )
            .child(
                div()
                    .id("import-list")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .p_2()
                    .rounded(theme.radius_sm)
                    .bg(theme.bg_input)
                    .child(list),
            );

        let footer = div()
            .flex()
            .items_center()
            .justify_between()
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
            .width(px(480.))
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

/// A bold, uppercase group header for the select step's connection list.
fn section_header(label: &str, theme: &Theme) -> gpui::Div {
    div()
        .pt_2()
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
    if n == 1 {
        ""
    } else {
        "s"
    }
}
