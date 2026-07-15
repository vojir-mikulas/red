//! Data-compare (table diff) core: the pure merge-walk that aligns two
//! key-ordered row streams and classifies each row as added / removed / changed
//! (see docs/plans/todo/data-diff.md). UI- and runtime-free — it operates on
//! [`Value`] rows and knows nothing about drivers, cursors, or the UI. The
//! streaming backend job (`red-service`) feeds it one window per side and never
//! materializes either table whole; the same [`DiffAccumulator::step`] logic is
//! exercised by [`diff_sorted`] over in-memory vectors in tests.
//!
//! **Alignment assumption:** both sides arrive sorted by the *same* key column in
//! the *same* engine, so the engine's `ORDER BY` and this module's [`compare_keys`]
//! agree. That holds for a same-engine diff (the shipped scope); a cross-engine
//! diff with an unusual collation is best-effort (documented in the plan).

use std::cmp::Ordering;

use crate::Value;

/// How one aligned row differs between the two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// Present on the right, absent on the left.
    Added,
    /// Present on the left, absent on the right.
    Removed,
    /// Same key on both sides, but at least one compared cell differs.
    Changed,
}

/// One materialized diff row (unchanged rows are counted, never stored). `left`
/// and `right` are projected to the compared-column order ([`DiffColumnPlan::columns`]);
/// `left` is empty for [`DiffKind::Added`], `right` empty for [`DiffKind::Removed`].
/// `changed[i]` marks a differing compared cell (all false unless [`DiffKind::Changed`]).
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub kind: DiffKind,
    pub key: String,
    pub left: Vec<Value>,
    pub right: Vec<Value>,
    pub changed: Vec<bool>,
}

/// Running totals across the whole diff, including rows past the stored cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSummary {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub unchanged: usize,
}

/// The column alignment the diff ran under: the compared columns (present on both
/// sides, in the left table's order, including the `key`) and the columns that
/// exist on only one side (reported, never compared).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffColumnPlan {
    pub key: String,
    pub columns: Vec<String>,
    pub left_only: Vec<String>,
    pub right_only: Vec<String>,
}

/// Which side(s) the caller should advance after a [`DiffAccumulator::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    Left,
    Right,
    Both,
    Done,
}

/// Accumulates the diff as the caller walks both key-ordered streams. Holds only
/// the config plus the bounded result set (added/removed/changed rows, capped)
/// and the running [`DiffSummary`] — one window per side lives in the caller, so
/// memory stays bounded by the cap, not by row count.
#[derive(Debug, Clone)]
pub struct DiffAccumulator {
    /// Index of the key column within a projected (compared-order) row; both sides
    /// share it because both are selected in the same compared-column order.
    key_index: usize,
    /// Max diff rows to retain; further diffs bump `truncated` and are dropped.
    cap: usize,
    pub summary: DiffSummary,
    pub rows: Vec<DiffRow>,
    /// True once a diff row was dropped because the store hit `cap`.
    pub truncated: bool,
}

impl DiffAccumulator {
    pub fn new(key_index: usize, cap: usize) -> Self {
        Self {
            key_index,
            cap,
            summary: DiffSummary::default(),
            rows: Vec::new(),
            truncated: false,
        }
    }

    /// Inspect the current head of each side, record the diff that head-comparison
    /// implies, and return which side(s) to advance. `None` means that side is
    /// exhausted. This is the whole classification: streaming and in-memory callers
    /// share it, so the tested behaviour is the shipped behaviour.
    pub fn step(&mut self, left: Option<&[Value]>, right: Option<&[Value]>) -> Advance {
        match (left, right) {
            (None, None) => Advance::Done,
            (Some(l), None) => {
                self.removed(l);
                Advance::Left
            }
            (None, Some(r)) => {
                self.added(r);
                Advance::Right
            }
            (Some(l), Some(r)) => match compare_keys(l, r, self.key_index) {
                Ordering::Less => {
                    self.removed(l);
                    Advance::Left
                }
                Ordering::Greater => {
                    self.added(r);
                    Advance::Right
                }
                Ordering::Equal => {
                    self.pair(l, r);
                    Advance::Both
                }
            },
        }
    }

    fn key_of(&self, row: &[Value]) -> String {
        row.get(self.key_index).map(render_key).unwrap_or_default()
    }

    fn store(&mut self, row: DiffRow) {
        if self.rows.len() < self.cap {
            self.rows.push(row);
        } else {
            self.truncated = true;
        }
    }

    fn added(&mut self, right: &[Value]) {
        self.summary.added += 1;
        let key = self.key_of(right);
        self.store(DiffRow {
            kind: DiffKind::Added,
            key,
            left: Vec::new(),
            right: right.to_vec(),
            changed: Vec::new(),
        });
    }

    fn removed(&mut self, left: &[Value]) {
        self.summary.removed += 1;
        let key = self.key_of(left);
        self.store(DiffRow {
            kind: DiffKind::Removed,
            key,
            left: left.to_vec(),
            right: Vec::new(),
            changed: Vec::new(),
        });
    }

    fn pair(&mut self, left: &[Value], right: &[Value]) {
        let changed: Vec<bool> = left.iter().zip(right.iter()).map(|(l, r)| l != r).collect();
        if changed.iter().any(|&c| c) {
            self.summary.changed += 1;
            let key = self.key_of(left);
            self.store(DiffRow {
                kind: DiffKind::Changed,
                key,
                left: left.to_vec(),
                right: right.to_vec(),
                changed,
            });
        } else {
            self.summary.unchanged += 1;
        }
    }
}

/// Compare the key columns of two projected rows for merge ordering. Numeric keys
/// compare numerically, text lexicographically, with a stable rank for mixed or
/// null keys so the walk is always total.
pub fn compare_keys(left: &[Value], right: &[Value], key_index: usize) -> Ordering {
    match (left.get(key_index), right.get(key_index)) {
        (Some(a), Some(b)) => key_cmp(a, b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

/// A total order over key values. Same-typed keys compare naturally (ints
/// numerically, text lexicographically); differing types fall back to a fixed
/// rank so the merge never stalls on an unexpected shape.
fn key_cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Integer(x), Value::Real(y)) => {
            (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal)
        }
        (Value::Real(x), Value::Integer(y)) => {
            x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal)
        }
        (Value::Text(x), Value::Text(y)) => x.as_ref().cmp(y.as_ref()),
        (Value::Null, Value::Null) => Ordering::Equal,
        _ => key_rank(a).cmp(&key_rank(b)),
    }
}

/// A fixed rank per value shape, so mixed-type keys have a deterministic order.
fn key_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Integer(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
        Value::Capped(_) => 4,
    }
}

/// A short textual rendering of a key value, for the diff row's display label.
fn render_key(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        Value::Capped(_) => "<capped>".to_string(),
    }
}

/// Drive the merge-walk over two already-sorted, already-projected row vectors:
/// the in-memory counterpart to the streaming backend job, and the entry point
/// the tests exercise. `key_index` is the key's position in each projected row;
/// `cap` bounds the stored diff rows.
pub fn diff_sorted(
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    key_index: usize,
    cap: usize,
) -> DiffAccumulator {
    let mut acc = DiffAccumulator::new(key_index, cap);
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        let l = left.get(i).map(Vec::as_slice);
        let r = right.get(j).map(Vec::as_slice);
        match acc.step(l, r) {
            Advance::Done => break,
            Advance::Left => i += 1,
            Advance::Right => j += 1,
            Advance::Both => {
                i += 1;
                j += 1;
            }
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn int_row(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Integer(id), Value::Text(Arc::from(name))]
    }

    #[test]
    fn aligns_and_classifies_added_removed_changed_unchanged() {
        // key column is index 0.
        let left = vec![int_row(1, "a"), int_row(2, "b"), int_row(3, "c")];
        // 1 unchanged, 2 changed, 3 removed, 4 added.
        let right = vec![int_row(1, "a"), int_row(2, "B"), int_row(4, "d")];
        let acc = diff_sorted(left, right, 0, 1000);
        assert_eq!(acc.summary.unchanged, 1);
        assert_eq!(acc.summary.changed, 1);
        assert_eq!(acc.summary.removed, 1); // key 3
        assert_eq!(acc.summary.added, 1); // key 4

        // Rows are emitted in key order: 2 (changed), 3 (removed), 4 (added).
        let kinds: Vec<(DiffKind, &str)> =
            acc.rows.iter().map(|r| (r.kind, r.key.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (DiffKind::Changed, "2"),
                (DiffKind::Removed, "3"),
                (DiffKind::Added, "4"),
            ]
        );
        // The changed row flags only the differing (name) column.
        let changed = acc
            .rows
            .iter()
            .find(|r| r.kind == DiffKind::Changed)
            .unwrap();
        assert_eq!(changed.changed, vec![false, true]);
    }

    #[test]
    fn empty_sides_are_all_added_or_all_removed() {
        let rows = vec![int_row(1, "a"), int_row(2, "b")];
        let added = diff_sorted(Vec::new(), rows.clone(), 0, 1000);
        assert_eq!(added.summary.added, 2);
        assert_eq!(added.summary.removed, 0);

        let removed = diff_sorted(rows, Vec::new(), 0, 1000);
        assert_eq!(removed.summary.removed, 2);
        assert_eq!(removed.summary.added, 0);
    }

    #[test]
    fn identical_tables_have_no_diff_rows() {
        let rows = vec![int_row(1, "a"), int_row(2, "b"), int_row(3, "c")];
        let acc = diff_sorted(rows.clone(), rows, 0, 1000);
        assert_eq!(acc.summary.unchanged, 3);
        assert!(acc.rows.is_empty());
        assert!(!acc.truncated);
    }

    #[test]
    fn text_keys_merge_lexicographically() {
        let left = vec![int_row(0, "alpha"), int_row(0, "gamma")];
        let right = vec![int_row(0, "alpha"), int_row(0, "beta")];
        // Key on the text column (index 1).
        let acc = diff_sorted(left, right, 1, 1000);
        // alpha unchanged; beta added; gamma removed.
        assert_eq!(acc.summary.unchanged, 1);
        assert_eq!(acc.summary.added, 1);
        assert_eq!(acc.summary.removed, 1);
        let keys: Vec<&str> = acc.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["beta", "gamma"]);
    }

    #[test]
    fn cap_truncates_stored_rows_but_not_the_summary() {
        let left: Vec<Vec<Value>> = (0..10).map(|i| int_row(i, "x")).collect();
        let right: Vec<Vec<Value>> = (100..110).map(|i| int_row(i, "y")).collect();
        // Every left row is removed, every right row added: 20 diffs, cap 5.
        let acc = diff_sorted(left, right, 0, 5);
        assert_eq!(acc.rows.len(), 5);
        assert!(acc.truncated);
        assert_eq!(acc.summary.removed, 10);
        assert_eq!(acc.summary.added, 10);
    }
}
