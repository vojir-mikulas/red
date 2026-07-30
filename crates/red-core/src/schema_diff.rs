//! Schema compare: what differs between two schemas, and the DDL that reconciles
//! them.
//!
//! The structural sibling of [`crate::diff`], which compares *data*. Both are
//! pure: no drivers, no cursors, no UI, no runtime. This one takes the
//! introspection types RED already loads ([`SchemaMeta`] + [`crate::TableDetail`]) and
//! answers what changed; the service walks the objects and feeds it, the UI
//! renders the result.
//!
//! **Type equivalence is the hard part.** `varchar(255)` on MySQL,
//! `character varying(255)` on Postgres, and `String` on ClickHouse are one
//! column spelled three ways. Comparing the raw strings would report every column
//! of a cross-engine diff as changed, which is worse than having no feature. So
//! the comparison key is [`crate::typemap`]'s engine-neutral class, with the raw
//! spelling kept for display, and a column counts as changed only when the
//! classes differ, or when the engines match and the spellings do not. What the
//! lattice cannot classify is compared by string and flagged
//! [`Confidence::Uncertain`] rather than asserted.
//!
//! Nothing here executes. [`SchemaDelta::to_sql`] produces text, additive by
//! default, with drops commented out unless explicitly asked for; running it is
//! the user's own trip through the editor and its guards.

use crate::typemap::{self, NormType};
use crate::{ColumnMeta, DbKind, ForeignKeyMeta, IndexMeta, ObjectKind, ObjectMeta, SchemaMeta};

/// One side of a comparison: a namespace's objects plus the detail of each
/// relation in it.
#[derive(Debug, Clone, Default)]
pub struct SchemaSnapshot {
    pub engine: DbKind,
    pub namespace: String,
    pub objects: Vec<ObjectMeta>,
    /// Keyed by object name, for the relations whose detail was loaded. An object
    /// absent here is compared by existence only.
    pub details: std::collections::HashMap<String, crate::TableDetail>,
}

impl SchemaSnapshot {
    pub fn from_meta(engine: DbKind, meta: &SchemaMeta) -> Self {
        Self {
            engine,
            namespace: meta.name.clone(),
            objects: meta.objects.clone(),
            details: std::collections::HashMap::new(),
        }
    }
}

/// How sure the comparator is about a column difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The type lattice classified both sides.
    Certain,
    /// One side is a type the lattice does not model (an array, a `tsvector`, a
    /// custom enum), so this is a raw-string comparison and may be a spelling
    /// difference rather than a real one.
    Uncertain,
}

/// A column present on both sides but different.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnChange {
    pub left: ColumnMeta,
    pub right: ColumnMeta,
    pub confidence: Confidence,
    /// What differs, already phrased: "varchar(50) → text", "now NOT NULL".
    pub summary: String,
}

/// What differs about one relation present on both sides.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableDelta {
    pub name: String,
    /// On the right but not the left.
    pub columns_added: Vec<ColumnMeta>,
    /// On the left but not the right. Reconciling one means dropping data.
    pub columns_removed: Vec<ColumnMeta>,
    pub columns_changed: Vec<ColumnChange>,
    pub indexes_added: Vec<IndexMeta>,
    pub indexes_removed: Vec<IndexMeta>,
    pub fks_added: Vec<ForeignKeyMeta>,
    pub fks_removed: Vec<ForeignKeyMeta>,
}

impl TableDelta {
    pub fn is_empty(&self) -> bool {
        self.columns_added.is_empty()
            && self.columns_removed.is_empty()
            && self.columns_changed.is_empty()
            && self.indexes_added.is_empty()
            && self.indexes_removed.is_empty()
            && self.fks_added.is_empty()
            && self.fks_removed.is_empty()
    }
}

/// The whole comparison.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaDelta {
    /// On the right but not the left.
    pub objects_added: Vec<ObjectMeta>,
    /// On the left but not the right.
    pub objects_removed: Vec<ObjectMeta>,
    pub tables_changed: Vec<TableDelta>,
    /// True when the two sides are different engines, so the report can say the
    /// comparison is by type class rather than by spelling.
    pub cross_engine: bool,
}

impl SchemaDelta {
    pub fn is_empty(&self) -> bool {
        self.objects_added.is_empty()
            && self.objects_removed.is_empty()
            && self.tables_changed.is_empty()
    }

    /// Total differences, for the summary line.
    pub fn count(&self) -> usize {
        self.objects_added.len()
            + self.objects_removed.len()
            + self
                .tables_changed
                .iter()
                .map(|t| {
                    t.columns_added.len()
                        + t.columns_removed.len()
                        + t.columns_changed.len()
                        + t.indexes_added.len()
                        + t.indexes_removed.len()
                        + t.fks_added.len()
                        + t.fks_removed.len()
                })
                .sum::<usize>()
    }
}

/// Compare `left` against `right`: the delta describes what would have to change
/// on **left** to make it look like **right**.
pub fn compare(left: &SchemaSnapshot, right: &SchemaSnapshot) -> SchemaDelta {
    let mut delta = SchemaDelta {
        cross_engine: left.engine != right.engine,
        ..Default::default()
    };

    let in_left = |name: &str| left.objects.iter().any(|o| o.name == name);
    let in_right = |name: &str| right.objects.iter().any(|o| o.name == name);

    for obj in &right.objects {
        if !in_left(&obj.name) {
            delta.objects_added.push(obj.clone());
        }
    }
    for obj in &left.objects {
        if !in_right(&obj.name) {
            delta.objects_removed.push(obj.clone());
        }
    }

    // Only relations have a shape to compare; a routine present on both sides is
    // compared by existence here, because its body is `object_ddl`'s business.
    for obj in &left.objects {
        if !obj.kind.is_relation() || !in_right(&obj.name) {
            continue;
        }
        let (Some(l), Some(r)) = (left.details.get(&obj.name), right.details.get(&obj.name)) else {
            continue;
        };
        let mut td = TableDelta {
            name: obj.name.clone(),
            ..Default::default()
        };

        for rc in &r.columns {
            match l.columns.iter().find(|lc| lc.name == rc.name) {
                None => td.columns_added.push(rc.clone()),
                Some(lc) => {
                    if let Some(change) = compare_column(lc, rc, left.engine, right.engine) {
                        td.columns_changed.push(change);
                    }
                }
            }
        }
        for lc in &l.columns {
            if !r.columns.iter().any(|rc| rc.name == lc.name) {
                td.columns_removed.push(lc.clone());
            }
        }

        // Indexes and foreign keys compare by name: both sides came from the same
        // introspection path, and a rename is a legitimate difference to report.
        for ri in &r.indexes {
            if !l.indexes.iter().any(|li| li.name == ri.name) {
                td.indexes_added.push(ri.clone());
            }
        }
        for li in &l.indexes {
            if !r.indexes.iter().any(|ri| ri.name == li.name) {
                td.indexes_removed.push(li.clone());
            }
        }
        for rf in &r.foreign_keys {
            if !l.foreign_keys.iter().any(|lf| fk_eq(lf, rf)) {
                td.fks_added.push(rf.clone());
            }
        }
        for lf in &l.foreign_keys {
            if !r.foreign_keys.iter().any(|rf| fk_eq(lf, rf)) {
                td.fks_removed.push(lf.clone());
            }
        }

        if !td.is_empty() {
            delta.tables_changed.push(td);
        }
    }
    delta.tables_changed.sort_by(|a, b| a.name.cmp(&b.name));
    delta
}

/// A foreign key is the same edge when it points from the same column at the same
/// target, regardless of what the constraint happens to be called.
fn fk_eq(a: &ForeignKeyMeta, b: &ForeignKeyMeta) -> bool {
    a.column == b.column && a.ref_table == b.ref_table && a.ref_column == b.ref_column
}

/// Compare one column on both sides, or `None` when they match.
fn compare_column(
    left: &ColumnMeta,
    right: &ColumnMeta,
    left_engine: DbKind,
    right_engine: DbKind,
) -> Option<ColumnChange> {
    let mut parts: Vec<String> = Vec::new();
    let mut confidence = Confidence::Certain;

    let lt = left.type_name.as_deref().unwrap_or("");
    let rt = right.type_name.as_deref().unwrap_or("");
    let (lc, rc) = (typemap::normalize(lt), typemap::normalize(rt));
    let type_differs = match (&lc, &rc) {
        // At least one side is outside the lattice: raw comparison, flagged, so
        // the report says "this may be a spelling difference" instead of
        // asserting a change it cannot actually see.
        (NormType::Unknown(_), _) | (_, NormType::Unknown(_)) => {
            confidence = Confidence::Uncertain;
            !lt.eq_ignore_ascii_case(rt)
        }
        // Both classified and different: a real change on any engine.
        (a, b) if a != b => true,
        // Same class. Within one engine the raw spelling still carries detail the
        // lattice coarsens away, so a spelling difference there is real; across
        // engines it is noise by construction (`varchar` vs `character varying`).
        _ => left_engine == right_engine && !lt.eq_ignore_ascii_case(rt),
    };
    if type_differs {
        parts.push(format!("{lt} → {rt}"));
    }
    if left.not_null != right.not_null {
        parts.push(if right.not_null {
            "now NOT NULL".to_string()
        } else {
            "now nullable".to_string()
        });
    }
    if left.default != right.default {
        parts.push(match (&left.default, &right.default) {
            (_, Some(d)) => format!("default {d}"),
            (Some(_), None) => "default dropped".to_string(),
            (None, None) => unreachable!("equal defaults are not a difference"),
        });
    }
    if left.primary_key != right.primary_key {
        parts.push(if right.primary_key {
            "now primary key".to_string()
        } else {
            "no longer primary key".to_string()
        });
    }

    (!parts.is_empty()).then(|| ColumnChange {
        left: left.clone(),
        right: right.clone(),
        confidence,
        summary: parts.join(", "),
    })
}

/// How much of a delta to spell out as DDL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptScope {
    /// `CREATE` / `ADD` only. Destructive statements are emitted commented out.
    Additive,
    /// Everything, drops included, still as text for the user to run themselves.
    IncludeDrops,
}

impl SchemaDelta {
    /// Render the delta as DDL that would move **left** toward **right**.
    ///
    /// Text, never execution. Destructive statements are commented out under
    /// [`ScriptScope::Additive`], and type changes always carry a warning comment,
    /// because an `ALTER … TYPE` that narrows a column is a data-loss event the
    /// comparator cannot reason about.
    ///
    /// `quote` is the target dialect's identifier quoter (the driver's
    /// `quote_ident`), so the script is spelled for the engine it will run on.
    pub fn to_sql(
        &self,
        namespace: &str,
        scope: ScriptScope,
        quote: &dyn Fn(&str) -> String,
    ) -> String {
        let mut out = String::new();
        out.push_str(
            "-- Generated by RED from a schema comparison. Read it before running it:\n\
             -- RED does not execute this, and it is not a migration tool.\n",
        );
        if self.cross_engine {
            out.push_str(
                "-- The two sides are different engines, so columns were compared by type\n\
                 -- class rather than by spelling. Check the types before applying.\n",
            );
        }
        out.push('\n');

        let drop_prefix = match scope {
            ScriptScope::Additive => "-- ",
            ScriptScope::IncludeDrops => "",
        };

        for obj in &self.objects_added {
            // The full CREATE needs the object's own DDL, which lives behind
            // `object_ddl`; naming it is the honest thing this layer can do.
            out.push_str(&format!(
                "-- {} {}.{} exists only on the right; copy its definition with Show DDL.\n",
                match obj.kind {
                    ObjectKind::Table => "table",
                    ObjectKind::View => "view",
                    other => other.as_str(),
                },
                namespace,
                obj.name
            ));
        }
        for obj in &self.objects_removed {
            out.push_str(&format!(
                "{drop_prefix}DROP TABLE {}.{};\n",
                quote(namespace),
                quote(&obj.name)
            ));
        }
        if !self.objects_added.is_empty() || !self.objects_removed.is_empty() {
            out.push('\n');
        }

        for t in &self.tables_changed {
            let table = format!("{}.{}", quote(namespace), quote(&t.name));
            for col in &t.columns_added {
                let ty = col.type_name.as_deref().unwrap_or("text");
                let null = if col.not_null { " NOT NULL" } else { "" };
                let default = col
                    .default
                    .as_ref()
                    .map(|d| format!(" DEFAULT {d}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "ALTER TABLE {table} ADD COLUMN {} {ty}{default}{null};\n",
                    quote(&col.name)
                ));
            }
            for col in &t.columns_removed {
                out.push_str(&format!(
                    "{drop_prefix}ALTER TABLE {table} DROP COLUMN {};\n",
                    quote(&col.name)
                ));
            }
            for change in &t.columns_changed {
                out.push_str(&format!(
                    "-- {}: {} ({})\n",
                    change.left.name,
                    change.summary,
                    match change.confidence {
                        Confidence::Certain => "check for data loss before running",
                        Confidence::Uncertain =>
                            "types outside RED's lattice; this may be a spelling difference",
                    }
                ));
                if let Some(ty) = &change.right.type_name {
                    out.push_str(&format!(
                        "-- ALTER TABLE {table} ALTER COLUMN {} TYPE {ty};\n",
                        quote(&change.left.name)
                    ));
                }
            }
            for idx in &t.indexes_added {
                let cols = idx
                    .columns
                    .iter()
                    .map(|c| quote(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let unique = if idx.unique { "UNIQUE " } else { "" };
                out.push_str(&format!(
                    "CREATE {unique}INDEX {} ON {table} ({cols});\n",
                    quote(&idx.name)
                ));
            }
            for idx in &t.indexes_removed {
                out.push_str(&format!("{drop_prefix}DROP INDEX {};\n", quote(&idx.name)));
            }
            for fk in &t.fks_added {
                out.push_str(&format!(
                    "ALTER TABLE {table} ADD FOREIGN KEY ({}) REFERENCES {} ({});\n",
                    quote(&fk.column),
                    quote(&fk.ref_table),
                    quote(&fk.ref_column)
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TableDetail;

    fn col(name: &str, ty: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            type_name: Some(ty.to_string()),
            not_null: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        }
    }

    fn snapshot(engine: DbKind, tables: &[(&str, Vec<ColumnMeta>)]) -> SchemaSnapshot {
        let mut snap = SchemaSnapshot {
            engine,
            namespace: "public".to_string(),
            objects: Vec::new(),
            details: std::collections::HashMap::new(),
        };
        for (name, columns) in tables {
            snap.objects.push(ObjectMeta {
                name: name.to_string(),
                kind: ObjectKind::Table,
            });
            snap.details.insert(
                name.to_string(),
                TableDetail {
                    columns: columns.clone(),
                    ..Default::default()
                },
            );
        }
        snap
    }

    #[test]
    fn identical_schemas_have_no_delta() {
        let a = snapshot(DbKind::Postgres, &[("orders", vec![col("id", "integer")])]);
        let b = snapshot(DbKind::Postgres, &[("orders", vec![col("id", "integer")])]);
        assert!(compare(&a, &b).is_empty());
    }

    #[test]
    fn objects_present_on_one_side_only_are_added_or_removed() {
        let a = snapshot(DbKind::Postgres, &[("orders", vec![col("id", "integer")])]);
        let b = snapshot(
            DbKind::Postgres,
            &[
                ("orders", vec![col("id", "integer")]),
                ("invoices", vec![col("id", "integer")]),
            ],
        );
        let d = compare(&a, &b);
        assert_eq!(d.objects_added.len(), 1);
        assert_eq!(d.objects_added[0].name, "invoices");
        assert!(d.objects_removed.is_empty());

        // Reversed, the same pair reads as a removal.
        let d = compare(&b, &a);
        assert_eq!(d.objects_removed.len(), 1);
        assert!(d.objects_added.is_empty());
    }

    /// The whole reason the type lattice is in this path: the same column spelled
    /// two ways across two engines is not a difference.
    #[test]
    fn cross_engine_type_spellings_of_one_class_are_not_a_difference() {
        let pg = snapshot(
            DbKind::Postgres,
            &[("t", vec![col("name", "character varying(255)")])],
        );
        let my = snapshot(DbKind::Mysql, &[("t", vec![col("name", "varchar(255)")])]);
        let d = compare(&pg, &my);
        assert!(
            d.is_empty(),
            "varchar and character varying are one class: {:?}",
            d.tables_changed
        );
    }

    /// The same engine, however, does compare spellings: a width change is real.
    #[test]
    fn same_engine_width_change_is_a_difference() {
        let a = snapshot(DbKind::Mysql, &[("t", vec![col("name", "varchar(50)")])]);
        let b = snapshot(DbKind::Mysql, &[("t", vec![col("name", "varchar(255)")])]);
        let d = compare(&a, &b);
        assert_eq!(d.tables_changed.len(), 1);
        let change = &d.tables_changed[0].columns_changed[0];
        assert_eq!(change.confidence, Confidence::Certain);
        assert!(change.summary.contains("varchar(50) → varchar(255)"));
    }

    #[test]
    fn different_classes_are_a_difference_even_across_engines() {
        let a = snapshot(DbKind::Postgres, &[("t", vec![col("qty", "integer")])]);
        let b = snapshot(DbKind::Mysql, &[("t", vec![col("qty", "varchar(10)")])]);
        let d = compare(&a, &b);
        assert_eq!(d.tables_changed[0].columns_changed.len(), 1);
    }

    /// A type the lattice cannot classify is reported, but marked uncertain
    /// rather than asserted.
    #[test]
    fn unclassifiable_types_compare_by_string_and_are_uncertain() {
        let a = snapshot(DbKind::Postgres, &[("t", vec![col("doc", "tsvector")])]);
        let b = snapshot(DbKind::Postgres, &[("t", vec![col("doc", "tsquery")])]);
        let d = compare(&a, &b);
        assert_eq!(
            d.tables_changed[0].columns_changed[0].confidence,
            Confidence::Uncertain
        );
    }

    #[test]
    fn nullability_and_default_changes_are_summarised() {
        let mut left = col("email", "text");
        let mut right = col("email", "text");
        right.not_null = true;
        right.default = Some("''".to_string());
        left.not_null = false;
        let a = snapshot(DbKind::Postgres, &[("t", vec![left])]);
        let b = snapshot(DbKind::Postgres, &[("t", vec![right])]);
        let d = compare(&a, &b);
        let summary = &d.tables_changed[0].columns_changed[0].summary;
        assert!(summary.contains("now NOT NULL"), "got {summary}");
        assert!(summary.contains("default"), "got {summary}");
    }

    /// The safety promise of the generated script: nothing destructive is spelled
    /// out as a runnable statement unless the caller explicitly asked.
    #[test]
    fn additive_script_comments_out_every_drop() {
        let a = snapshot(
            DbKind::Postgres,
            &[("t", vec![col("id", "integer"), col("legacy", "text")])],
        );
        let b = snapshot(DbKind::Postgres, &[("t", vec![col("id", "integer")])]);
        let d = compare(&a, &b);
        let quote = |s: &str| format!("\"{s}\"");
        let sql = d.to_sql("public", ScriptScope::Additive, &quote);
        for line in sql.lines().filter(|l| l.contains("DROP")) {
            assert!(
                line.trim_start().starts_with("--"),
                "an additive script must not emit a live DROP: {line}"
            );
        }
        // And with drops asked for, it is a real statement.
        let sql = d.to_sql("public", ScriptScope::IncludeDrops, &quote);
        assert!(
            sql.lines()
                .any(|l| l.starts_with("ALTER TABLE") && l.contains("DROP COLUMN")),
            "IncludeDrops should emit the statement: {sql}"
        );
    }

    #[test]
    fn added_column_becomes_an_add_column_statement() {
        let a = snapshot(DbKind::Postgres, &[("t", vec![col("id", "integer")])]);
        let mut added = col("note", "text");
        added.not_null = true;
        let b = snapshot(
            DbKind::Postgres,
            &[("t", vec![col("id", "integer"), added])],
        );
        let d = compare(&a, &b);
        let quote = |s: &str| format!("\"{s}\"");
        let sql = d.to_sql("public", ScriptScope::Additive, &quote);
        assert!(
            sql.contains(r#"ALTER TABLE "public"."t" ADD COLUMN "note" text NOT NULL;"#),
            "got {sql}"
        );
    }

    #[test]
    fn a_type_change_is_never_emitted_as_a_live_statement() {
        let a = snapshot(DbKind::Postgres, &[("t", vec![col("qty", "text")])]);
        let b = snapshot(DbKind::Postgres, &[("t", vec![col("qty", "integer")])]);
        let d = compare(&a, &b);
        let quote = |s: &str| format!("\"{s}\"");
        let sql = d.to_sql("public", ScriptScope::IncludeDrops, &quote);
        for line in sql.lines().filter(|l| l.contains("ALTER COLUMN")) {
            assert!(
                line.trim_start().starts_with("--"),
                "a type change is a data-loss risk and stays commented: {line}"
            );
        }
    }
}
