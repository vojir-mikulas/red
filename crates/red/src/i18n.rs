//! The UI string catalog: which locale is active, and lookup for keys that are
//! only known at runtime.
//!
//! Catalogs are Fluent, at `assets/i18n/<domain>/<locale>.ftl`, embedded through
//! [`crate::assets::Assets`]. The locale is the file's name and the folder is the
//! UI area; every file for one locale merges into that locale's bundle, so a
//! domain keeps any single catalog small enough to review and lets two
//! translators work without touching the same file.
//!
//! **Why Fluent and not a key-value catalog.** Czech selects between three plural
//! forms, Polish and Russian likewise. Fluent carries the CLDR rules, so the
//! translator writes the forms and no Rust code decides grammar per language:
//!
//! ```ftl
//! notify-rows_affected = { $n ->
//!     [one] { $n } řádek
//!     [few] { $n } řádky
//!    *[other] { $n } řádků
//! }
//! ```
//!
//! **Key shape.** Call sites use dotted keys (`settings.data.page_size.label`).
//! A Fluent id cannot contain a dot, which there means "attribute", so
//! [`fluent_id`] maps a dotted key onto a message id plus an optional attribute:
//! a trailing `label` / `help` / `subtitle` becomes an attribute, which groups a
//! row's text under one message for the translator, and everything else flattens
//! to a hyphenated id.
//!
//! **Degrading.** A key missing from the active locale falls back to `en`, then
//! to the English source the caller holds ([`tr_or`]), then to the key itself.
//! The UI drops to English, then to something diagnosable, never to blank.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use gpui::SharedString;
use unic_langid::LanguageIdentifier;

/// The locale a catalog is guaranteed to exist for, and the fallback every other
/// locale resolves against.
pub(crate) const DEFAULT: &str = "en";

/// The pseudolocale: the English catalog with every letter accented and each
/// string bracketed. Not a language, a coverage test. Any string still rendering
/// as plain ASCII under it was never extracted, which is the one way to audit
/// coverage that does not depend on reading every call site.
pub(crate) const PSEUDO: &str = "en-XA";

/// The suffixes that become Fluent attributes rather than part of the message id.
/// Chosen so a settings row's label and help sit under one message, which is the
/// context a translator needs and the reason to use attributes at all.
const ATTRIBUTES: &[&str] = &["label", "help", "subtitle"];

type Bundle = FluentBundle<FluentResource>;

/// Every locale's bundle, parsed once from the embedded catalogs.
fn bundles() -> &'static HashMap<String, Bundle> {
    static BUNDLES: OnceLock<HashMap<String, Bundle>> = OnceLock::new();
    BUNDLES.get_or_init(load_bundles)
}

/// The active locale. Written by [`apply`] on startup and on every settings
/// reload, read by every lookup.
fn active() -> &'static RwLock<String> {
    static ACTIVE: OnceLock<RwLock<String>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(DEFAULT.to_string()))
}

/// Parses every embedded `.ftl` into one bundle per locale.
///
/// A malformed catalog is skipped with a warning rather than failing the launch:
/// a syntax error in one community-contributed translation should cost that
/// locale, not the app. English is generated, so if it were ever broken the
/// extractor's own check would have failed first.
fn load_bundles() -> HashMap<String, Bundle> {
    let mut sources: HashMap<String, Vec<String>> = HashMap::new();

    for path in crate::assets::Assets::iter() {
        let Some(locale) = locale_of(&path) else {
            continue;
        };
        let Some(file) = crate::assets::Assets::get(&path) else {
            continue;
        };
        match String::from_utf8(file.data.to_vec()) {
            Ok(text) => sources.entry(locale).or_default().push(text),
            Err(_) => tracing::warn!(%path, "locale catalog is not valid UTF-8, skipping"),
        }
    }

    let mut bundles = HashMap::new();
    for (locale, texts) in sources {
        let Ok(lang) = locale.parse::<LanguageIdentifier>() else {
            tracing::warn!(%locale, "not a language tag, skipping catalog");
            continue;
        };
        let mut bundle = FluentBundle::new_concurrent(vec![lang]);
        // Fluent isolates placeables with Unicode FSI/PDI by default, which is
        // right for mixed-direction text and wrong here: RED ships no RTL locale,
        // and the marks are invisible characters that break string comparison.
        bundle.set_use_isolating(false);

        for text in texts {
            match FluentResource::try_new(text) {
                Ok(res) => {
                    if let Err(errs) = bundle.add_resource(res) {
                        tracing::warn!(%locale, ?errs, "duplicate messages in catalog");
                    }
                }
                Err((res, errs)) => {
                    tracing::warn!(%locale, ?errs, "syntax errors in catalog, using what parsed");
                    let _ = bundle.add_resource(res);
                }
            }
        }
        bundles.insert(locale, bundle);
    }
    bundles
}

/// The locale a catalog path belongs to: `i18n/settings/en-XA.ftl` is `en-XA`.
fn locale_of(path: &str) -> Option<String> {
    let rest = path.strip_prefix("i18n/")?;
    let (_domain, file) = rest.rsplit_once('/')?;
    Some(file.strip_suffix(".ftl")?.to_string())
}

/// A dotted call-site key as a Fluent message id plus an optional attribute.
///
/// `settings.data.page_size.label` becomes `("settings-data-page_size", Some("label"))`;
/// `palette.cmd_run` becomes `("palette-cmd_run", None)`.
fn fluent_id(key: &str) -> (String, Option<&str>) {
    match key.rsplit_once('.') {
        Some((head, tail)) if ATTRIBUTES.contains(&tail) => (head.replace('.', "-"), Some(tail)),
        _ => (key.replace('.', "-"), None),
    }
}

/// Formats a key out of one bundle, or `None` if that bundle does not define it.
fn format_from(bundle: &Bundle, key: &str, args: Option<&FluentArgs>) -> Option<String> {
    let (id, attr) = fluent_id(key);
    let message = bundle.get_message(&id)?;
    let pattern = match attr {
        Some(attr) => message.get_attribute(attr)?.value(),
        None => message.value()?,
    };

    let mut errors = Vec::new();
    let text = bundle.format_pattern(pattern, args, &mut errors);
    if !errors.is_empty() {
        // A message that exists but fails to format is a **miss**, not a hit.
        // Fluent renders the failure as the placeable's own name, so returning
        // `Some` here put a literal `{$count}` on screen — the one degradation the
        // module's own charter says cannot happen. `None` drops to the next bundle
        // (English), and from there to the callsite English, which is the whole
        // point of the chain.
        warn_once(key, &errors);
        return None;
    }
    Some(text.into_owned())
}

/// Log a formatting failure at most once per key.
///
/// The unguarded `warn!` sat on the render path, so one bad translation line
/// warn-spammed at frame rate — `kv.monitor_streaming` re-renders per MONITOR
/// line. The first report is the useful one; the next ten thousand are noise that
/// buries it.
fn warn_once(key: &str, errors: &[fluent_bundle::FluentError]) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut seen) = seen.lock() else {
        return;
    };
    // Bounded: a poisoned catalog cannot grow this without bound, and the cap is
    // far above the number of distinct keys any build has.
    if seen.len() < 4096 && seen.insert(key.to_string()) {
        tracing::warn!(%key, ?errors, "error formatting catalog string; falling back");
    }
}

/// Resolves a key against the active locale, then `en`.
fn format(key: &str, args: Option<&FluentArgs>) -> Option<String> {
    let locale = active().read().ok()?.clone();
    let bundles = bundles();

    bundles
        .get(&locale)
        .and_then(|b| format_from(b, key, args))
        .or_else(|| {
            (locale != DEFAULT)
                .then(|| bundles.get(DEFAULT).and_then(|b| format_from(b, key, args)))
                .flatten()
        })
}

/// Resolves a key *without* the English fallback, yielding the key itself when
/// the catalog does not define it.
///
/// Only the drift tests want this. Rendering code goes through [`tr_or`], which
/// hides a missing key behind the English source, and that is precisely what a
/// test checking for a missing key must not do.
#[cfg(test)]
pub(crate) fn lookup(key: &str) -> SharedString {
    SharedString::from(format(key, None).unwrap_or_else(|| key.to_string()))
}

/// Like [`lookup`], but falls back to the English source text a caller holds.
///
/// Callers that carry their own English (the settings registry does, as
/// `en_label` / `en_help`) render that instead of a raw key, so a string added
/// without regenerating the catalog still reads as English rather than as
/// `settings.data.page_size.label`.
pub(crate) fn tr_or(key: &str, english: &'static str) -> SharedString {
    match format(key, None) {
        Some(text) => SharedString::from(text),
        None => SharedString::from(english),
    }
}

/// [`tr_or`] with Fluent arguments, for a string that interpolates data.
///
/// The English fallback is returned verbatim when the key is missing, so it is
/// written with its placeables already filled by the caller. See the `tr!` macro,
/// which is what call sites use.
pub(crate) fn tr_args_or(key: &str, english: String, args: FluentArgs<'_>) -> SharedString {
    match format(key, Some(&args)) {
        Some(text) => SharedString::from(text),
        None => SharedString::from(english),
    }
}

/// A user-facing string at a call site: its catalog key and its English source,
/// together.
///
/// The two extraction shapes need the same guarantee from opposite directions.
/// A data table (the settings registry, the keymap defaults) already carries an
/// identity per row, so its keys derive from that and its English is a field.
/// Everything else is a literal sitting in a `render` somewhere with no identity
/// at all, so the key is written next to the text:
///
/// ```ignore
/// MenuItem::action(tr!("menu.app.about", "About RED"), About)
/// tr!("notify.exported", "Exported {rows} rows to {path}", rows = n, path = p)
/// ```
///
/// The argument form takes the English as a `format!` template so the fallback is
/// a real sentence rather than one with `{rows}` still in it, while the catalog
/// entry uses Fluent's `{ $rows }`. Keeping the English *at the call site* is what
/// makes the code readable after extraction, and `scripts/i18n-extract.py` scans
/// for this macro, so the pair cannot drift apart.
macro_rules! tr {
    ($key:literal, $english:literal) => {
        $crate::i18n::tr_or($key, $english)
    };
    ($key:literal, $english:literal, $($name:ident = $value:expr),+ $(,)?) => {{
        // Bind once, by reference: each value is used twice (the English
        // fallback, then the Fluent arguments), and evaluating `$value` twice
        // would run any expression twice as well.
        $(let $name = &$value;)+
        let fallback = format!($english, $($name = $name),+);
        let mut args = ::fluent_bundle::FluentArgs::new();
        $(args.set(stringify!($name), $crate::i18n::ToArg::to_arg($name));)+
        $crate::i18n::tr_args_or($key, fallback, args)
    }};
}
pub(crate) use tr;

/// Borrows a call-site value as a Fluent argument.
///
/// By reference on purpose: `tr!` uses each value twice (the English fallback,
/// then the Fluent arguments), and taking ownership would make a `String`
/// argument unusable afterwards at the call site, which is a poor trade for a
/// notification message.
///
/// Numbers arrive as numbers, never as strings: Fluent selects a plural form by
/// running CLDR rules over the value, so a stringified count silently always
/// picks `*[other]`. Correct in English, wrong in Czech, and invisible in both
/// until someone reads the output in the other one.
pub(crate) trait ToArg {
    fn to_arg(&self) -> fluent_bundle::FluentValue<'_>;
}

impl ToArg for String {
    fn to_arg(&self) -> fluent_bundle::FluentValue<'_> {
        fluent_bundle::FluentValue::from(self.as_str())
    }
}

impl ToArg for str {
    fn to_arg(&self) -> fluent_bundle::FluentValue<'_> {
        fluent_bundle::FluentValue::from(self)
    }
}

/// Any depth of reference. `tr!` binds each argument by reference, so a caller
/// passing an existing `&String` arrives here as `&&String`; without this the
/// call site would have to know how many `&` the macro added.
impl<T: ToArg + ?Sized> ToArg for &T {
    fn to_arg(&self) -> fluent_bundle::FluentValue<'_> {
        (**self).to_arg()
    }
}

impl ToArg for gpui::SharedString {
    fn to_arg(&self) -> fluent_bundle::FluentValue<'_> {
        fluent_bundle::FluentValue::from(self.as_ref())
    }
}

macro_rules! numeric_arg {
    ($($t:ty),+) => {$(
        impl ToArg for $t {
            fn to_arg(&self) -> fluent_bundle::FluentValue<'_> {
                fluent_bundle::FluentValue::from(*self)
            }
        }
    )+};
}
numeric_arg!(i32, i64, u32, u64, f64);

impl ToArg for usize {
    fn to_arg(&self) -> fluent_bundle::FluentValue<'_> {
        // Through `u64` because Fluent has no `usize` conversion; the cast is
        // lossless on every target RED builds for.
        fluent_bundle::FluentValue::from(*self as u64)
    }
}

/// Every locale compiled into this build, for the Language setting to offer:
/// `en` first as the source language, then real languages sorted, then the
/// pseudolocale, which is a QA tool and does not belong among them.
pub(crate) fn available() -> Vec<String> {
    let mut locales: Vec<String> = bundles().keys().cloned().collect();
    locales.sort_unstable_by_key(|l| match l.as_str() {
        DEFAULT => (0, String::new()),
        PSEUDO => (2, String::new()),
        other => (1, other.to_string()),
    });
    locales
}

/// The locale the `locale` setting actually selects: `"system"` resolved against
/// the environment, anything else taken literally, and either way narrowed to a
/// locale this build has a catalog for.
///
/// An unknown locale degrades to English rather than erroring. The value arrives
/// from a hand-edited `settings.toml`, and a typo there should cost the user
/// their language, not their app.
pub(crate) fn resolve(setting: &str) -> String {
    let requested = if setting == "system" {
        detect_system()
    } else {
        setting.to_string()
    };

    available()
        .into_iter()
        .find(|l| *l == requested)
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// Applies the `locale` setting to the process-wide active locale.
pub(crate) fn apply(setting: &str) {
    let resolved = resolve(setting);
    if let Ok(mut guard) = active().write() {
        *guard = resolved;
    }
}

/// The OS UI language, as a best-effort read of the POSIX locale environment.
///
/// Deliberately env-only for now: every shipped catalog is English, so the answer
/// cannot yet be wrong in a way a user would notice. It will need a real platform
/// query before a second language ships, because a macOS `.app` launched from
/// Finder inherits no `LANG` at all and would silently read as English.
fn detect_system() -> String {
    // `filter(|v| !v.is_empty())`: POSIX says a variable that is *set but empty*
    // does not select a locale, so the search falls through to the next one.
    // Stopping at an empty `LC_ALL` — which plenty of shells export — read as
    // "no locale" and forced English.
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();

    // `cs_CZ.UTF-8` -> `cs`. The territory and codeset are dropped because
    // catalogs are keyed by language alone until a locale needs to differ by
    // region (pt-BR vs pt-PT), which none does yet.
    let lang = raw.split(['.', '@']).next().unwrap_or("").replace('_', "-");

    match lang.split('-').next().unwrap_or("") {
        "" | "C" | "POSIX" => DEFAULT.to_string(),
        primary => primary.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves against a named locale regardless of what other tests have set
    /// the process-wide one to.
    fn in_locale(locale: &str, key: &str) -> Option<String> {
        bundles()
            .get(locale)
            .and_then(|b| format_from(b, key, None))
    }

    #[test]
    fn dotted_keys_map_onto_fluent_ids() {
        assert_eq!(
            fluent_id("settings.data.page_size.label"),
            ("settings-data-page_size".to_string(), Some("label"))
        );
        assert_eq!(
            fluent_id("palette.cmd_run"),
            ("palette-cmd_run".to_string(), None)
        );
        // `system` is not an attribute name, so the whole key is the id.
        assert_eq!(
            fluent_id("settings.appearance.locale.seg.system"),
            ("settings-appearance-locale-seg-system".to_string(), None)
        );
    }

    #[test]
    fn locale_is_read_from_the_file_name_not_the_folder() {
        assert_eq!(
            locale_of("i18n/settings/en-XA.ftl").as_deref(),
            Some("en-XA")
        );
        assert_eq!(locale_of("i18n/keymap/en.ftl").as_deref(), Some("en"));
        assert_eq!(locale_of("fonts/IBMPlexSans-Regular.ttf"), None);
    }

    #[test]
    fn en_is_always_available_and_first() {
        assert_eq!(available().first().map(String::as_str), Some(DEFAULT));
    }

    #[test]
    fn pseudolocale_ships_for_coverage_audits_but_sorts_last() {
        assert!(available().iter().any(|l| l == PSEUDO));
        assert_eq!(available().last().map(String::as_str), Some(PSEUDO));
    }

    #[test]
    fn unknown_locale_falls_back_to_english() {
        assert_eq!(resolve("does-not-exist"), DEFAULT);
    }

    #[test]
    fn a_shipped_locale_is_selected_verbatim() {
        assert_eq!(resolve(PSEUDO), PSEUDO);
    }

    #[test]
    fn registry_keys_resolve_through_the_catalog() {
        assert_eq!(
            in_locale(DEFAULT, "settings.appearance.reduce_motion.label").as_deref(),
            Some("Reduce motion")
        );
    }

    #[test]
    fn a_missing_key_has_no_value_so_callers_can_fall_back() {
        assert_eq!(in_locale(DEFAULT, "settings.nope.label"), None);
        assert_eq!(lookup("settings.nope.label"), "settings.nope.label");
        assert_eq!(
            tr_or("settings.nope.label", "English source"),
            "English source"
        );
    }

    #[test]
    fn pseudolocale_covers_the_same_keys_as_english() {
        // A key present in `en` but absent from `en-XA` would render as English
        // under the pseudolocale: the exact "never extracted" signal the audit
        // relies on, produced by a catalog gap rather than a real miss.
        let pseudo = in_locale(PSEUDO, "settings.appearance.reduce_motion.label")
            .expect("pseudolocale is missing a key that en has");
        assert!(
            pseudo.starts_with('[') && pseudo.ends_with(']'),
            "pseudolocale string was not transformed: {pseudo}"
        );
    }

    /// The reason RED is on Fluent rather than a key-value catalog.
    ///
    /// English keeps the house style of `row(s)`, which is generated from the
    /// call site and stays one string. A translator into a language with real
    /// plural categories writes the forms in their own file, and Fluent picks
    /// between them with the CLDR rules for that language. No Rust code decides
    /// grammar, and English did not have to get worse for Czech to get right.
    #[test]
    fn a_translation_can_add_plural_forms_english_does_not_have() {
        let cs = FluentResource::try_new(
            r#"
notify-exported_rows = { $rows ->
    [one] Exportován { $rows } řádek
    [few] Exportovány { $rows } řádky
   *[other] Exportováno { $rows } řádků
}
"#
            .to_string(),
        )
        .expect("czech test catalog parses");

        let mut bundle = FluentBundle::new_concurrent(vec!["cs".parse().unwrap()]);
        bundle.set_use_isolating(false);
        bundle.add_resource(cs).unwrap();

        let render = |n: i64| {
            let mut args = FluentArgs::new();
            args.set("rows", n);
            format_from(&bundle, "notify.exported_rows", Some(&args)).unwrap()
        };

        assert_eq!(render(1), "Exportován 1 řádek");
        assert_eq!(render(3), "Exportovány 3 řádky");
        assert_eq!(render(7), "Exportováno 7 řádků");
    }

    /// A number must reach Fluent as a number. Passed as a string it formats the
    /// same, but plural selection runs CLDR over the *value*, so a stringified
    /// count silently always picks `*[other]`: right in English, wrong in Czech,
    /// and invisible in both until someone reads the output in the other one.
    #[test]
    fn numeric_arguments_stay_numeric_for_plural_selection() {
        let res = FluentResource::try_new(
            "n-forms = { $n ->\n    [one] singular\n   *[other] plural\n}".to_string(),
        )
        .unwrap();
        let mut bundle = FluentBundle::new_concurrent(vec!["en".parse().unwrap()]);
        bundle.set_use_isolating(false);
        bundle.add_resource(res).unwrap();

        let mut numeric = FluentArgs::new();
        numeric.set("n", 1i64.to_arg());
        assert_eq!(
            format_from(&bundle, "n-forms", Some(&numeric)).unwrap(),
            "singular"
        );

        let mut stringy = FluentArgs::new();
        stringy.set("n", "1".to_arg());
        assert_eq!(
            format_from(&bundle, "n-forms", Some(&stringy)).unwrap(),
            "plural",
            "a stringified count stopped selecting `other`; the arg() guard may \
             no longer be needed, but check before relying on it"
        );
    }

    /// Literal braces survive the round trip through the catalog.
    ///
    /// Fluent has no escape character, so a `{` in the source text has to ride as
    /// a string placeable (`{"{"}`). Getting that wrong is quiet: the string still
    /// renders, just with the escaping machinery showing, and the placeholder
    /// examples in the Mongo filter box are exactly where a user would see it.
    #[test]
    fn literal_braces_round_trip() {
        assert_eq!(
            in_locale(DEFAULT, "doc.filter_e_g_status_active").as_deref(),
            Some(r#"filter, e.g. { "status": "active" }"#)
        );
    }

    /// Rust unicode escapes reach the catalog as the characters they denote, not
    /// as their source text. `\u{2318}` left undecoded would render literally, and
    /// its `{2318}` could be read as a placeable.
    #[test]
    fn unicode_escapes_are_decoded_before_extraction() {
        let text = in_locale(DEFAULT, "doc.aggregation_pipeline_e_g_group_runs")
            .expect("aggregation placeholder");
        assert!(text.contains('⌘'), "expected a decoded glyph in: {text}");
        assert!(
            !text.contains("\\u"),
            "escape survived into the catalog: {text}"
        );
    }

    #[test]
    fn placeables_are_not_wrapped_in_isolation_marks() {
        // With `use_isolating` left on, Fluent brackets every placeable in U+2068
        // / U+2069. They are invisible, so this only ever shows up as a string
        // comparison failing for no visible reason.
        let bundle = bundles().get(DEFAULT).expect("en bundle");
        let res = FluentResource::try_new("probe-arg = got { $n }".to_string()).unwrap();
        let mut probe = FluentBundle::new_concurrent(bundle.locales.clone());
        probe.set_use_isolating(false);
        probe.add_resource(res).unwrap();

        let mut args = FluentArgs::new();
        args.set("n", 5);
        let out = format_from(&probe, "probe-arg", Some(&args)).unwrap();
        assert_eq!(out, "got 5");
    }
}
