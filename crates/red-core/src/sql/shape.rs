//! Whether a read query answers the question it looks like it answers.
//!
//! [`risk`](super::risk) grades how *destructive* a statement is, and every
//! `SELECT` grades `Safe` there, correctly: a `SELECT` cannot destroy anything.
//! This module asks the other question. A join on the wrong key, an `INNER` where
//! `LEFT` was meant, an aggregate computed after a one-to-many join has already
//! tripled the rows -- the query runs, the number comes back, it is wrong by a
//! factor of three, and nothing throws. That is the most common way an answer
//! about a database is wrong, and it is invisible to every gate RED already has.
//!
//! Three rules shape everything here.
//!
//! **Scan the stripped copy.** Like every other scanner in this module tree, the
//! input is [`strip_noise`], so a `join` inside a `--` comment or a `, ` inside a
//! string literal cannot trigger anything.
//!
//! **Decline rather than guess.** This is a scanner, not a parser. A CTE chain, a
//! subquery in `FROM`, a `LATERAL` -- anything whose shape is not plainly
//! readable returns nothing at all. A false positive is noise, and a checker that
//! cries wolf is one the reader learns to skip, after which it protects nothing.
//! Missing a real hazard costs one wrong answer; training someone to ignore the
//! warnings costs all of them.
//!
//! **Warn, never block.** A cross join is sometimes exactly what was wanted (a
//! calendar spine, a deliberate matrix). The hard blocks in the write gate are
//! justified by irreversibility; a wrong `SELECT` is reversible by reading it
//! again.

use super::{Dialect, first_keyword, has_top_level_comma, strip_noise, top_level_word};

/// A structural hazard in a read query: something that runs fine and answers the
/// wrong question.
///
/// Structural rather than pre-worded, like [`Risk`](super::risk::Risk): the
/// phrasing belongs to whoever displays it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeWarning {
    /// Two or more relations with nothing correlating them: `FROM a, b` with no
    /// equality in the `WHERE`, or a `JOIN` with no `ON`/`USING`. Every row of
    /// one against every row of the other.
    CrossJoin,
    /// An aggregate over a query that joins more than one relation, with no
    /// `DISTINCT` to pin the grain. If any join is one-to-many the aggregate
    /// counts the same row several times.
    ///
    /// A `GROUP BY` deliberately does **not** suppress this: grouping the output
    /// changes which rows land in which bucket, not how many times a row was
    /// duplicated on the way in. `SUM` over a fanned-out join is wrong per group
    /// exactly as it is wrong in total.
    AggregateOverJoin { func: String, relations: usize },
    /// A join predicate with no equality in it -- `ON a.x > b.y`. Legitimate for
    /// range joins, and a classic accident otherwise.
    NonEquiJoin { predicate: String },
    /// `SELECT *` across a join: ambiguous column names, and the result's grain
    /// is invisible to whoever reads it.
    StarAcrossJoin,
    /// A plain `SELECT` with no `LIMIT`. Informational where the caller clamps
    /// anyway; real where the statement is handed to a grid.
    Unbounded,
}

/// The structural hazards in `sql`, or an empty vec when there are none *or* the
/// shape could not be read confidently. The two are deliberately the same answer:
/// a caller must not treat "no warnings" as "verified correct".
pub fn inspect(sql: &str, dialect: Dialect) -> Vec<ShapeWarning> {
    let stripped = strip_noise(sql, dialect);
    let lower = stripped.to_ascii_lowercase();
    // Only a plain SELECT. A `WITH` chain is exactly the shape this declines on:
    // the relations that matter are inside the CTEs, and reading them without a
    // parser would be guessing.
    if !first_keyword(&stripped).eq_ignore_ascii_case("select") {
        return Vec::new();
    }
    let Some(from_at) = top_level_word(&lower, "from") else {
        // `SELECT 1`, `SELECT now()`: nothing joins, nothing to say.
        return Vec::new();
    };
    let select_list = &lower[..from_at];
    let from = from_clause(&lower, from_at);

    // Shapes this scanner cannot read. A subquery or a lateral in `FROM` means the
    // relation count and the join predicates are both unreliable, so it says
    // nothing rather than something confident and wrong.
    if from.contains('(') || super::has_word(from, "lateral") || super::has_word(from, "unnest") {
        return Vec::new();
    }

    let relations = count_relations(from);
    if relations == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();

    // --- an uncorrelated product ---
    if relations > 1 && is_uncorrelated(&lower, from, from_at) {
        out.push(ShapeWarning::CrossJoin);
    }

    // --- an aggregate over a join ---
    if relations > 1
        && !super::has_word(&lower, "distinct")
        && let Some(func) = aggregate_in(select_list)
    {
        out.push(ShapeWarning::AggregateOverJoin { func, relations });
    }

    // --- a join predicate with no equality ---
    for predicate in on_predicates(from) {
        if !predicate.contains('=') {
            out.push(ShapeWarning::NonEquiJoin {
                predicate: predicate.trim().to_string(),
            });
        }
    }

    // --- `SELECT *` across a join ---
    if relations > 1 && select_list.contains('*') {
        out.push(ShapeWarning::StarAcrossJoin);
    }

    // --- unbounded ---
    if top_level_word(&lower, "limit").is_none() && top_level_word(&lower, "fetch").is_none() {
        out.push(ShapeWarning::Unbounded);
    }
    out
}

/// The `FROM` clause's text: everything from just past `from` to the next
/// top-level clause keyword, lower-cased and noise-stripped like its input.
fn from_clause(lower: &str, from_at: usize) -> &str {
    lower
        .get(from_at + 4..from_at + 4 + from_clause_len(lower, from_at))
        .unwrap_or("")
        .trim()
}

/// The `FROM` clause's length from just past `from`, **untrimmed**, so a caller
/// slicing the original at the same offsets stays byte-aligned with it.
fn from_clause_len(lower: &str, from_at: usize) -> usize {
    let start = from_at + 4;
    let end = [
        "where",
        "group",
        "having",
        "window",
        "order",
        "limit",
        "fetch",
        "union",
        "intersect",
        "except",
        "into",
        "for",
    ]
    .iter()
    .filter_map(|w| top_level_word(lower, w))
    .filter(|at| *at > start)
    .min()
    .unwrap_or(lower.len());
    end.saturating_sub(start)
}

/// How many relations the `FROM` clause names: one per top-level comma segment,
/// plus one per `JOIN`.
fn count_relations(from: &str) -> usize {
    if from.is_empty() {
        return 0;
    }
    let commas = top_level_segments(from).len();
    let joins = count_word(from, "join");
    commas + joins
}

/// Whether nothing correlates the relations, which makes the result a product.
///
/// Two shapes qualify. A comma-joined `FROM` whose `WHERE` carries no equality at
/// all: `FROM a, b WHERE a.active` filters but never correlates. And a `JOIN`
/// with no `ON`/`USING` of its own -- excluding `CROSS`/`NATURAL`, which say what
/// they mean and are therefore deliberate.
///
/// The `WHERE`-has-an-equality case declines instead of resolving aliases:
/// `FROM a, b WHERE a.x = b.y` is old-style but correct, and telling it apart
/// from `FROM a, b WHERE a.x = 1` needs to know which side each column belongs
/// to. Declining costs a missed warning; guessing costs the reader's trust.
fn is_uncorrelated(lower: &str, from: &str, from_at: usize) -> bool {
    let explicit = count_word(from, "cross") + count_word(from, "natural");
    let joins = count_word(from, "join");
    if joins.saturating_sub(explicit) > count_word(from, "on") + count_word(from, "using") {
        return true;
    }
    // Comma-joined relations with no equality anywhere in the WHERE to tie them.
    if top_level_segments(from).len() > 1 {
        let where_clause = top_level_word(lower, "where")
            .filter(|at| *at > from_at)
            .map(|at| from_clause_after(lower, at + 5))
            .unwrap_or("");
        return !where_clause.contains('=');
    }
    false
}

/// Everything from `start` to the next clause keyword; the `WHERE` analogue of
/// [`from_clause`].
fn from_clause_after(lower: &str, start: usize) -> &str {
    let end = [
        "group", "having", "window", "order", "limit", "fetch", "union",
    ]
    .iter()
    .filter_map(|w| top_level_word(lower, w))
    .filter(|at| *at > start)
    .min()
    .unwrap_or(lower.len());
    lower.get(start..end).unwrap_or("")
}

/// The `ON` predicates in a `FROM` clause, one per join, as written.
fn on_predicates(from: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = from.as_bytes();
    let mut i = 0;
    while let Some(at) = find_word_from(from, "on", i) {
        let start = at + 2;
        // The predicate runs to the next join-ish keyword, or the end.
        let end = ["join", "inner", "left", "right", "full", "cross", "natural"]
            .iter()
            .filter_map(|w| find_word_from(from, w, start))
            .min()
            .unwrap_or(bytes.len());
        if let Some(text) = from.get(start..end) {
            out.push(text);
        }
        i = end.max(start);
    }
    out
}

/// The aggregate function called at the top level of a select list, if any.
/// Matched as `func` immediately followed by `(`, so a column named `sum` is not
/// mistaken for the function.
fn aggregate_in(select_list: &str) -> Option<String> {
    const AGGREGATES: [&str; 5] = ["sum", "avg", "count", "min", "max"];
    let bytes = select_list.as_bytes();
    for func in AGGREGATES {
        let mut i = 0;
        while let Some(at) = find_word_from(select_list, func, i) {
            let after = at + func.len();
            // Allow whitespace between the name and its parenthesis.
            let mut j = after;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'(') {
                return Some(func.to_string());
            }
            i = after;
        }
    }
    None
}

/// The `FROM` clause split on top-level commas.
fn top_level_segments(from: &str) -> Vec<&str> {
    if !has_top_level_comma(from) {
        return vec![from];
    }
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in from.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&from[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&from[start..]);
    out
}

/// How many times `word` appears as a whole word at paren depth 0.
fn count_word(haystack: &str, word: &str) -> usize {
    let mut n = 0;
    let mut i = 0;
    while let Some(at) = find_word_from(haystack, word, i) {
        n += 1;
        i = at + word.len();
    }
    n
}

/// The offset of `word` as a whole word at paren depth 0, at or after `from`.
fn find_word_from(haystack: &str, word: &str, from: usize) -> Option<usize> {
    let tail = haystack.get(from..)?;
    top_level_word(tail, word).map(|at| at + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dialect, since the point of taking one is that comment and quoting
    /// rules differ and scanning with the wrong ones is the drift this module
    /// tree exists to prevent.
    const DIALECTS: [Dialect; 5] = [
        Dialect::Generic,
        Dialect::Postgres,
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::ClickHouse,
    ];

    /// Warnings other than `Unbounded`, which fires on nearly everything and
    /// would drown the assertions below.
    fn hazards(sql: &str, dialect: Dialect) -> Vec<ShapeWarning> {
        inspect(sql, dialect)
            .into_iter()
            .filter(|w| *w != ShapeWarning::Unbounded)
            .collect()
    }

    fn on_every_dialect(sql: &str, want: &[ShapeWarning]) {
        for dialect in DIALECTS {
            assert_eq!(hazards(sql, dialect), want, "{dialect:?}: {sql}");
        }
    }

    /// The shape this module exists for: an aggregate sitting above a join that
    /// may be one-to-many.
    #[test]
    fn flags_an_aggregate_over_a_join() {
        on_every_dialect(
            "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id",
            &[ShapeWarning::AggregateOverJoin {
                func: "sum".into(),
                relations: 2,
            }],
        );
    }

    /// The negative cases matter more than the positive ones: a checker that
    /// cries wolf gets ignored, after which it protects nothing.
    #[test]
    fn stays_quiet_on_shapes_that_cannot_fan_out() {
        // One relation: nothing to multiply.
        on_every_dialect("SELECT SUM(total) FROM orders", &[]);
        // DISTINCT pins the grain.
        on_every_dialect(
            "SELECT COUNT(DISTINCT o.id) FROM orders o JOIN items i ON i.order_id = o.id",
            &[],
        );
        // A join with no aggregate above it is just a join.
        on_every_dialect(
            "SELECT o.id, i.sku FROM orders o JOIN items i ON i.order_id = o.id",
            &[],
        );
        // No FROM at all.
        on_every_dialect("SELECT 1", &[]);
    }

    /// Old-style comma joins: correlated in the WHERE is correct SQL, and must
    /// not be flagged just because the predicate isn't in an `ON`.
    #[test]
    fn a_comma_join_correlated_in_the_where_is_not_a_cross_join() {
        on_every_dialect("SELECT a.x FROM a, b WHERE a.x = b.y", &[]);
        // Nothing correlating them at all: a genuine product.
        on_every_dialect("SELECT a.x FROM a, b", &[ShapeWarning::CrossJoin]);
        // A WHERE that filters but never correlates is still a product.
        on_every_dialect(
            "SELECT a.x FROM a, b WHERE a.active",
            &[ShapeWarning::CrossJoin],
        );
    }

    /// A `JOIN` with no `ON` is an accident; `CROSS`/`NATURAL JOIN` say what they
    /// mean and are left alone.
    #[test]
    fn a_join_without_a_predicate_is_flagged_unless_it_says_it_means_it() {
        on_every_dialect("SELECT a.x FROM a JOIN b", &[ShapeWarning::CrossJoin]);
        on_every_dialect("SELECT a.x FROM a CROSS JOIN b", &[]);
        on_every_dialect("SELECT a.x FROM a NATURAL JOIN b", &[]);
        on_every_dialect("SELECT a.x FROM a JOIN b USING (id)", &[]);
    }

    #[test]
    fn flags_a_join_predicate_with_no_equality() {
        let warnings = hazards(
            "SELECT a.x FROM a JOIN b ON a.day > b.day",
            Dialect::Generic,
        );
        assert!(
            matches!(&warnings[..], [ShapeWarning::NonEquiJoin { predicate }] if predicate.contains('>')),
            "{warnings:?}"
        );
        // An equality anywhere in the predicate is enough.
        on_every_dialect(
            "SELECT a.x FROM a JOIN b ON a.id = b.id AND a.day > b.day",
            &[],
        );
    }

    /// The reason the input is the stripped copy. Both of these would light up
    /// every warning in the module if it scanned the raw text.
    #[test]
    fn comments_and_literals_cannot_trigger_anything() {
        on_every_dialect("SELECT total FROM orders -- join items on nothing\n", &[]);
        on_every_dialect("SELECT total FROM orders /* a, b */", &[]);
        on_every_dialect("SELECT 'orders, items' AS note FROM orders", &[]);
    }

    /// Shapes the scanner cannot read: it declines rather than guessing, and
    /// "no warnings" therefore never means "verified correct".
    #[test]
    fn declines_on_shapes_it_cannot_read() {
        // A CTE chain: the relations that matter are inside it.
        on_every_dialect(
            "WITH x AS (SELECT * FROM a, b) SELECT SUM(v) FROM x JOIN c ON c.id = x.id",
            &[],
        );
        // A subquery in FROM.
        on_every_dialect(
            "SELECT SUM(t.v) FROM (SELECT * FROM a JOIN b ON a.id = b.id) t",
            &[],
        );
        // A lateral.
        on_every_dialect("SELECT SUM(a.v) FROM a, LATERAL (SELECT 1) b", &[]);
    }

    /// `SELECT *` across a join hides the result's grain from whoever reads it.
    #[test]
    fn flags_a_star_across_a_join() {
        on_every_dialect(
            "SELECT * FROM a JOIN b ON a.id = b.id",
            &[ShapeWarning::StarAcrossJoin],
        );
        // One relation: a star is just a star.
        on_every_dialect("SELECT * FROM a", &[]);
    }

    /// The probe cuts both counts out of the *original* statement, so literals
    /// survive and the two differ only in how much of the `FROM` they carry.
    #[test]
    fn the_probe_cuts_both_counts_from_the_original() {
        let probe = join_probe(
            "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id \
             WHERE o.status = 'paid'",
            Dialect::Generic,
        )
        .expect("a plain join probes");
        assert_eq!(probe.table, "orders");
        assert_eq!(probe.base, "SELECT * FROM orders o WHERE o.status = 'paid'");
        assert_eq!(
            probe.joined,
            "SELECT * FROM orders o JOIN items i ON i.order_id = o.id WHERE o.status = 'paid'"
        );
    }

    /// The guard that keeps the annotation honest: a `WHERE` naming another
    /// relation cannot be applied to the driving table alone, and dropping it
    /// would compare a filtered count against an unfiltered one and report a
    /// fan-out that isn't there.
    #[test]
    fn the_probe_declines_a_predicate_it_cannot_reuse() {
        assert_eq!(
            join_probe(
                "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id \
                 WHERE i.sku = 'x'",
                Dialect::Generic
            ),
            None
        );
        // No WHERE at all is fine: both counts are unfiltered.
        let probe = join_probe(
            "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id",
            Dialect::Generic,
        )
        .expect("an unfiltered join probes");
        assert_eq!(probe.base, "SELECT * FROM orders o");
        // A decimal point is not a qualifier.
        assert!(
            join_probe(
                "SELECT SUM(o.total) FROM orders o JOIN items i ON i.order_id = o.id \
                 WHERE o.total > 1.5",
                Dialect::Generic
            )
            .is_some()
        );
    }

    #[test]
    fn unbounded_fires_only_without_a_row_bound() {
        assert!(inspect("SELECT x FROM a", Dialect::Generic).contains(&ShapeWarning::Unbounded));
        assert!(
            !inspect("SELECT x FROM a LIMIT 10", Dialect::Generic)
                .contains(&ShapeWarning::Unbounded)
        );
        // Postgres/ANSI `FETCH FIRST` counts too.
        assert!(
            !inspect(
                "SELECT x FROM a FETCH FIRST 10 ROWS ONLY",
                Dialect::Postgres
            )
            .contains(&ShapeWarning::Unbounded)
        );
    }
}

/// The two counts that tell you whether a join fanned out, as SQL fragments cut
/// from the original statement.
///
/// The static pass can see that an aggregate sits above a join; only the engine
/// knows whether that join is one-to-many. Running both of these and comparing
/// them answers it: if the join multiplies rows, every aggregate above it counts
/// the same driving row more than once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinProbe {
    /// The driving relation's name, for the message.
    pub table: String,
    /// `FROM <driving relation> [WHERE <predicate>]`, sliced from the original so
    /// literals are intact.
    pub base: String,
    /// The same, with the whole `FROM` clause.
    pub joined: String,
}

/// The fan-out probe for `sql`, or `None` when it cannot be built safely.
///
/// `None` for anything the scanner cannot cut cleanly, and in particular for a
/// `WHERE` that references a relation other than the driving one: reusing such a
/// predicate against the driving table alone would not even parse, and dropping
/// it would compare a filtered count against an unfiltered one and report a
/// fan-out that isn't there. A missing probe costs one annotation; a wrong one
/// costs the reader's trust in all of them.
pub fn join_probe(sql: &str, dialect: Dialect) -> Option<JoinProbe> {
    let stripped = strip_noise(sql, dialect);
    let lower = stripped.to_ascii_lowercase();
    if !first_keyword(&stripped).eq_ignore_ascii_case("select") {
        return None;
    }
    let from_at = top_level_word(&lower, "from")?;
    let from_lower = from_clause(&lower, from_at);
    if from_lower.contains('(') || super::has_word(from_lower, "lateral") {
        return None;
    }
    // Offsets into the original: `strip_noise` maps one byte to one byte, so a
    // span found in the stripped copy cuts the real SQL at the same place.
    let from_start = from_at + 4;
    let raw_from = lower
        .get(from_start..)?
        .get(..from_clause_len(&lower, from_at))?;
    let from_text = sql.get(from_start..from_start + raw_from.len())?.trim();

    // The driving relation is everything up to the first top-level comma or join
    // keyword -- `orders o` out of `orders o JOIN items i ON ...`.
    let rel_end = [
        "join", "inner", "left", "right", "full", "cross", "natural", ",",
    ]
    .iter()
    .filter_map(|w| {
        if *w == "," {
            raw_from.find(',')
        } else {
            top_level_word(raw_from, w)
        }
    })
    .min()
    .unwrap_or(raw_from.len());
    let driving_text = sql
        .get(from_start..from_start + rel_end)?
        .trim()
        .to_string();
    let mut words = raw_from.get(..rel_end)?.split_whitespace();
    let driving = words.next()?;
    // `orders o`, `orders AS o`, or just `orders`.
    let alias = match words.next() {
        Some("as") => words.next().unwrap_or(driving),
        Some(w) => w,
        None => driving,
    };

    // The WHERE, only when it is safe to apply to the driving relation alone.
    let where_text = match top_level_word(&lower, "where").filter(|at| *at > from_at) {
        Some(at) => {
            let clause_lower = from_clause_after(&lower, at + 5);
            if !qualifiers_only(clause_lower, driving, alias) {
                return None;
            }
            let start = at + 5;
            sql.get(start..start + clause_lower.len())
                .map(|w| format!(" WHERE {}", w.trim()))
        }
        None => Some(String::new()),
    }?;

    Some(JoinProbe {
        table: driving.to_string(),
        base: format!("SELECT * FROM {driving_text}{where_text}"),
        joined: format!("SELECT * FROM {from_text}{where_text}"),
    })
}

/// Whether every `alias.column` qualifier in `clause` names the driving relation.
///
/// An unqualified column is accepted: on a query whose `FROM` this scanner
/// already vetted, it resolves against the driving relation or fails to parse,
/// and a probe that fails to parse is a missing annotation rather than a wrong one.
fn qualifiers_only(clause: &str, table: &str, alias: &str) -> bool {
    let bytes = clause.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if *c != b'.' {
            continue;
        }
        // Walk back over the qualifier immediately before the dot.
        let mut start = i;
        while start > 0 && super::is_word_byte(bytes[start - 1]) {
            start -= 1;
        }
        let qualifier = &clause[start..i];
        // A numeric literal's decimal point is not a qualifier.
        if qualifier.is_empty() || qualifier.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if qualifier != table && qualifier != alias {
            return false;
        }
    }
    true
}
