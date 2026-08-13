//! The transfer wizard: one stepped modal that builds a
//! [`TransferPlan`](red_core::transfer::TransferPlan) and runs it.
//!
//! RED could already stream one table into another and one schema into another
//! database, but neither flow could say "duplicate this database, these two
//! tables empty, that one filtered". This is the surface that can. Every step
//! reads and writes the *same* plan value, so going back and forth is lossless
//! and the Review step is a pure render of what will happen.
//!
//! Layout: this file owns the state and the transitions; [`render`] owns the
//! modal shell, the step rail and the per-step bodies. The streaming execution
//! is entirely in `red-service` (`dispatch::transfer`); nothing here knows what
//! engine is underneath, only `red-core` types.

mod render;

use flint::prelude::*;
use gpui::{App, AppContext, Context, Entity};
use red_core::CopyMode;
use red_core::transfer::{
    ItemAction, ItemContent, ItemOutcome, ItemReport, ItemSource, OnError, PlanIssue, TransferItem,
    TransferOptions, TransferPlan, TransferSummary,
};
use red_service::{Command, OpId, SessionId};

use crate::app::{AppState, Phase};
use crate::schema::SchemaState;

/// One step of the wizard, in rail order. Which of these an invocation has
/// depends on how it was opened: `Duplicate table…` is Content plus Review,
/// a schema-level copy is all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Where it lands: the connection and namespace.
    Destination,
    /// The checklist: which objects, and data vs structure vs skip.
    Objects,
    /// Per-item depth: rename, action, row shaping, the column/DDL disclosure.
    Content,
    /// Job-wide options and the summary that must be read before any write.
    Review,
    /// The run itself, and then the report.
    Progress,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Destination => "Destination",
            Step::Objects => "Objects",
            Step::Content => "Content",
            Step::Review => "Review",
            Step::Progress => "Progress",
        }
    }
}

/// A namespace a transfer can be pointed at: one row of the Destination step.
#[derive(Clone)]
pub(crate) struct Destination {
    pub session: SessionId,
    pub conn_name: String,
    pub kind: red_core::DbKind,
    pub namespace: String,
    /// The objects already in it, so the Objects step can resolve each item to
    /// `Create` or `Existing` without a round trip.
    pub objects: Vec<String>,
    /// True when this is the connection the wizard was opened from. A
    /// same-connection transfer is the common case and sorts first.
    pub same_connection: bool,
}

/// A transfer that is running (or has finished): the Progress step's state.
pub(crate) struct TransferRun {
    pub id: OpId,
    /// The target names the plan had when the run started, positionally. The
    /// terminal summary arrives in *execution* order, so this is how a report is
    /// matched back to its row.
    pub plan_names: Vec<String>,
    /// One slot per planned item, filled as `TransferItemDone` arrives. Indexed
    /// by the item's position in `plan.items`, not by execution order, so the
    /// list stays where the user last saw it.
    pub reports: Vec<Option<ItemReport>>,
    /// The item currently streaming and how far it has got.
    pub current: Option<(String, u64)>,
    /// Rows committed across the whole job.
    pub rows: u64,
    /// Set once the job reaches a terminal state.
    pub outcome: Option<RunOutcome>,
}

/// A dry run's answer: the script the plan would execute and a per-item row
/// estimate. Held until the plan changes, because a script rendered against the
/// previous plan is a lie about the current one.
pub(crate) struct DryRun {
    pub script: String,
    pub estimates: Vec<(String, Option<u64>)>,
}

/// How a run ended, for the report the Progress step becomes.
pub(crate) enum RunOutcome {
    Finished(TransferSummary),
    Failed { message: String },
    Cancelled,
}

/// The wizard while it is open. Held on [`AppState`] as one `Option`, like the
/// connection-import wizard it is modelled on.
pub(crate) struct TransferWizard {
    /// The steps this invocation has, in rail order. The rail only ever shows
    /// the steps that actually apply (see [`AppState::open_transfer`]).
    pub steps: Vec<Step>,
    pub current: usize,
    /// The single value every step reads and writes.
    pub plan: TransferPlan,
    /// The connection the items are read from.
    pub source: SessionId,
    pub source_label: String,
    pub destinations: Vec<Destination>,
    /// Index into `destinations`; `None` until one is chosen.
    pub destination: Option<usize>,
    /// Objects step: the filter box.
    pub filter: Entity<TextInput>,
    /// Content step: which item's pane is showing.
    pub focused: usize,
    /// Content step editors. One set, re-seeded when the focused item changes,
    /// because only one item is ever being edited.
    pub rename: Entity<TextInput>,
    pub where_expr: Entity<TextInput>,
    pub limit: Entity<TextInput>,
    /// Whether the "Columns and DDL" disclosure is open. Auto-opened when the
    /// focused item has something worth looking at.
    pub disclosure: bool,
    /// `Duplicate database…`: the name of the namespace to create first.
    pub new_namespace: Option<Entity<TextInput>>,
    /// Review step: the name to save this plan under.
    pub plan_name: Entity<TextInput>,
    /// Validation issues from the last edit, so the rail can badge the step that
    /// owns each one.
    pub issues: Vec<PlanIssue>,
    /// The dry run's answer, shown on the Review step until the plan changes.
    pub dry_run: Option<DryRun>,
    pub run: Option<TransferRun>,
    /// A message the last action produced (a refused namespace, a saved plan).
    pub note: Option<String>,
    /// Keeps the Content step's editors wired to [`AppState::sync_transfer_editors`].
    /// The plan is the state of record, so every keystroke lands in it rather than
    /// waiting for a button nobody should have to find. Dropping the wizard drops
    /// these, which is what unsubscribes them.
    _editor_subs: Vec<gpui::Subscription>,
}

impl TransferWizard {
    /// The step on screen.
    pub(crate) fn step(&self) -> Step {
        self.steps
            .get(self.current)
            .copied()
            .unwrap_or(Step::Review)
    }

    /// The destination the plan is pointed at.
    pub(crate) fn target(&self) -> Option<&Destination> {
        self.destination.and_then(|i| self.destinations.get(i))
    }

    /// Items in the order the Objects step lists them, after the filter box.
    /// Returns plan indices so a filtered row still edits the right item.
    pub(crate) fn visible_items(&self, filter: &str) -> Vec<usize> {
        let needle = filter.trim().to_ascii_lowercase();
        (0..self.plan.items.len())
            .filter(|&i| {
                needle.is_empty()
                    || self.plan.items[i]
                        .source_label()
                        .to_ascii_lowercase()
                        .contains(&needle)
            })
            .collect()
    }

    /// Whether the plan is runnable. `Transfer` is live as soon as this is true,
    /// on any step: the rail is navigation, not a gate.
    pub(crate) fn runnable(&self) -> bool {
        self.destination.is_some()
            && self.run.is_none()
            && red_core::transfer::validate(&self.plan).is_ok()
    }

    /// The issues that belong to `step`, so the rail can badge it.
    pub(crate) fn issues_for(&self, step: Step) -> usize {
        self.issues
            .iter()
            .filter(|issue| match step {
                Step::Destination => issue.item.is_none() && self.destination.is_none(),
                Step::Objects | Step::Content => issue.item.is_some(),
                _ => false,
            })
            .count()
    }

    /// Re-validate after an edit, and drop a stale dry run: a script rendered
    /// against the previous plan would be a lie about the current one.
    fn revalidate(&mut self) {
        self.issues = red_core::transfer::validate(&self.plan)
            .err()
            .unwrap_or_default();
        self.dry_run = None;
    }
}

/// How the wizard was opened, which decides its steps and its starting plan.
pub(crate) enum TransferEntry {
    /// One table, into a namespace the user picks.
    Table { schema: String, name: String },
    /// One table, into its own namespace under a new name.
    DuplicateTable { schema: String, name: String },
    /// Every table in a namespace, into a namespace the user picks.
    Database { schema: String },
    /// Every table in a namespace, into a namespace created on the way.
    DuplicateDatabase { schema: String },
    /// The focused result (its filter and sort included).
    Result { epoch: red_service::Epoch },
    /// A `SELECT` typed or selected in the editor.
    Sql(String),
    /// A saved plan, reopened verbatim. Its items are whatever the file says,
    /// not whatever the tree currently shows.
    Plan(Box<TransferPlan>),
}

impl AppState {
    /// Open the wizard for `entry`. Returns without opening (with a toast) when
    /// there is nothing to transfer or nowhere to put it, because an empty
    /// wizard is a worse answer than a sentence explaining why.
    pub(crate) fn open_transfer(&mut self, entry: TransferEntry, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        let source = active.session;
        let source_label = active.config.name.clone();
        let schema_state = active.schema.read(cx);

        let (source_namespace, items, mut steps) = match &entry {
            TransferEntry::Table { schema, name } => (
                Some(schema.clone()),
                vec![table_item(schema, name, name)],
                vec![Step::Destination, Step::Content, Step::Review],
            ),
            TransferEntry::DuplicateTable { schema, name } => (
                Some(schema.clone()),
                vec![table_item(schema, name, &format!("{name}_copy"))],
                vec![Step::Content, Step::Review],
            ),
            TransferEntry::Database { schema } => {
                let names = tables_in(schema_state, schema);
                let items = names
                    .iter()
                    .map(|n| table_item(schema, n, n))
                    .collect::<Vec<_>>();
                (
                    Some(schema.clone()),
                    items,
                    vec![
                        Step::Destination,
                        Step::Objects,
                        Step::Content,
                        Step::Review,
                    ],
                )
            }
            TransferEntry::DuplicateDatabase { schema } => {
                let names = tables_in(schema_state, schema);
                let items = names
                    .iter()
                    .map(|n| table_item(schema, n, n))
                    .collect::<Vec<_>>();
                (
                    Some(schema.clone()),
                    items,
                    vec![
                        Step::Destination,
                        Step::Objects,
                        Step::Content,
                        Step::Review,
                    ],
                )
            }
            TransferEntry::Result { epoch } => (
                active.namespace.clone(),
                vec![TransferItem {
                    source: ItemSource::Result { epoch: epoch.get() },
                    target_name: String::new(),
                    action: ItemAction::Create,
                    content: ItemContent::AllRows,
                    mapping: Vec::new(),
                }],
                vec![Step::Destination, Step::Content, Step::Review],
            ),
            TransferEntry::Sql(sql) => (
                active.namespace.clone(),
                vec![TransferItem {
                    source: ItemSource::Sql(sql.clone()),
                    target_name: String::new(),
                    action: ItemAction::Create,
                    content: ItemContent::AllRows,
                    mapping: Vec::new(),
                }],
                vec![Step::Destination, Step::Content, Step::Review],
            ),
            TransferEntry::Plan(plan) => (
                plan.source_namespace.clone(),
                plan.items.clone(),
                vec![
                    Step::Destination,
                    Step::Objects,
                    Step::Content,
                    Step::Review,
                ],
            ),
        };
        steps.push(Step::Progress);

        if items.is_empty() {
            self.notify(ToastVariant::Info, "Nothing here to transfer.", cx);
            return;
        }

        let destinations = self.transfer_destinations(Some(source), cx);
        if destinations.is_empty() {
            self.notify(
                ToastVariant::Error,
                "No writable database to transfer into. Open one first.",
                cx,
            );
            return;
        }

        // A `Duplicate…` fixes its own namespace, so its Destination step is
        // skipped and the plan points at where it already is. A reopened plan
        // names the namespace it was saved against.
        let duplicate_ns = match &entry {
            TransferEntry::DuplicateTable { schema, .. } => Some(schema.clone()),
            TransferEntry::Plan(plan) => plan.target_namespace.clone(),
            _ => None,
        };
        let destination = duplicate_ns.as_ref().and_then(|ns| {
            destinations
                .iter()
                .position(|d| d.session == source && d.namespace.eq_ignore_ascii_case(ns))
        });

        let filter = cx.new(|cx| TextInput::new(cx).with_placeholder("Filter tables…"));
        let rename = cx.new(|cx| TextInput::new(cx).with_placeholder("Target table name"));
        let where_expr = cx.new(|cx| {
            TextInput::new(cx).with_placeholder("created_at > now() - interval '30 days'")
        });
        let limit = cx.new(|cx| TextInput::new(cx).with_placeholder("1000"));
        let new_namespace = matches!(entry, TransferEntry::DuplicateDatabase { .. }).then(|| {
            let seed = match &entry {
                TransferEntry::DuplicateDatabase { schema } => format!("{schema}_copy"),
                _ => String::new(),
            };
            cx.new(|cx| {
                TextInput::new(cx)
                    .with_placeholder("New database name")
                    .with_content(seed)
            })
        });

        let plan_name = cx.new(|cx| TextInput::new(cx).with_placeholder("Nightly refresh"));
        let mut wizard = TransferWizard {
            steps,
            current: 0,
            plan: TransferPlan {
                source_namespace,
                target_namespace: duplicate_ns,
                items,
                options: TransferOptions::default(),
            },
            source,
            source_label,
            destinations,
            destination,
            filter,
            focused: 0,
            rename,
            where_expr,
            limit,
            disclosure: false,
            new_namespace,
            plan_name,
            issues: Vec::new(),
            dry_run: None,
            run: None,
            note: None,
            _editor_subs: Vec::new(),
        };
        // A `Duplicate…` fixes its own namespace and skips the Destination step,
        // but only if that namespace really is a legal target: dropping the step
        // when nothing resolved would leave the wizard unable to ask.
        if !wizard.steps.contains(&Step::Destination) && wizard.destination.is_none() {
            wizard.steps.insert(0, Step::Destination);
        }
        if wizard.step() == Step::Destination && wizard.destination.is_some() {
            wizard.current = 1;
        }
        // A reopened plan keeps the actions it was saved with; a fresh one
        // resolves them against what the destination already holds.
        let resolve = !matches!(entry, TransferEntry::Plan(_));
        let options = match &entry {
            TransferEntry::Plan(plan) => plan.options.clone(),
            _ => TransferOptions::default(),
        };
        wizard.plan.options = options;
        // `set_content` deliberately does not emit `Change`, so re-seeding the
        // editors on a focus change cannot echo back through these.
        wizard._editor_subs = [&wizard.rename, &wizard.where_expr, &wizard.limit]
            .into_iter()
            .map(|editor| {
                cx.subscribe(editor, |this, _, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Change) {
                        this.sync_transfer_editors(cx);
                    }
                })
            })
            .collect();
        wizard.revalidate();
        self.transfer = Some(wizard);
        self.seed_transfer_editors(cx);
        if resolve {
            self.resolve_transfer_actions(cx);
        }
        self.focus_modal = true;
        cx.notify();
    }

    /// The namespace a schema-level transfer would read from: the tree's
    /// selection, or the connection's only namespace. `None` when there is no
    /// connection or nothing selected to transfer.
    pub(crate) fn transfer_schema_source(&self, cx: &App) -> Option<String> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let state = active.schema.read(cx);
        let selected = match &state.selected {
            Some(crate::schema::NodeId::Schema(name)) => Some(name.clone()),
            Some(crate::schema::NodeId::Group { schema, .. })
            | Some(crate::schema::NodeId::Object { schema, .. })
            | Some(crate::schema::NodeId::Column { schema, .. }) => Some(schema.clone()),
            None => None,
        }
        .or_else(|| (state.schemas.len() == 1).then(|| state.schemas[0].name.clone()))?;
        (!tables_in(state, &selected).is_empty()).then_some(selected)
    }

    /// The focused result's epoch, if there is one to transfer.
    pub(crate) fn transfer_result_epoch(&self) -> Option<red_service::Epoch> {
        match &self.phase {
            Phase::Connected(active) => active.active_result().map(|g| g.epoch),
            _ => None,
        }
    }

    /// F5: transfer whatever the schema tree has selected. A selected table
    /// means that table; anything else means its namespace, which is the level
    /// the key reads as in every tool that binds it.
    pub(crate) fn open_transfer_for_selection(&mut self, cx: &mut Context<Self>) {
        let selected = match &self.phase {
            Phase::Connected(active) => active.schema.read(cx).selected.clone(),
            _ => None,
        };
        match selected {
            Some(crate::schema::NodeId::Object { schema, name }) => {
                self.open_transfer(TransferEntry::Table { schema, name }, cx)
            }
            _ => self.open_schema_transfer(cx),
        }
    }

    /// `schema: migrate to…`: the whole selected namespace, through the wizard,
    /// so "all of it" and "most of it" are the same gesture.
    pub(crate) fn open_schema_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(schema) = self.transfer_schema_source(cx) else {
            self.notify(
                ToastVariant::Info,
                "Select a schema with tables to transfer.",
                cx,
            );
            return;
        };
        self.open_transfer(TransferEntry::Database { schema }, cx);
    }

    /// `result: transfer into…`: the focused result, filter and sort included.
    pub(crate) fn open_result_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(epoch) = self.transfer_result_epoch() else {
            self.notify(ToastVariant::Info, "No open result to transfer.", cx);
            return;
        };
        self.open_transfer(TransferEntry::Result { epoch }, cx);
    }

    /// Every namespace on every open, writable connection: the Destination step's
    /// rows. A read-only connection or an engine that can't accept inserts never
    /// appears, because planning a transfer that fails on item one is worse than
    /// not offering it (the service refuses it too; this is the visible half).
    pub(crate) fn transfer_destinations(
        &self,
        source: Option<SessionId>,
        cx: &App,
    ) -> Vec<Destination> {
        let mut out = Vec::new();
        let mut collect = |session, config: &red_core::ConnectionConfig, schema: &SchemaState| {
            if config.read_only || !config.kind.write_caps().insert {
                return;
            }
            for ns in &schema.schemas {
                out.push(Destination {
                    session,
                    conn_name: config.name.clone(),
                    kind: config.kind,
                    namespace: ns.name.clone(),
                    objects: ns.objects.iter().map(|o| o.name.clone()).collect(),
                    same_connection: Some(session) == source,
                });
            }
        };
        if let Phase::Connected(active) = &self.phase {
            collect(active.session, &active.config, active.schema.read(cx));
        }
        for (id, conn) in &self.parked {
            collect(*id, &conn.config, conn.schema.read(cx));
        }
        // Same connection first: a transfer inside one database is the common
        // case, and a long roster of other connections should not bury it.
        out.sort_by_key(|d| (!d.same_connection, d.conn_name.clone(), d.namespace.clone()));
        out
    }

    /// Choose the destination namespace, then re-resolve every item's action
    /// against what is already there.
    pub(crate) fn set_transfer_destination(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        let Some(dest) = w.destinations.get(index) else {
            return;
        };
        w.destination = Some(index);
        w.plan.target_namespace = Some(dest.namespace.clone());
        w.note = None;
        self.resolve_transfer_actions(cx);
        cx.notify();
    }

    /// Re-derive each item's action from what the chosen destination already
    /// holds: present means `Existing`, absent means `Create`.
    ///
    /// Only touches items the user has not overridden by hand, so re-picking a
    /// destination does not undo a deliberate `Recreate` or `Skip`.
    pub(crate) fn resolve_transfer_actions(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        let objects = w.target().map(|d| d.objects.clone()).unwrap_or_default();
        for item in &mut w.plan.items {
            if matches!(item.action, ItemAction::Skip | ItemAction::Recreate) {
                continue;
            }
            item.action = red_core::transfer::default_action(&item.target_name, &objects);
        }
        w.revalidate();
        cx.notify();
    }

    /// Objects step: the three-way `Data | Structure | Skip` control on one row.
    pub(crate) fn set_transfer_row(&mut self, index: usize, choice: usize, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        let objects = w.target().map(|d| d.objects.clone()).unwrap_or_default();
        if let Some(item) = w.plan.items.get_mut(index) {
            apply_row_choice(item, choice, &objects);
        }
        w.revalidate();
        cx.notify();
    }

    /// Objects step: the bulk buttons ("All data", "All structure only",
    /// "Select all", "Select none", "Invert"). Scoped to the rows the filter box
    /// is showing, so "None" means what is on screen rather than what is not.
    pub(crate) fn transfer_bulk(&mut self, bulk: TransferBulk, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        let objects = w.target().map(|d| d.objects.clone()).unwrap_or_default();
        let filter = w.filter.read(cx).content().to_string();
        let visible = w.visible_items(&filter);
        apply_bulk(&mut w.plan.items, &visible, bulk, &objects);
        w.revalidate();
        cx.notify();
    }

    /// Content step: focus one item's pane and re-seed the editors from it.
    pub(crate) fn focus_transfer_item(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        if let Some(w) = self.transfer.as_mut() {
            w.focused = index;
            w.disclosure = false;
        }
        self.seed_transfer_editors(cx);
        cx.notify();
    }

    /// Push the focused item's values into the shared editors. One set of
    /// editors serves every item, so they are re-seeded on each focus change
    /// rather than one editor being kept alive per table.
    fn seed_transfer_editors(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        let Some(item) = w.plan.items.get(w.focused) else {
            return;
        };
        let name = item.target_name.clone();
        let (where_text, limit_text) = match &item.content {
            ItemContent::Where(expr) => (expr.clone(), String::new()),
            ItemContent::Limit(n) => (String::new(), n.to_string()),
            _ => (String::new(), String::new()),
        };
        let (rename, where_expr, limit) = (w.rename.clone(), w.where_expr.clone(), w.limit.clone());
        rename.update(cx, |input, cx| input.set_content(name, cx));
        where_expr.update(cx, |input, cx| input.set_content(where_text, cx));
        limit.update(cx, |input, cx| input.set_content(limit_text, cx));
    }

    /// Content step: pull the editors back into the focused item. Called on every
    /// change, so the plan is always the state of record and the Review step can
    /// stay a pure render of it.
    pub(crate) fn sync_transfer_editors(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        let name = w.rename.read(cx).content().trim().to_string();
        let where_text = w.where_expr.read(cx).content().to_string();
        let limit_text = w.limit.read(cx).content().trim().to_string();
        let focused = w.focused;
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        if let Some(item) = w.plan.items.get_mut(focused) {
            if !name.is_empty() {
                item.target_name = name;
            }
            match &mut item.content {
                ItemContent::Where(expr) => *expr = where_text,
                ItemContent::Limit(n) => *n = limit_text.parse().unwrap_or(0),
                _ => {}
            }
        }
        w.revalidate();
        cx.notify();
    }

    /// Content step: the focused item's action.
    pub(crate) fn set_transfer_action(&mut self, action: ItemAction, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        if let Some(w) = self.transfer.as_mut() {
            let focused = w.focused;
            if let Some(item) = w.plan.items.get_mut(focused) {
                item.action = action;
            }
            w.revalidate();
        }
        cx.notify();
    }

    /// Content step: the focused item's row shaping.
    pub(crate) fn set_transfer_content(&mut self, content: ItemContent, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        if let Some(w) = self.transfer.as_mut() {
            let focused = w.focused;
            if let Some(item) = w.plan.items.get_mut(focused) {
                item.content = content;
            }
            w.revalidate();
        }
        self.seed_transfer_editors(cx);
        cx.notify();
    }

    /// Content step: apply the focused item's row shaping to every selected
    /// item, so a filter can be written once instead of per table.
    pub(crate) fn apply_transfer_content_to_all(&mut self, cx: &mut Context<Self>) {
        if let Some(w) = self.transfer.as_mut() {
            let Some(content) = w.plan.items.get(w.focused).map(|i| i.content.clone()) else {
                return;
            };
            for item in w.plan.items.iter_mut().filter(|i| i.is_active()) {
                item.content = content.clone();
            }
            w.revalidate();
        }
        cx.notify();
    }

    /// Toggle the "Columns and DDL" disclosure.
    pub(crate) fn toggle_transfer_disclosure(&mut self, cx: &mut Context<Self>) {
        if let Some(w) = self.transfer.as_mut() {
            w.disclosure = !w.disclosure;
        }
        cx.notify();
    }

    /// Review step: a job-wide option.
    pub(crate) fn set_transfer_option(&mut self, option: TransferOption, cx: &mut Context<Self>) {
        if let Some(w) = self.transfer.as_mut() {
            match option {
                TransferOption::PrimaryKeys(on) => w.plan.options.primary_keys = on,
                TransferOption::Indexes(on) => w.plan.options.indexes = on,
                TransferOption::ForeignKeys(on) => w.plan.options.foreign_keys = on,
                TransferOption::OnError(mode) => w.plan.options.on_error = mode,
            }
            w.dry_run = None;
        }
        cx.notify();
    }

    /// Move to a step by rail index. Navigation, never a gate: any step already
    /// in the rail is reachable, so a user who wants the default presses Enter.
    pub(crate) fn goto_transfer_step(&mut self, index: usize, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        if let Some(w) = self.transfer.as_mut()
            && index < w.steps.len()
            // The Progress step is where a run lives; it is not somewhere to
            // wander before there is one.
            && (w.steps[index] != Step::Progress || w.run.is_some())
        {
            w.current = index;
        }
        self.seed_transfer_editors(cx);
        cx.notify();
    }

    /// The wizard's Enter / Next.
    pub(crate) fn transfer_next(&mut self, cx: &mut Context<Self>) {
        // The destructive confirm layers over the wizard and shares its focus
        // handle, so an Enter meant for the confirm would otherwise also advance
        // the step underneath it.
        if self.confirm_exec.is_some() {
            return;
        }
        self.sync_transfer_editors(cx);
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        // On the last non-progress step, Enter runs it. Anywhere else it advances.
        let last = w.current + 1 >= w.steps.len().saturating_sub(1);
        if last {
            self.start_transfer(cx);
        } else {
            self.goto_transfer_step(w.current + 1, cx);
        }
    }

    /// The wizard's Back.
    pub(crate) fn transfer_back(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        self.goto_transfer_step(w.current.saturating_sub(1), cx);
    }

    /// Close the wizard. A run in flight keeps going and is followed by a toast,
    /// so dismissing the modal never makes an in-progress write invisible.
    pub(crate) fn close_transfer(&mut self, cx: &mut Context<Self>) {
        // Esc belongs to the confirm while one is up; closing the wizard out
        // from under it would answer a question nobody asked.
        if self.confirm_exec.is_some() {
            return;
        }
        let Some(w) = self.transfer.take() else {
            return;
        };
        if let Some(run) = w.run.filter(|r| r.outcome.is_none()) {
            self.raise_transfer_toast(run.id, run.rows, cx);
        }
        self.refocus_root = true;
        cx.notify();
    }

    /// `Duplicate database…`: create the namespace, then point the plan at it.
    /// The wizard stays open; `NamespaceCreated` selects the new destination.
    pub(crate) fn create_transfer_namespace(&mut self, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        let Some(input) = w.new_namespace.as_ref() else {
            return;
        };
        let name = input.read(cx).content().trim().to_string();
        if name.is_empty() {
            if let Some(w) = self.transfer.as_mut() {
                w.note = Some("Enter a name for the new database.".into());
            }
            cx.notify();
            return;
        }
        let source = w.source;
        let id = OpId::new(self.next_export_id);
        self.next_export_id += 1;
        self.pending_namespace = Some((id, name.clone()));
        self.service
            .send_to(source, Command::CreateNamespace { id, name });
        if let Some(w) = self.transfer.as_mut() {
            w.note = Some("Creating the database…".into());
        }
        cx.notify();
    }

    /// Run the plan. Destructive items are counted and confirmed once for the
    /// whole job, naming what dies rather than asking per table.
    pub(crate) fn start_transfer(&mut self, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        if !w.runnable() {
            return;
        }
        let destructive = w.plan.items.iter().filter(|i| i.is_destructive()).count();
        if destructive > 0 && !self.transfer_confirmed {
            self.confirm_transfer(destructive, cx);
            return;
        }
        self.transfer_confirmed = false;
        self.send_transfer(false, cx);
    }

    /// Review step: plan and render without writing anything.
    pub(crate) fn dry_run_transfer(&mut self, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        if self.transfer.as_ref().is_some_and(|w| w.runnable()) {
            self.send_transfer(true, cx);
        }
    }

    /// Build the command and fire it at the source session.
    fn send_transfer(&mut self, dry_run: bool, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        let Some(target_session) = w.target().map(|d| d.session) else {
            return;
        };
        let mut plan = w.plan.clone();
        plan.options.dry_run = dry_run;
        let source = w.source;
        let items = plan.items.len();
        let id = OpId::new(self.next_export_id);
        self.next_export_id += 1;
        if !dry_run {
            // Move to the Progress step and stand up its per-item list, so the
            // first `TransferItemDone` has somewhere to land.
            if let Some(pos) = w.steps.iter().position(|s| *s == Step::Progress) {
                w.current = pos;
            }
            w.run = Some(TransferRun {
                id,
                plan_names: w.plan.items.iter().map(|i| i.target_name.clone()).collect(),
                reports: vec![None; items],
                current: None,
                rows: 0,
                outcome: None,
            });
        }
        self.service.send_to(
            source,
            Command::RunTransfer {
                id,
                plan: Box::new(plan),
                target_session,
            },
        );
        cx.notify();
    }

    /// Save the plan under the name in the Review step's box, so a transfer
    /// worth doing twice can be named and re-run (and handed to
    /// `red transfer --plan`, which reads the same file).
    pub(crate) fn save_transfer_plan(&mut self, cx: &mut Context<Self>) {
        self.sync_transfer_editors(cx);
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        let name = w.plan_name.read(cx).content().trim().to_string();
        if name.is_empty() {
            if let Some(w) = self.transfer.as_mut() {
                w.note = Some("Enter a name to save this plan under.".into());
            }
            cx.notify();
            return;
        }
        let note = match red_config::plans::save(&name, &w.plan) {
            Ok(path) => format!("Saved to {}", path.display()),
            Err(e) => format!("Couldn't save the plan: {e}"),
        };
        if let Some(w) = self.transfer.as_mut() {
            w.note = Some(note);
        }
        cx.notify();
    }

    /// Open the wizard on a saved plan. The destination is re-resolved against
    /// what is open *now*: a plan records a namespace, not a live session, so a
    /// namespace that isn't currently open leaves the Destination step to answer.
    pub(crate) fn open_saved_plan(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(saved) = self.saved_plans.get(index).cloned() else {
            return;
        };
        self.open_transfer(TransferEntry::Plan(Box::new(saved.plan)), cx);
        let Some(w) = self.transfer.as_mut() else {
            return;
        };
        // A plan whose target is no longer open must not silently retarget: drop
        // the namespace with the destination so the wizard asks again.
        if w.destination.is_none() {
            w.plan.target_namespace = None;
            w.current = 0;
            w.note = Some("This plan's target database isn't open; pick one.".into());
            w.revalidate();
        }
        cx.notify();
    }

    /// Cancel the running transfer. Finished items stay committed.
    pub(crate) fn cancel_transfer(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .transfer
            .as_ref()
            .and_then(|w| w.run.as_ref())
            .filter(|r| r.outcome.is_none())
            .map(|r| r.id)
        else {
            return;
        };
        let source = self.transfer.as_ref().map(|w| w.source);
        if let Some(source) = source {
            self.service.send_to(source, Command::CancelTransfer { id });
        }
        cx.notify();
    }

    // --- event handlers ---

    pub(crate) fn on_transfer_progress(
        &mut self,
        id: OpId,
        table: String,
        item_rows: u64,
        rows: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.transfer_run_mut(id) {
            run.current = Some((table.clone(), item_rows));
            run.rows = rows;
        }
        self.update_transfer_toast(id, rows, cx);
        cx.notify();
    }

    pub(crate) fn on_transfer_item_done(
        &mut self,
        id: OpId,
        item: usize,
        report: ItemReport,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.transfer_run_mut(id)
            && let Some(slot) = run.reports.get_mut(item)
        {
            *slot = Some(report);
            run.current = None;
        }
        cx.notify();
    }

    pub(crate) fn on_transfer_finished(
        &mut self,
        id: OpId,
        summary: TransferSummary,
        cx: &mut Context<Self>,
    ) {
        let rows = summary.rows;
        let failures = summary.failures();
        self.merge_transfer_summary(id, &summary);
        if let Some(run) = self.transfer_run_mut(id) {
            run.rows = rows;
            run.current = None;
            run.outcome = Some(RunOutcome::Finished(summary));
        }
        self.finish_transfer_toast(id, cx);
        let message = if failures > 0 {
            crate::i18n::tr!(
                "transfer.finished_with_failures",
                "Transfer finished: {rows} row(s), {failures} item(s) failed",
                rows = rows,
                failures = failures,
            )
        } else {
            crate::i18n::tr!(
                "transfer.finished",
                "Transferred {rows} row(s)",
                rows = rows
            )
        };
        let variant = if failures > 0 {
            ToastVariant::Warning
        } else {
            ToastVariant::Success
        };
        // Only toast when the wizard is not on screen to say it itself.
        if self.transfer.is_none() {
            self.notify(variant, message, cx);
        }
        self.refresh_schema(cx);
        cx.notify();
    }

    pub(crate) fn on_transfer_failed(
        &mut self,
        id: OpId,
        message: String,
        summary: TransferSummary,
        cx: &mut Context<Self>,
    ) {
        // A `CreateNamespace` refusal shares this event: it is a transfer that
        // never got as far as an item.
        if self
            .pending_namespace
            .as_ref()
            .is_some_and(|(p, _)| *p == id)
        {
            self.pending_namespace = None;
            if let Some(w) = self.transfer.as_mut() {
                w.note = Some(message.clone());
                cx.notify();
                return;
            }
        }
        self.merge_transfer_summary(id, &summary);
        if let Some(run) = self.transfer_run_mut(id) {
            run.rows = summary.rows;
            run.current = None;
            run.outcome = Some(RunOutcome::Failed {
                message: message.clone(),
            });
        }
        self.finish_transfer_toast(id, cx);
        if self.transfer.is_none() {
            self.notify(
                ToastVariant::Error,
                crate::i18n::tr!(
                    "transfer.failed",
                    "Transfer failed: {message}",
                    message = message
                ),
                cx,
            );
        }
        self.refresh_schema(cx);
        cx.notify();
    }

    pub(crate) fn on_transfer_cancelled(&mut self, id: OpId, rows: u64, cx: &mut Context<Self>) {
        if let Some(run) = self.transfer_run_mut(id) {
            run.rows = rows;
            run.current = None;
            run.outcome = Some(RunOutcome::Cancelled);
        }
        self.finish_transfer_toast(id, cx);
        if self.transfer.is_none() {
            self.notify(
                ToastVariant::Info,
                crate::i18n::tr!(
                    "transfer.cancelled",
                    "Transfer cancelled ({rows} row(s) kept)",
                    rows = rows
                ),
                cx,
            );
        }
        self.refresh_schema(cx);
        cx.notify();
    }

    pub(crate) fn on_transfer_planned(
        &mut self,
        _id: OpId,
        script: String,
        estimates: Vec<(String, Option<u64>)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(w) = self.transfer.as_mut() {
            w.dry_run = Some(DryRun {
                script: script.clone(),
                estimates,
            });
            w.note = None;
        }
        let _ = script;
        cx.notify();
    }

    /// Put the dry run's script in a query tab, on request. Deliberately *not*
    /// automatic: a dry run is something you read in place, and silently
    /// swapping the modal for a new tab reads as "it ran".
    pub(crate) fn open_transfer_script(&mut self, cx: &mut Context<Self>) {
        let Some(script) = self
            .transfer
            .as_ref()
            .and_then(|w| w.dry_run.as_ref())
            .map(|dry| dry.script.clone())
        else {
            return;
        };
        // Text, not execution: nothing here runs it.
        self.new_query(cx);
        if let Phase::Connected(active) = &self.phase
            && let Some(tab) = active.active()
        {
            let editor = tab.editor.clone();
            editor.update(cx, |editor, cx| editor.set_content(script, cx));
        }
        self.close_transfer(cx);
        self.notify(
            ToastVariant::Info,
            "Transfer script opened in a query tab. Nothing has run.",
            cx,
        );
        cx.notify();
    }

    pub(crate) fn on_namespace_created(&mut self, id: OpId, name: String, cx: &mut Context<Self>) {
        if self
            .pending_namespace
            .as_ref()
            .is_none_or(|(p, _)| *p != id)
        {
            return;
        }
        self.pending_namespace = None;
        // The tree does not know about it yet, so add it to the destination list
        // directly rather than waiting for a refresh round trip.
        if let Some(w) = self.transfer.as_mut() {
            let session = w.source;
            let (conn_name, kind) = match &self.phase {
                Phase::Connected(active) => (active.config.name.clone(), active.config.kind),
                _ => (w.source_label.clone(), red_core::DbKind::Postgres),
            };
            w.destinations.push(Destination {
                session,
                conn_name,
                kind,
                namespace: name.clone(),
                objects: Vec::new(),
                same_connection: true,
            });
            w.destination = Some(w.destinations.len() - 1);
            w.plan.target_namespace = Some(name.clone());
            w.new_namespace = None;
            w.note = Some(format!("Created {name}."));
        }
        self.resolve_transfer_actions(cx);
        self.refresh_schema(cx);
        cx.notify();
    }

    /// Fold a terminal summary into the per-item list.
    ///
    /// The summary is in **execution** order (FK parents first) while the list is
    /// in plan order, so they are matched by target name, not by index. It also
    /// carries what `TransferItemDone` could not: the deferred index and
    /// foreign-key passes run after every item was reported, so their warnings
    /// only exist here.
    fn merge_transfer_summary(&mut self, id: OpId, summary: &TransferSummary) {
        let Some(run) = self.transfer_run_mut(id) else {
            return;
        };
        for report in &summary.items {
            // Two active items can share a target name only if the plan is
            // invalid, which `validate` refuses before the run, so the first
            // plan item with that name is the right slot.
            let Some(index) = run.plan_names.iter().position(|name| *name == report.table) else {
                continue;
            };
            if let Some(slot) = run.reports.get_mut(index) {
                *slot = Some(report.clone());
            }
        }
    }

    /// The run behind `id`, if this wizard owns it.
    fn transfer_run_mut(&mut self, id: OpId) -> Option<&mut TransferRun> {
        self.transfer
            .as_mut()
            .and_then(|w| w.run.as_mut())
            .filter(|r| r.id == id)
    }

    /// Raise the destructive confirm once for the whole plan, naming the count
    /// rather than asking per table.
    fn confirm_transfer(&mut self, destructive: usize, cx: &mut Context<Self>) {
        let Some(w) = self.transfer.as_ref() else {
            return;
        };
        let target = w
            .target()
            .map(|d| format!("{} · {}", d.conn_name, d.namespace))
            .unwrap_or_default();
        let prose = format!(
            "{destructive} of the {} table(s) in this transfer will be cleared or dropped \
             on {target} before the new rows land. That cannot be undone.",
            w.plan.items.iter().filter(|i| i.is_active()).count(),
        );
        let preview = w
            .plan
            .items
            .iter()
            .filter(|i| i.is_destructive())
            .map(|i| match i.action {
                ItemAction::Recreate => format!("DROP TABLE {} -- then recreate", i.target_name),
                _ => format!("DELETE FROM {} -- then insert", i.target_name),
            })
            .collect::<Vec<_>>()
            .join(";\n");
        self.confirm_exec =
            self.pending_confirm(crate::app::PendingWrite::Transfer { prose, preview });
        cx.notify();
    }

    /// The confirm was accepted: run the plan without asking again.
    pub(crate) fn confirmed_transfer(&mut self, cx: &mut Context<Self>) {
        self.transfer_confirmed = true;
        self.start_transfer(cx);
    }

    /// Stand up the background toast for a run whose modal was dismissed, so a
    /// job in flight is never invisible.
    fn raise_transfer_toast(&mut self, id: OpId, rows: u64, cx: &mut Context<Self>) {
        if self.transfer_toast_id(id).is_some() {
            return;
        }
        self.push_notification(
            crate::app::Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: crate::i18n::tr!("transfer.running", "Transferring…"),
                detail: None,
                detail_label: None,
                auto_dismiss: None,
                export: Some(crate::app::ExportProgress {
                    id,
                    rows: rows as usize,
                    // A plan is not row-counted before it runs, so the toast
                    // shows a running count rather than a false percentage.
                    total: 0,
                    kind: crate::app::TransferKind::Migrate,
                }),
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: None,
            },
            cx,
        );
    }

    /// The notification carrying transfer `id`, if one is up.
    fn transfer_toast_id(&self, id: OpId) -> Option<u64> {
        self.notifications
            .iter()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == id))
            .map(|n| n.id)
    }

    /// Advance the background toast, when there is one.
    fn update_transfer_toast(&mut self, id: OpId, rows: u64, cx: &mut Context<Self>) {
        if let Some(n) = self
            .notifications
            .iter_mut()
            .find(|n| n.export.as_ref().is_some_and(|e| e.id == id))
            && let Some(export) = &mut n.export
        {
            export.rows = rows as usize;
            n.message = crate::i18n::tr!(
                "transfer.running_rows",
                "Transferring… {rows} row(s)",
                rows = rows
            );
        }
        cx.notify();
    }

    /// Drop the background toast on a terminal event.
    fn finish_transfer_toast(&mut self, id: OpId, cx: &mut Context<Self>) {
        if let Some(nid) = self.transfer_toast_id(id) {
            self.dismiss(nid, cx);
        }
    }
}

/// The Objects step's bulk actions.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TransferBulk {
    AllData,
    AllStructure,
    SelectAll,
    SelectNone,
    Invert,
}

/// One job-wide option toggle from the Review step.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TransferOption {
    PrimaryKeys(bool),
    Indexes(bool),
    ForeignKeys(bool),
    OnError(OnError),
}

/// A plain table-to-table item.
fn table_item(schema: &str, source: &str, target: &str) -> TransferItem {
    TransferItem {
        source: ItemSource::Table {
            schema: Some(schema.to_string()),
            name: source.to_string(),
        },
        target_name: target.to_string(),
        action: ItemAction::Create,
        content: ItemContent::AllRows,
        mapping: Vec::new(),
    }
}

/// The **table** names in one namespace of the schema tree.
///
/// Spelled `ObjectKind::Table` deliberately, not `is_relation()`: a view is a
/// relation and is not an INSERT target, and neither is any kind added later.
fn tables_in(state: &SchemaState, schema: &str) -> Vec<String> {
    state
        .schemas
        .iter()
        .find(|ns| ns.name == schema)
        .map(|ns| {
            ns.objects
                .iter()
                .filter(|o| matches!(o.kind, red_core::ObjectKind::Table))
                .map(|o| o.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The one-line outcome label a report row shows.
pub(crate) fn outcome_label(outcome: &ItemOutcome) -> String {
    match outcome {
        ItemOutcome::Created => "created".into(),
        ItemOutcome::CreatedEmpty => "created empty".into(),
        ItemOutcome::Recreated => "recreated".into(),
        ItemOutcome::Appended => "appended".into(),
        ItemOutcome::Replaced => "replaced".into(),
        ItemOutcome::Skipped { reason } => format!("skipped ({reason})"),
        ItemOutcome::Failed { message } => format!("failed: {message}"),
    }
}

/// Which segment of the Objects row's `Data | Structure | Skip` control is on.
pub(crate) fn row_choice(item: &TransferItem) -> usize {
    if !item.is_active() {
        2
    } else if item.content.moves_rows() {
        0
    } else {
        1
    }
}

/// The resolved-action label the Objects row shows on the right: what will
/// happen to the target, not a choice made on that row.
pub(crate) fn action_label(action: ItemAction) -> (&'static str, bool) {
    match action {
        ItemAction::Skip => ("Skip", false),
        ItemAction::Create => ("Create", false),
        // Writing into a table that already has rows deserves the glyph.
        ItemAction::Existing {
            mode: CopyMode::Append,
        } => ("Existing", true),
        ItemAction::Existing {
            mode: CopyMode::TruncateInsert,
        } => ("Replace", true),
        ItemAction::Recreate => ("Recreate", true),
    }
}

/// Apply one Objects row's three-way choice to its item.
///
/// Choosing Data or Structure on a skipped row re-selects it, resolving its
/// action against the target the way the Objects step's right-hand column shows;
/// an action the user set by hand (`Recreate`, or a truncating `Existing`) is
/// left alone, because the row control is about *content*, not about undoing a
/// deliberate choice.
fn apply_row_choice(item: &mut TransferItem, choice: usize, target_objects: &[String]) {
    let reselect = |item: &mut TransferItem| {
        if matches!(item.action, ItemAction::Skip) {
            item.action = red_core::transfer::default_action(&item.target_name, target_objects);
        }
    };
    match choice {
        0 => {
            item.content = ItemContent::AllRows;
            reselect(item);
        }
        1 => {
            item.content = ItemContent::StructureOnly;
            reselect(item);
        }
        _ => item.action = ItemAction::Skip,
    }
}

/// Apply a bulk action to the `visible` items (indices into `items`).
fn apply_bulk(
    items: &mut [TransferItem],
    visible: &[usize],
    bulk: TransferBulk,
    target_objects: &[String],
) {
    for &index in visible {
        let Some(item) = items.get_mut(index) else {
            continue;
        };
        let default = red_core::transfer::default_action(&item.target_name, target_objects);
        match bulk {
            TransferBulk::AllData => {
                item.action = default;
                item.content = ItemContent::AllRows;
            }
            TransferBulk::AllStructure => {
                item.action = default;
                item.content = ItemContent::StructureOnly;
            }
            TransferBulk::SelectAll => item.action = default,
            TransferBulk::SelectNone => item.action = ItemAction::Skip,
            TransferBulk::Invert => {
                item.action = if item.is_active() {
                    ItemAction::Skip
                } else {
                    default
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<TransferItem> {
        vec![
            TransferItem::table("users"),
            TransferItem::table("orders"),
            TransferItem::table("audit_log"),
        ]
    }

    #[test]
    fn the_row_control_reselects_a_skipped_row() {
        let mut item = TransferItem::table("users");
        item.action = ItemAction::Skip;
        apply_row_choice(&mut item, 0, &[]);
        assert_eq!(item.action, ItemAction::Create);
        assert_eq!(item.content, ItemContent::AllRows);
    }

    #[test]
    fn the_row_control_resolves_against_the_target() {
        // Present on the target means the row reads `Existing`, which is what the
        // old migrate job decided silently by skipping the table.
        let mut item = TransferItem::table("users");
        item.action = ItemAction::Skip;
        apply_row_choice(&mut item, 1, &["Users".to_string()]);
        assert_eq!(
            item.action,
            ItemAction::Existing {
                mode: red_core::CopyMode::Append
            }
        );
        assert_eq!(item.content, ItemContent::StructureOnly);
    }

    #[test]
    fn the_row_control_leaves_a_deliberate_action_alone() {
        // Switching a Recreate row to structure-only must not quietly demote it
        // to a plain Create: the drop was the user's decision.
        let mut item = TransferItem::table("users");
        item.action = ItemAction::Recreate;
        apply_row_choice(&mut item, 1, &[]);
        assert_eq!(item.action, ItemAction::Recreate);
        assert_eq!(item.content, ItemContent::StructureOnly);
    }

    #[test]
    fn bulk_only_touches_the_rows_the_filter_shows() {
        let mut items = items();
        apply_bulk(&mut items, &[0, 1], TransferBulk::SelectNone, &[]);
        assert!(!items[0].is_active());
        assert!(!items[1].is_active());
        assert!(items[2].is_active(), "a filtered-out row is untouched");
    }

    #[test]
    fn bulk_structure_keeps_the_selection_and_empties_the_rows() {
        let mut items = items();
        apply_bulk(&mut items, &[0, 1, 2], TransferBulk::AllStructure, &[]);
        assert!(items.iter().all(|i| i.is_active()));
        assert!(items.iter().all(|i| !i.content.moves_rows()));
    }

    #[test]
    fn invert_flips_each_row_independently() {
        let mut items = items();
        items[1].action = ItemAction::Skip;
        apply_bulk(&mut items, &[0, 1, 2], TransferBulk::Invert, &[]);
        assert!(!items[0].is_active());
        assert!(items[1].is_active());
        assert!(!items[2].is_active());
    }

    #[test]
    fn the_objects_row_shows_the_right_segment() {
        let mut item = TransferItem::table("users");
        assert_eq!(row_choice(&item), 0);
        item.content = ItemContent::StructureOnly;
        assert_eq!(row_choice(&item), 1);
        item.action = ItemAction::Skip;
        assert_eq!(row_choice(&item), 2);
    }

    #[test]
    fn writing_into_something_that_exists_carries_the_glyph() {
        // The warning is the whole point of the resolved-action column: an
        // `Existing` row writes into a table that already has rows in it.
        assert_eq!(action_label(ItemAction::Create), ("Create", false));
        assert_eq!(
            action_label(ItemAction::Existing {
                mode: red_core::CopyMode::Append
            }),
            ("Existing", true)
        );
        assert_eq!(action_label(ItemAction::Recreate), ("Recreate", true));
    }
}
