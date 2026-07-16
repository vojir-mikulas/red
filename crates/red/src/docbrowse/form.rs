//! The field-by-field ("Form") document editor for the Mongo inspector, the
//! Compass/Studio 3T-style alternative to the raw extended-JSON surface. A
//! [`DocForm`] is a live tree of [`FormField`]s built from a [`Document`] (edit)
//! or a blank/clone template (insert): each scalar owns its own `TextInput`, each
//! object/array nests collapsibly, and every field carries a type picker. On save
//! the whole tree serializes back to pretty extended JSON — the *same* string the
//! raw editor produces — so it reuses the existing `DocInsert`/`DocReplace`
//! pipeline unchanged. Form is the primary surface; toggling to Raw serializes the
//! form into the editor (a one-way sync, since the workspace `serde_json` is not
//! order-preserving and a reverse parse would scramble field order).

use flint::prelude::*;
use flint::{ComboBox, ComboBoxEvent, Theme};
use gpui::{SharedString, WeakEntity, div, prelude::*, px};
use red_core::doc::{DocType, DocValue, Document};

use crate::app::AppState;

use super::*;

/// Which editing surface the inspector shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectorMode {
    /// The field-by-field form (default).
    Form,
    /// The raw extended-JSON editor.
    Raw,
}

/// One editable field in a [`DocForm`]: a key, a BSON type, and a value. Nested
/// objects/arrays hold their children inline so the tree is a direct mirror of the
/// document being edited.
pub(crate) struct FormField {
    /// The field-name editor. Ignored (and rendered as a static index) when the
    /// field is an element of an array.
    key: Entity<TextInput>,
    /// True when this field is an array element (its "key" is the position).
    array_item: bool,
    /// The field's current BSON type; drives the value editor and serialization.
    ty: DocType,
    value: FormValue,
    /// The searchable type picker (a Flint `ComboBox`). Owned per field so it can
    /// filter the type list; its `Select` event routes back here by entity identity.
    /// Present even for [`FormValue::Opaque`] fields (which render a static badge
    /// instead), so every field is uniform.
    type_combo: Entity<ComboBox>,
    /// Whether a nested object/array is collapsed in the tree.
    collapsed: bool,
}

/// A field's value, shaped by its [`DocType`].
pub(crate) enum FormValue {
    /// A text-edited scalar (string / number / decimal / objectId / date).
    Scalar(Entity<TextInput>),
    /// A boolean, edited via a two-way toggle.
    Bool(bool),
    /// An explicit null (no editor).
    Null,
    /// A nested object or array; the [`FormField::ty`] distinguishes them.
    Nested(Vec<FormField>),
    /// A type the form can't edit inline (binary / regex / timestamp). Preserved
    /// verbatim and shown read-only; edit it in the Raw surface.
    Opaque(DocValue),
}

/// The field-by-field editor model for one document.
pub(crate) struct DocForm {
    /// The original `_id` (edit mode); `None` when composing a new document, so
    /// the server mints a fresh one.
    id: Option<DocValue>,
    /// The top-level fields, in document order.
    fields: Vec<FormField>,
    /// Monotonic source of unique element ids for each field's type `ComboBox`, so
    /// no two combos collide even as fields are added and removed.
    combo_seq: u64,
}

/// The BSON types offered in the per-field type picker, in menu order. Exotic
/// types (binary/regex/timestamp) are intentionally absent — they round-trip as
/// [`FormValue::Opaque`] and are edited in the Raw surface.
const FORM_TYPES: [DocType; 11] = [
    DocType::Str,
    DocType::Int,
    DocType::Long,
    DocType::Double,
    DocType::Decimal,
    DocType::Bool,
    DocType::Date,
    DocType::ObjectId,
    DocType::Null,
    DocType::Object,
    DocType::Array,
];

impl DocForm {
    /// Build a form from an existing document (edit mode): `_id` is preserved and
    /// shown read-only, the rest of the fields become editable rows.
    pub(crate) fn from_document(
        doc: &Document,
        session: SessionId,
        cx: &mut Context<AppState>,
    ) -> Self {
        let mut seq = 0u64;
        let fields = doc
            .fields
            .iter()
            .map(|(k, v)| field_from_value(k, v, false, session, &mut seq, cx))
            .collect();
        Self {
            id: Some(doc.id.clone()),
            fields,
            combo_seq: seq,
        }
    }

    /// Build a form from a set of fields with no `_id` (clone / compose-from-copy):
    /// the insert mints a fresh identifier.
    pub(crate) fn from_fields(
        fields: &[(String, DocValue)],
        session: SessionId,
        cx: &mut Context<AppState>,
    ) -> Self {
        let mut seq = 0u64;
        let fields = fields
            .iter()
            .map(|(k, v)| field_from_value(k, v, false, session, &mut seq, cx))
            .collect();
        Self {
            id: None,
            fields,
            combo_seq: seq,
        }
    }

    /// A blank compose form: a single empty string field to fill in.
    pub(crate) fn blank(session: SessionId, cx: &mut Context<AppState>) -> Self {
        let mut seq = 0u64;
        let field = empty_field(session, &mut seq, cx);
        Self {
            id: None,
            fields: vec![field],
            combo_seq: seq,
        }
    }

    /// Serialize the whole form to pretty extended JSON — the body passed to
    /// `DocInsert`/`DocReplace`. Returns the first validation error (bad number,
    /// malformed objectId, …) so the caller can surface it instead of writing.
    pub(crate) fn serialize(&self, cx: &mut Context<AppState>) -> Result<String, String> {
        let mut members: Vec<String> = Vec::new();
        if let Some(id) = &self.id {
            members.push(format!(
                "  {}: {}",
                json_string("_id"),
                id.to_extended_json()
            ));
        }
        for f in &self.fields {
            members.push(member_str(f, false, 1, cx)?);
        }
        if members.is_empty() {
            return Ok("{}".to_string());
        }
        Ok(format!("{{\n{}\n}}", members.join(",\n")))
    }
}

/// Serialize one field to a `"key": value` (object) or bare `value` (array) member
/// at the given indent level.
fn member_str(
    field: &FormField,
    array_item: bool,
    indent: usize,
    cx: &mut Context<AppState>,
) -> Result<String, String> {
    let pad = "  ".repeat(indent);
    let value = value_str(field.ty, &field.value, indent, cx)?;
    if array_item {
        Ok(format!("{pad}{value}"))
    } else {
        let key = field.key.read(cx).content();
        if key.trim().is_empty() {
            return Err("A field is missing its name.".to_string());
        }
        Ok(format!("{pad}{}: {value}", json_string(&key)))
    }
}

/// Serialize a value at `indent` (the level of its own closing brace/bracket).
fn value_str(
    ty: DocType,
    value: &FormValue,
    indent: usize,
    cx: &mut Context<AppState>,
) -> Result<String, String> {
    match value {
        FormValue::Null => Ok("null".to_string()),
        FormValue::Bool(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        FormValue::Opaque(dv) => Ok(dv.to_extended_json()),
        FormValue::Scalar(input) => scalar_json(ty, &input.read(cx).content()),
        FormValue::Nested(children) => {
            let is_array = ty == DocType::Array;
            let (open, close) = if is_array { ('[', ']') } else { ('{', '}') };
            if children.is_empty() {
                return Ok(format!("{open}{close}"));
            }
            let mut members = Vec::with_capacity(children.len());
            for c in children {
                members.push(member_str(c, is_array, indent + 1, cx)?);
            }
            let close_pad = "  ".repeat(indent);
            Ok(format!(
                "{open}\n{}\n{close_pad}{close}",
                members.join(",\n")
            ))
        }
    }
}

/// Serialize a scalar's text to its extended-JSON fragment, validating by type.
fn scalar_json(ty: DocType, text: &str) -> Result<String, String> {
    let trimmed = text.trim();
    match ty {
        DocType::Str => Ok(json_string(text)),
        DocType::Int => trimmed
            .parse::<i32>()
            .map(|n| n.to_string())
            .map_err(|_| format!("\u{201c}{trimmed}\u{201d} is not a 32-bit integer.")),
        DocType::Long => trimmed
            .parse::<i64>()
            .map(|n| n.to_string())
            .map_err(|_| format!("\u{201c}{trimmed}\u{201d} is not a 64-bit integer.")),
        DocType::Double => {
            let x: f64 = trimmed
                .parse()
                .map_err(|_| format!("\u{201c}{trimmed}\u{201d} is not a number."))?;
            if x.is_finite() {
                Ok(format_double(x))
            } else {
                Err("A double can't be infinite or NaN in the form editor.".to_string())
            }
        }
        DocType::Decimal => {
            if trimmed.is_empty() || trimmed.parse::<f64>().is_err() {
                return Err(format!(
                    "\u{201c}{trimmed}\u{201d} is not a decimal number."
                ));
            }
            Ok(format!(
                "{{{}: {}}}",
                json_string("$numberDecimal"),
                json_string(trimmed)
            ))
        }
        DocType::ObjectId => {
            let valid = trimmed.len() == 24 && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
            if !valid {
                return Err(format!(
                    "An ObjectId must be 24 hex characters (got \u{201c}{trimmed}\u{201d})."
                ));
            }
            Ok(format!(
                "{{{}: {}}}",
                json_string("$oid"),
                json_string(trimmed)
            ))
        }
        DocType::Date => {
            if trimmed.is_empty() {
                return Err("A date is empty (use an ISO-8601 string).".to_string());
            }
            Ok(format!(
                "{{{}: {}}}",
                json_string("$date"),
                json_string(trimmed)
            ))
        }
        // Exotic types never reach here as scalars (they are Opaque); treat any
        // stray as a string so serialization stays total.
        _ => Ok(json_string(text)),
    }
}

/// Format a finite double so it round-trips as a JSON *double* (never an int):
/// `5` becomes `5.0`, `1.5` stays `1.5`, `1e10` stays exponential.
fn format_double(x: f64) -> String {
    let s = format!("{x}");
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// JSON-escape a string (keys and string scalars) via `serde_json` so the output
/// parses cleanly downstream.
fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

// --- building the form from a value -----------------------------------------

fn field_from_value(
    key: &str,
    value: &DocValue,
    array_item: bool,
    session: SessionId,
    next: &mut u64,
    cx: &mut Context<AppState>,
) -> FormField {
    let (ty, value) = value_from(value, session, next, cx);
    let name = if array_item {
        String::new()
    } else {
        key.to_string()
    };
    let type_combo = make_type_combo(ty, session, next, cx);
    FormField {
        key: cx.new(|cx| {
            TextInput::new(cx)
                .with_placeholder("field")
                .with_content(name)
        }),
        array_item,
        ty,
        value,
        type_combo,
        collapsed: false,
    }
}

fn value_from(
    dv: &DocValue,
    session: SessionId,
    next: &mut u64,
    cx: &mut Context<AppState>,
) -> (DocType, FormValue) {
    match dv {
        DocValue::Null => (DocType::Null, FormValue::Null),
        DocValue::Bool(b) => (DocType::Bool, FormValue::Bool(*b)),
        DocValue::Array(items) => (
            DocType::Array,
            FormValue::Nested(
                items
                    .iter()
                    .map(|v| field_from_value("", v, true, session, next, cx))
                    .collect(),
            ),
        ),
        DocValue::Document(fields) => (
            DocType::Object,
            FormValue::Nested(
                fields
                    .iter()
                    .map(|(k, v)| field_from_value(k, v, false, session, next, cx))
                    .collect(),
            ),
        ),
        DocValue::Binary { .. } | DocValue::Regex { .. } | DocValue::Timestamp(_) => {
            (dv.doc_type(), FormValue::Opaque(dv.clone()))
        }
        scalar => {
            let seed = scalar_seed(scalar);
            let input = cx.new(|cx| {
                TextInput::new(cx)
                    .with_placeholder("value")
                    .with_content(seed)
            });
            (scalar.doc_type(), FormValue::Scalar(input))
        }
    }
}

/// Build a field's searchable type picker: a `ComboBox` over [`FORM_TYPES`] with
/// the current type applied, wired so its `Select` event routes back to
/// [`AppState::doc_form_combo_selected`] by entity identity.
fn make_type_combo(
    ty: DocType,
    session: SessionId,
    next: &mut u64,
    cx: &mut Context<AppState>,
) -> Entity<ComboBox> {
    let id = *next;
    *next += 1;
    let selected = FORM_TYPES.iter().position(|t| *t == ty);
    let options: Vec<SharedString> = FORM_TYPES
        .iter()
        .map(|t| SharedString::from(t.label()))
        .collect();
    let combo = cx.new(|cx| {
        let mut c = ComboBox::new(SharedString::from(format!("form-type-{id}")), cx);
        c.set_search_placeholder("Search type…", cx);
        c.set_full_width(true, cx);
        c.set_options(options, selected, cx);
        c.set_chevron(
            |app| {
                crate::icons::icon("chevron-down", app.theme().scale(12.), app.theme().text_dim)
                    .into_any_element()
            },
            cx,
        );
        c.set_check(
            |app| {
                crate::icons::icon("check", app.theme().scale(12.), app.theme().accent)
                    .into_any_element()
            },
            cx,
        );
        c
    });
    cx.subscribe(&combo, move |this, emitter, event: &ComboBoxEvent, cx| {
        if let ComboBoxEvent::Select(label) = event {
            this.doc_form_combo_selected(session, emitter, label.clone(), cx);
        }
    })
    .detach();
    combo
}

/// The initial text a scalar's editor shows.
fn scalar_seed(dv: &DocValue) -> String {
    match dv {
        DocValue::Str(s) => s.clone(),
        DocValue::Int32(n) => n.to_string(),
        DocValue::Int64(n) => n.to_string(),
        DocValue::Double(x) => format!("{x}"),
        DocValue::Decimal128(s) => s.clone(),
        DocValue::ObjectId(bytes) => hex::encode(bytes),
        DocValue::DateTime(ms) => date_seed(*ms),
        other => other.to_extended_json(),
    }
}

/// The ISO-8601 seed for a datetime, unwrapped from its extended-JSON form; falls
/// back to the raw milliseconds when out of the ISO-representable range.
fn date_seed(ms: i64) -> String {
    let ej = DocValue::DateTime(ms).to_extended_json();
    ej.strip_prefix("{\"$date\":\"")
        .and_then(|r| r.strip_suffix("\"}"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// A fresh, empty string field for the "add field" affordance.
fn empty_field(session: SessionId, next: &mut u64, cx: &mut Context<AppState>) -> FormField {
    let type_combo = make_type_combo(DocType::Str, session, next, cx);
    FormField {
        key: cx.new(|cx| TextInput::new(cx).with_placeholder("field")),
        array_item: false,
        ty: DocType::Str,
        value: FormValue::Scalar(cx.new(|cx| TextInput::new(cx).with_placeholder("value"))),
        type_combo,
        collapsed: false,
    }
}

// --- tree navigation --------------------------------------------------------

/// The mutable child list for a container path (`[]` = the form's top level, else
/// the nested object/array reached by `path`).
fn children_at<'a>(form: &'a mut DocForm, path: &[usize]) -> Option<&'a mut Vec<FormField>> {
    if path.is_empty() {
        return Some(&mut form.fields);
    }
    match &mut field_at(&mut form.fields, path)?.value {
        FormValue::Nested(children) => Some(children),
        _ => None,
    }
}

/// The mutable field reached by `path` (indices from the top level down).
fn field_at<'a>(fields: &'a mut [FormField], path: &[usize]) -> Option<&'a mut FormField> {
    let (first, rest) = path.split_first()?;
    let field = fields.get_mut(*first)?;
    if rest.is_empty() {
        return Some(field);
    }
    match &mut field.value {
        FormValue::Nested(children) => field_at(children, rest),
        _ => None,
    }
}

/// Whether the container at `path` is an array (its children are index-keyed).
fn container_is_array(form: &mut DocForm, path: &[usize]) -> bool {
    if path.is_empty() {
        return false;
    }
    field_at(&mut form.fields, path).is_some_and(|f| f.ty == DocType::Array)
}

// --- form-editing commands (AppState) ---------------------------------------

impl AppState {
    /// Switch the inspector's editing surface. Moving to Raw serializes the form
    /// into the editor first, so the raw text reflects the form's edits.
    pub(crate) fn doc_set_inspector_mode(
        &mut self,
        session: SessionId,
        mode: InspectorMode,
        cx: &mut Context<Self>,
    ) {
        let sync = {
            let Some(current) = self.doc_focused_coll_mut(session) else {
                return;
            };
            if current.inspector_mode == mode {
                return;
            }
            current.inspector_mode = mode;
            if mode == InspectorMode::Raw {
                current
                    .form
                    .as_ref()
                    .map(|f| (current.inspector_editor.clone(), f.serialize(cx)))
            } else {
                None
            }
        };
        if let Some((editor, result)) = sync {
            match result {
                Ok(json) => editor.update(cx, |ed, cx| ed.set_content(json, cx)),
                Err(err) => {
                    self.notify(ToastVariant::Error, err, cx);
                }
            }
        }
        cx.notify();
    }

    /// Append a blank field to the container at `path` (`[]` = top level).
    pub(crate) fn doc_form_add_field(
        &mut self,
        session: SessionId,
        path: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        // Mint a unique combo id before building the field (which borrows `cx`).
        let Some(mut next) = self
            .doc_focused_coll_mut(session)
            .and_then(|c| c.form.as_ref())
            .map(|f| f.combo_seq)
        else {
            return;
        };
        let field = empty_field(session, &mut next, cx);
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let Some(form) = current.form.as_mut() else {
            return;
        };
        form.combo_seq = next;
        let array_item = container_is_array(form, &path);
        if let Some(children) = children_at(form, &path) {
            let mut field = field;
            field.array_item = array_item;
            children.push(field);
        }
        cx.notify();
    }

    /// Remove the field reached by `path`.
    pub(crate) fn doc_form_remove_field(
        &mut self,
        session: SessionId,
        path: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let Some(form) = current.form.as_mut() else {
            return;
        };
        let Some((&index, parent)) = path.split_last() else {
            return;
        };
        if let Some(children) = children_at(form, parent)
            && index < children.len()
        {
            children.remove(index);
        }
        cx.notify();
    }

    /// Collapse/expand the nested field reached by `path`.
    pub(crate) fn doc_form_toggle_collapse(
        &mut self,
        session: SessionId,
        path: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let Some(form) = current.form.as_mut() else {
            return;
        };
        if let Some(field) = field_at(&mut form.fields, &path) {
            field.collapsed = !field.collapsed;
        }
        cx.notify();
    }

    /// Set a boolean field's value.
    pub(crate) fn doc_form_set_bool(
        &mut self,
        session: SessionId,
        path: Vec<usize>,
        value: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let Some(form) = current.form.as_mut() else {
            return;
        };
        if let Some(field) = field_at(&mut form.fields, &path)
            && let FormValue::Bool(b) = &mut field.value
        {
            *b = value;
        }
        cx.notify();
    }

    /// Apply a type change picked in a field's `ComboBox`. The field is located by
    /// the combo's entity identity (the inspector only renders the focused
    /// collection's form, so the pick can only come from it), then its value is
    /// converted: scalars keep their text where sensible, containers reset to empty,
    /// bool/null lose it.
    pub(crate) fn doc_form_combo_selected(
        &mut self,
        session: SessionId,
        combo: Entity<ComboBox>,
        label: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some(ty) = FORM_TYPES
            .iter()
            .copied()
            .find(|t| t.label() == label.as_ref())
        else {
            return;
        };
        // Phase 1: find the field's path and any carried-over scalar text.
        let Some((path, seed)) = ({
            let Some(current) = self.doc_focused_coll_mut(session) else {
                return;
            };
            let Some(form) = current.form.as_mut() else {
                return;
            };
            path_of_combo(&form.fields, &combo, &mut Vec::new()).map(|path| {
                let seed = match field_at(&mut form.fields, &path).map(|f| &f.value) {
                    Some(FormValue::Scalar(input)) => input.read(cx).content().to_string(),
                    _ => String::new(),
                };
                (path, seed)
            })
        }) else {
            return;
        };
        // Phase 2: build the scalar editor (if the new type needs one).
        let scalar = matches!(
            ty,
            DocType::Str
                | DocType::Int
                | DocType::Long
                | DocType::Double
                | DocType::Decimal
                | DocType::ObjectId
                | DocType::Date
        );
        let new_input = scalar.then(|| {
            cx.new(|cx| {
                TextInput::new(cx)
                    .with_placeholder("value")
                    .with_content(seed)
            })
        });
        // Phase 3: apply the type + converted value.
        let Some(current) = self.doc_focused_coll_mut(session) else {
            return;
        };
        let Some(form) = current.form.as_mut() else {
            return;
        };
        if let Some(field) = field_at(&mut form.fields, &path) {
            field.ty = ty;
            field.collapsed = false;
            // `new_input` is `Some` exactly for the scalar types; the rest map to
            // their container/bool/null shape.
            field.value = match new_input {
                Some(input) => FormValue::Scalar(input),
                None if ty == DocType::Bool => FormValue::Bool(false),
                None if ty == DocType::Null => FormValue::Null,
                None => FormValue::Nested(Vec::new()),
            };
        }
        cx.notify();
    }
}

/// The path to the field whose type `ComboBox` is `combo`, searched depth-first.
/// `prefix` is scratch space (pass `&mut Vec::new()`).
fn path_of_combo(
    fields: &[FormField],
    combo: &Entity<ComboBox>,
    prefix: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    for (i, field) in fields.iter().enumerate() {
        if &field.type_combo == combo {
            let mut path = prefix.clone();
            path.push(i);
            return Some(path);
        }
        if let FormValue::Nested(children) = &field.value {
            prefix.push(i);
            if let Some(path) = path_of_combo(children, combo, prefix) {
                return Some(path);
            }
            prefix.pop();
        }
    }
    None
}

// --- rendering --------------------------------------------------------------

impl AppState {
    /// Render the field-by-field editor body for the inspector.
    pub(super) fn render_doc_form(
        &self,
        session: SessionId,
        current: &CollView,
        view: &WeakEntity<AppState>,
        theme: &Theme,
    ) -> gpui::AnyElement {
        let Some(form) = current.form.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if let Some(id) = &form.id {
            rows.push(id_row(id, theme));
        }
        for (i, field) in form.fields.iter().enumerate() {
            rows.push(render_field(session, field, vec![i], i, view, theme));
        }
        rows.push(add_button(session, Vec::new(), false, view, theme));

        div()
            .id("doc-form")
            .flex_1()
            .min_h(px(0.))
            .overflow_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap(px(3.))
            .text_size(theme.scale(12.))
            .children(rows)
            .into_any_element()
    }
}

/// The read-only `_id` row shown at the top of an edit form.
fn id_row(id: &DocValue, theme: &Theme) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py(px(2.))
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                .text_color(theme.accent)
                .child("_id"),
        )
        .child(type_badge(id.type_name(), theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.text_muted)
                .font_family(theme.mono_family.clone())
                .child(id.to_cell(120).to_string()),
        )
        .into_any_element()
}

/// Render one field (and, for containers, its children and an "add" affordance).
fn render_field(
    session: SessionId,
    field: &FormField,
    path: Vec<usize>,
    index: usize,
    view: &WeakEntity<AppState>,
    theme: &Theme,
) -> gpui::AnyElement {
    let depth = path.len().saturating_sub(1);
    let indent = px(depth as f32 * 14.);
    let nested = matches!(field.value, FormValue::Nested(_));
    let key = pk(&path);

    // Leading control: a collapse chevron for containers, else a spacer.
    let lead = if nested {
        let toggle_view = view.clone();
        let toggle_path = path.clone();
        let name = if field.collapsed {
            "chevron"
        } else {
            "chevron-down"
        };
        div()
            .id(SharedString::from(format!("form-collapse-{key}")))
            .flex_shrink_0()
            .size(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .rounded(px(3.))
            .hover(|s| s.bg(theme.bg_elevated))
            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                toggle_view
                    .update(cx, |this, cx| {
                        this.doc_form_toggle_collapse(session, toggle_path.clone(), cx)
                    })
                    .ok();
            })
            .child(crate::icons::icon(name, theme.scale(12.), theme.text_muted))
            .into_any_element()
    } else {
        div().flex_shrink_0().size(px(16.)).into_any_element()
    };

    // Key: an editable input, or a static index for array elements.
    let key_el = if field.array_item {
        div()
            .w(px(150.))
            .flex_shrink_0()
            .text_color(theme.text_muted)
            .child(format!("{index}"))
            .into_any_element()
    } else {
        div()
            .w(px(150.))
            .flex_shrink_0()
            .child(field.key.clone())
            .into_any_element()
    };

    // Type control: a searchable combo box for editable types, a static badge for
    // opaque ones (binary/regex/timestamp, edited in the Raw surface).
    let type_el = if matches!(field.value, FormValue::Opaque(_)) {
        type_badge(field.ty.label(), theme).into_any_element()
    } else {
        div()
            .w(px(112.))
            .flex_shrink_0()
            .child(field.type_combo.clone())
            .into_any_element()
    };

    let value_el = render_value(session, field, &path, view, theme);

    // Trailing remove button (a small trash affordance).
    let remove_view = view.clone();
    let remove_path = path.clone();
    let remove = div()
        .id(SharedString::from(format!("form-remove-{key}")))
        .flex_shrink_0()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .cursor_pointer()
        .text_color(theme.text_muted)
        .hover(|s| s.bg(theme.bg_elevated).text_color(theme.red))
        .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
            remove_view
                .update(cx, |this, cx| {
                    this.doc_form_remove_field(session, remove_path.clone(), cx)
                })
                .ok();
        })
        .child(crate::icons::icon(
            "trash",
            theme.scale(12.),
            theme.text_muted,
        ));

    let header = div()
        .flex()
        .items_center()
        .gap_2()
        .pl(indent)
        .child(lead)
        .child(key_el)
        .child(type_el)
        .child(value_el)
        .child(remove);

    let mut container = div().flex().flex_col().gap(px(3.)).child(header);

    // Children + "add" affordance for an expanded object/array.
    if let FormValue::Nested(children) = &field.value
        && !field.collapsed
    {
        let child_array = field.ty == DocType::Array;
        for (i, child) in children.iter().enumerate() {
            let mut child_path = path.clone();
            child_path.push(i);
            container = container.child(render_field(session, child, child_path, i, view, theme));
        }
        container = container.child(add_button(session, path.clone(), child_array, view, theme));
    }

    container.into_any_element()
}

/// Render the value editor for a field.
fn render_value(
    session: SessionId,
    field: &FormField,
    path: &[usize],
    view: &WeakEntity<AppState>,
    theme: &Theme,
) -> gpui::AnyElement {
    match &field.value {
        FormValue::Scalar(input) => div()
            .flex_1()
            .min_w_0()
            .child(input.clone())
            .into_any_element(),
        FormValue::Bool(b) => {
            let set_view = view.clone();
            let set_path = path.to_vec();
            div()
                .flex_1()
                .child(
                    Segmented::new(SharedString::from(format!("form-bool-{}", pk(path))))
                        .segment("false")
                        .segment("true")
                        .selected(if *b { 1 } else { 0 })
                        .on_select(move |ix, _, cx| {
                            let val = ix == 1;
                            set_view
                                .update(cx, |this, cx| {
                                    this.doc_form_set_bool(session, set_path.clone(), val, cx)
                                })
                                .ok();
                        }),
                )
                .into_any_element()
        }
        FormValue::Null => div()
            .flex_1()
            .text_color(theme.text_muted)
            .font_family(theme.mono_family.clone())
            .child("null")
            .into_any_element(),
        FormValue::Opaque(dv) => div()
            .flex_1()
            .min_w_0()
            .truncate()
            .text_color(theme.text_muted)
            .font_family(theme.mono_family.clone())
            .child(dv.to_cell(200).to_string())
            .into_any_element(),
        FormValue::Nested(children) => {
            let label = if field.ty == DocType::Array {
                format!("[ {} ]", children.len())
            } else {
                format!("{{ {} }}", children.len())
            };
            div()
                .flex_1()
                .text_color(theme.text_muted)
                .child(label)
                .into_any_element()
        }
    }
}

/// The "add field" / "add item" affordance for a container.
fn add_button(
    session: SessionId,
    path: Vec<usize>,
    array: bool,
    view: &WeakEntity<AppState>,
    theme: &Theme,
) -> gpui::AnyElement {
    let depth = path.len();
    let indent = px((depth as f32 * 14.) + 16.);
    let label = if array { "Add item" } else { "Add field" };
    let add_view = view.clone();
    div()
        .id(SharedString::from(format!("form-add-{}", pk(&path))))
        .flex()
        .items_center()
        .gap_1()
        .pl(indent)
        .py(px(1.))
        .w_full()
        .cursor_pointer()
        .text_color(theme.text_muted)
        .hover(|s| s.text_color(theme.accent))
        .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
            add_view
                .update(cx, |this, cx| {
                    this.doc_form_add_field(session, path.clone(), cx)
                })
                .ok();
        })
        .child(crate::icons::icon(
            "plus",
            theme.scale(11.),
            theme.text_muted,
        ))
        .child(label)
        .into_any_element()
}

/// A compact static type badge (for `_id` and opaque values).
fn type_badge(label: &str, theme: &Theme) -> gpui::Div {
    div()
        .w(px(96.))
        .flex_shrink_0()
        .px(px(6.))
        .text_size(theme.scale(11.))
        .text_color(theme.text_muted)
        .child(label.to_string())
}

/// A stable string key for a field path, used for element ids.
fn pk(path: &[usize]) -> String {
    path.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_json_typed_fragments() {
        assert_eq!(
            scalar_json(DocType::Str, "hi \"x\""),
            Ok(r#""hi \"x\"""#.to_string())
        );
        assert_eq!(scalar_json(DocType::Int, " 42 "), Ok("42".to_string()));
        assert_eq!(
            scalar_json(DocType::Long, "9000000000"),
            Ok("9000000000".to_string())
        );
        assert_eq!(
            scalar_json(DocType::ObjectId, "5f1d7f9b9d3b2a1c4e8b4567"),
            Ok(r#"{"$oid": "5f1d7f9b9d3b2a1c4e8b4567"}"#.to_string())
        );
        assert_eq!(
            scalar_json(DocType::Date, "2020-01-02T03:04:05Z"),
            Ok(r#"{"$date": "2020-01-02T03:04:05Z"}"#.to_string())
        );
        assert_eq!(
            scalar_json(DocType::Decimal, "1.50"),
            Ok(r#"{"$numberDecimal": "1.50"}"#.to_string())
        );
    }

    #[test]
    fn scalar_json_rejects_malformed_values() {
        assert!(scalar_json(DocType::Int, "x").is_err());
        assert!(scalar_json(DocType::Double, "not-a-number").is_err());
        // Too short to be an ObjectId.
        assert!(scalar_json(DocType::ObjectId, "abc").is_err());
        assert!(scalar_json(DocType::Date, "  ").is_err());
    }

    #[test]
    fn doubles_round_trip_as_doubles() {
        // A whole number keeps a fractional part so it never reparses as an int.
        assert_eq!(format_double(5.0), "5.0");
        assert_eq!(format_double(2.5), "2.5");
    }
}
