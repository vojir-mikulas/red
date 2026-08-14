//! The fast filter: a shorthand that compiles `status:active age>30 created:last7d`
//! into a MongoDB filter document, so the common narrowing does not cost a hand
//! written JSON object.
//!
//! Pure and total: no clock (the caller passes `now_ms`, which also makes the
//! relative-date terms testable), no engine types, no `serde_json`. The output is
//! extended-JSON *text*, parsed by the driver like any other filter, which is what
//! keeps `$oid` and `$date` typed without this module knowing what BSON is.
//!
//! The three-way outcome is the point. A filter bar that screams "invalid" at
//! `status:` while the user is still typing the value is worse than no validation,
//! so a term that is merely unfinished reports [`FastFilter::Incomplete`] and the
//! bar stays quiet.

use std::fmt::Write as _;

use super::write_json_string;

/// What a fast-filter line compiled to.
#[derive(Debug, Clone, PartialEq)]
pub enum FastFilter {
    /// Nothing to filter by (the line is blank).
    Empty,
    /// A filter document, as extended JSON.
    Ready(String),
    /// The line ends mid-term (`status:`, `age>`). Not an error: the user is
    /// still typing, and the bar should say nothing.
    Incomplete,
    /// The line cannot be read as a filter, with the reason.
    Invalid(String),
}

/// Milliseconds in a day, for the relative-date vocabulary.
const DAY_MS: i64 = 86_400_000;

/// Compile a fast-filter line. `now_ms` anchors the relative dates (`last7d`,
/// `today`); it is a parameter rather than a clock read so the same line always
/// compiles the same way in a test.
pub fn compile_fast_filter(input: &str, now_ms: i64) -> FastFilter {
    let terms = match split_terms(input) {
        Ok(terms) => terms,
        Err(e) => return FastFilter::Invalid(e),
    };
    if terms.is_empty() {
        return FastFilter::Empty;
    }

    // `(field, rendered constraint)` in source order. A field named twice keeps
    // both entries; they are merged under `$and` below, because a JSON object
    // cannot carry the same key twice and silently dropping one would filter by
    // something the user did not type.
    let mut clauses: Vec<(String, String)> = Vec::with_capacity(terms.len());
    for term in terms {
        match compile_term(&term, now_ms) {
            Ok(Some(clause)) => clauses.push(clause),
            Ok(None) => return FastFilter::Incomplete,
            Err(e) => return FastFilter::Invalid(e),
        }
    }

    let duplicated = clauses
        .iter()
        .enumerate()
        .any(|(i, (field, _))| clauses[..i].iter().any(|(prev, _)| prev == field));
    let mut out = String::from("{");
    if duplicated {
        out.push_str("\"$and\":[");
        for (i, (field, constraint)) in clauses.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            write_json_string(&mut out, field);
            out.push(':');
            out.push_str(constraint);
            out.push('}');
        }
        out.push(']');
    } else {
        for (i, (field, constraint)) in clauses.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(&mut out, field);
            out.push(':');
            out.push_str(constraint);
        }
    }
    out.push('}');
    FastFilter::Ready(out)
}

/// Split a line into terms on whitespace, keeping a double-quoted run together so
/// `name:"Ada Lovelace"` is one term. An unterminated quote is not an error: it is
/// a value the user is still typing, so the run to end-of-line becomes the term.
fn split_terms(input: &str) -> Result<Vec<String>, String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    terms.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    Ok(terms)
}

/// The comparison a term's separator asks for.
struct Op {
    /// Where the separator starts in the term.
    at: usize,
    /// How many bytes it spans.
    len: usize,
    /// The MongoDB operator, or `None` for plain equality.
    mongo: Option<&'static str>,
}

/// Find the term's separator. Longest match first, so `>=` is not read as `>`.
fn find_op(term: &str) -> Option<Op> {
    const OPS: [(&str, Option<&str>); 7] = [
        (">=", Some("$gte")),
        ("<=", Some("$lte")),
        ("!=", Some("$ne")),
        ("!:", Some("$ne")),
        (":", None),
        (">", Some("$gt")),
        ("<", Some("$lt")),
    ];
    OPS.iter()
        .filter_map(|(sep, mongo)| {
            term.find(sep).map(|at| Op {
                at,
                len: sep.len(),
                mongo: *mongo,
            })
        })
        // The separator nearest the start belongs to this term; a later one is
        // part of the value (a URL, a timestamp).
        .min_by_key(|op| (op.at, std::cmp::Reverse(op.len)))
}

/// Compile one term into `(field, constraint JSON)`. `Ok(None)` means the term is
/// unfinished, which the caller reports as [`FastFilter::Incomplete`].
fn compile_term(term: &str, now_ms: i64) -> Result<Option<(String, String)>, String> {
    let Some(op) = find_op(term) else {
        // A bare word is not a filter. It is almost always a field name the user
        // has not finished, so it reads as unfinished rather than wrong.
        return Ok(None);
    };
    let field = &term[..op.at];
    if field.is_empty() {
        return Err(format!("`{term}` has no field name before the operator"));
    }
    let raw = &term[op.at + op.len..];
    if raw.is_empty() {
        return Ok(None);
    }

    // `field:*` asks whether the field is there at all, the one question a value
    // comparison cannot express.
    if raw == "*" && op.mongo.is_none() {
        return Ok(Some((field.to_string(), "{\"$exists\":true}".to_string())));
    }
    // `field:~text` is a case-insensitive contains, the search a filter bar is
    // reached for most often.
    if let Some(needle) = raw.strip_prefix('~') {
        if needle.is_empty() {
            return Ok(None);
        }
        let mut out = String::from("{\"$regex\":");
        write_json_string(&mut out, &regex_escape(&unquote(needle)));
        out.push_str(",\"$options\":\"i\"}");
        return Ok(Some((field.to_string(), out)));
    }

    let value = match relative_range(raw, now_ms) {
        // A relative date is a range, so it carries its own operator and ignores
        // the term's: `created:last7d` means "since", never "equals".
        Some(range) => return Ok(Some((field.to_string(), range))),
        None => scalar_json(&unquote(raw), field),
    };
    Ok(Some(match op.mongo {
        None => (field.to_string(), value),
        Some(mongo) => (field.to_string(), format!("{{\"{mongo}\":{value}}}")),
    }))
}

/// Strip a surrounding pair of double quotes, if present.
fn unquote(raw: &str) -> String {
    let trimmed = raw
        .strip_prefix('"')
        .map(|s| s.strip_suffix('"').unwrap_or(s))
        .unwrap_or(raw);
    trimmed.to_string()
}

/// The `{"$gte": {"$date": …}}` a relative-date word means, or `None` when the
/// word is not one. `today` is the current UTC day; `lastNd` / `lastNh` count back
/// from `now_ms`.
fn relative_range(raw: &str, now_ms: i64) -> Option<String> {
    let since = if raw == "today" {
        now_ms - now_ms.rem_euclid(DAY_MS)
    } else {
        let rest = raw.strip_prefix("last")?;
        let (digits, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
        let n: i64 = digits.parse().ok()?;
        match unit {
            "d" => now_ms - n * DAY_MS,
            "h" => now_ms - n * 3_600_000,
            "m" => now_ms - n * 60_000,
            _ => return None,
        }
    };
    Some(format!(
        "{{\"$gte\":{{\"$date\":{{\"$numberLong\":\"{since}\"}}}}}}"
    ))
}

/// Render a value as JSON, reading its type from its spelling: booleans, `null`,
/// numbers and ObjectId hex are typed; everything else is a string.
///
/// `field` decides only the ObjectId case: a 24-hex run is an ObjectId in an `_id`
/// field and a string anywhere else, because a hex-looking hash or token in an
/// ordinary field is far more common than an ObjectId stored outside `_id`.
fn scalar_json(value: &str, field: &str) -> String {
    match value {
        "true" => return "true".to_string(),
        "false" => return "false".to_string(),
        "null" => return "null".to_string(),
        _ => {}
    }
    if is_id_field(field) && value.len() == 24 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        let mut out = String::from("{\"$oid\":");
        write_json_string(&mut out, value);
        out.push('}');
        return out;
    }
    // `parse` accepts `inf`/`NaN`, which JSON has no spelling for; require the
    // text to look like a JSON number before trusting it as one.
    if looks_numeric(value) && value.parse::<f64>().is_ok() {
        return value.to_string();
    }
    let mut out = String::new();
    let _ = write!(out, "");
    write_json_string(&mut out, value);
    out
}

/// Whether a field addresses an identity (`_id`, or a path ending in `._id`).
fn is_id_field(field: &str) -> bool {
    field == "_id" || field.ends_with("._id")
}

fn looks_numeric(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        && value.chars().any(|c| c.is_ascii_digit())
}

/// Escape the regex metacharacters in a contains term, so `a.b` matches the literal
/// text rather than any character between `a` and `b`.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if "\\^$.|?*+()[]{}".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-14T12:00:00Z, so the relative-date expectations are readable.
    const NOW: i64 = 1_786_824_000_000;

    fn ready(input: &str) -> String {
        match compile_fast_filter(input, NOW) {
            FastFilter::Ready(json) => json,
            other => panic!("expected a filter, got {other:?}"),
        }
    }

    #[test]
    fn equality_and_typed_scalars() {
        assert_eq!(ready("status:active"), r#"{"status":"active"}"#);
        assert_eq!(ready("age:30"), r#"{"age":30}"#);
        assert_eq!(ready("score:-1.5"), r#"{"score":-1.5}"#);
        assert_eq!(ready("active:true"), r#"{"active":true}"#);
        assert_eq!(ready("deleted:null"), r#"{"deleted":null}"#);
        // A quoted value keeps its spaces and stays a string.
        assert_eq!(
            ready(r#"name:"Ada Lovelace""#),
            r#"{"name":"Ada Lovelace"}"#
        );
        // A version-looking value is text, not a malformed number.
        assert_eq!(ready("v:1.2.3"), r#"{"v":"1.2.3"}"#);
    }

    #[test]
    fn comparisons_use_the_longest_operator() {
        assert_eq!(ready("age>30"), r#"{"age":{"$gt":30}}"#);
        assert_eq!(ready("age>=30"), r#"{"age":{"$gte":30}}"#);
        assert_eq!(ready("age<=30"), r#"{"age":{"$lte":30}}"#);
        assert_eq!(ready("status!=done"), r#"{"status":{"$ne":"done"}}"#);
    }

    #[test]
    fn exists_contains_and_object_ids() {
        assert_eq!(ready("email:*"), r#"{"email":{"$exists":true}}"#);
        assert_eq!(
            ready("name:~ada"),
            r#"{"name":{"$regex":"ada","$options":"i"}}"#
        );
        // A contains term is matched literally, not as a pattern.
        assert_eq!(
            ready("host:~a.b"),
            r#"{"host":{"$regex":"a\\.b","$options":"i"}}"#
        );
        assert_eq!(
            ready("_id:507f1f77bcf86cd799439011"),
            r#"{"_id":{"$oid":"507f1f77bcf86cd799439011"}}"#
        );
        // The same hex in an ordinary field is a string: a token, not an id.
        assert_eq!(
            ready("token:507f1f77bcf86cd799439011"),
            r#"{"token":"507f1f77bcf86cd799439011"}"#
        );
    }

    #[test]
    fn relative_dates_are_ranges_from_now() {
        assert_eq!(
            ready("created:last7d"),
            format!(
                r#"{{"created":{{"$gte":{{"$date":{{"$numberLong":"{}"}}}}}}}}"#,
                NOW - 7 * DAY_MS
            )
        );
        assert_eq!(
            ready("seen:last24h"),
            format!(
                r#"{{"seen":{{"$gte":{{"$date":{{"$numberLong":"{}"}}}}}}}}"#,
                NOW - 24 * 3_600_000
            )
        );
        // `today` is the start of the current UTC day, not 24h back.
        assert_eq!(
            ready("at:today"),
            format!(
                r#"{{"at":{{"$gte":{{"$date":{{"$numberLong":"{}"}}}}}}}}"#,
                NOW - NOW.rem_euclid(DAY_MS)
            )
        );
    }

    #[test]
    fn several_terms_conjoin_and_a_repeat_becomes_an_and() {
        assert_eq!(
            ready("status:active age>30"),
            r#"{"status":"active","age":{"$gt":30}}"#
        );
        // The same field twice cannot share one JSON key, so both survive under
        // `$and` instead of one being silently dropped.
        assert_eq!(
            ready("age>18 age<65"),
            r#"{"$and":[{"age":{"$gt":18}},{"age":{"$lt":65}}]}"#
        );
    }

    #[test]
    fn unfinished_input_is_quiet_and_broken_input_is_not() {
        assert_eq!(compile_fast_filter("", NOW), FastFilter::Empty);
        assert_eq!(compile_fast_filter("   ", NOW), FastFilter::Empty);
        // Mid-term: still typing, so no complaint.
        assert_eq!(compile_fast_filter("status", NOW), FastFilter::Incomplete);
        assert_eq!(compile_fast_filter("status:", NOW), FastFilter::Incomplete);
        assert_eq!(compile_fast_filter("age>", NOW), FastFilter::Incomplete);
        assert_eq!(compile_fast_filter("name:~", NOW), FastFilter::Incomplete);
        // A finished term followed by an unfinished one is still unfinished.
        assert_eq!(
            compile_fast_filter("status:active age>", NOW),
            FastFilter::Incomplete
        );
        // An operator with nothing in front of it is a real mistake.
        assert!(matches!(
            compile_fast_filter(":active", NOW),
            FastFilter::Invalid(_)
        ));
    }
}
