//! The settings registry: every simple setting described as data, once.
//!
//! The settings panel used to be ~2200 lines of hand-assembled rows, each one
//! restating its own presets, its own selection lookup, and its own setter. That
//! made a new setting expensive enough that engine sections never got added: the
//! MongoDB browser shipped with no settings at all, and Redis had one row filed
//! under "Behavior".
//!
//! Here a setting is a [`SettingDef`] — where it lives, what it's called, what it
//! does, how it's edited, and how to read/write it on [`Settings`]. The panel
//! renders from this list, so a new row is one entry rather than a page edit, and
//! three things fall out for free:
//!
//! - **Search** across every setting, whatever page it's filed under, which is the
//!   real fix for "I can never find that option".
//! - **Modified / Reset**, by comparing against [`Settings::default`].
//! - A **test** that every field of every section is registered
//!   (`tests::every_section_is_covered`), so a setting can't be added to the
//!   file format and stay invisible in the UI.
//!
//! Complex controls (the theme and font pickers, the AI account list, the keymap
//! table, the About build info) stay hand-written in `settings_ui`; they aren't
//! value edits and there'd be nothing to gain. The registry covers the long tail
//! that used to cost a page edit each.

use std::borrow::Cow;

use gpui::SharedString;

use crate::settings::{
    ConfirmThreshold, Density, DocView, KvQueryMode, MAX_DOC_COLUMNS, MAX_PREVIEW_COUNT,
    MAX_RESIDENT_KEYS, Settings,
};

/// A setting's value, in the three shapes a generic control can edit. Enums ride
/// as [`Value::Text`] using their `settings.toml` spelling, so the control, the
/// registry key, and the file all speak one vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    Int(i64),
    Text(Cow<'static, str>),
}

impl Value {
    pub(crate) fn as_bool(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    pub(crate) fn as_int(&self) -> i64 {
        match self {
            Value::Int(n) => *n,
            _ => 0,
        }
    }

    pub(crate) fn as_text(&self) -> &str {
        match self {
            Value::Text(s) => s,
            _ => "",
        }
    }
}

/// One choice in a [`Control::Segments`] row.
pub(crate) struct Segment {
    /// The English source text. Rendered through
    /// [`SettingDef::segment_label`], which needs the owning row to build the
    /// catalog key, so a preset can be translated in the context it appears in
    /// ("Normal" spacing and "Normal" density are not one word in every
    /// language).
    pub en_label: &'static str,
    pub value: Value,
}

/// Shorthand for a segment, so the table below stays readable.
const fn seg(en_label: &'static str, value: Value) -> Segment {
    Segment { en_label, value }
}

const fn int(n: i64) -> Value {
    Value::Int(n)
}

/// The wire value for a "no ceiling" preset, where the setting itself stores
/// [`usize::MAX`]. Casting `usize::MAX` to `i64` gives `-1`, which any sane
/// `set` clamps to `0` — turning "no limit" into "nothing", the exact inversion
/// of what the label promises. Going through [`to_usize`] / [`from_usize`]
/// instead keeps the two ends honest, and the registry's round-trip test proves
/// it (it is what caught this).
const UNCAPPED: i64 = i64::MAX;

/// A `usize` setting as a registry value, mapping the uncapped sentinel across.
fn from_usize(n: usize) -> i64 {
    if n == usize::MAX {
        UNCAPPED
    } else {
        n.min(i64::MAX as usize) as i64
    }
}

/// The inverse of [`from_usize`]: a negative (only reachable from a corrupt
/// value) floors at `0`.
fn to_usize(n: i64) -> usize {
    if n == UNCAPPED {
        usize::MAX
    } else {
        n.max(0) as usize
    }
}

const fn text(s: &'static str) -> Value {
    Value::Text(Cow::Borrowed(s))
}

/// How a setting is edited.
pub(crate) enum Control {
    /// An on/off switch.
    Toggle,
    /// A row of presets. A value that matches none of them (only reachable by
    /// hand-editing the file) selects nothing rather than snapping, so the panel
    /// never silently overwrites a deliberate custom value.
    Segments(&'static [Segment]),
}

/// Which seams a setting actually affects. Rendered as a badge on the shared
/// data-view page, where rows legitimately differ: the cross-seam knobs sit
/// beside a few that only the SQL grid can honour, and pretending otherwise
/// would be the same dishonesty this overhaul is fixing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applies {
    All,
    Sql,
    Kv,
    Doc,
}

impl Applies {
    /// The badge text, or `None` for a setting that applies everywhere.
    pub(crate) fn badge(self) -> Option<&'static str> {
        match self {
            Applies::All => None,
            Applies::Sql => Some("SQL"),
            Applies::Kv => Some("Redis"),
            Applies::Doc => Some("MongoDB"),
        }
    }
}

/// The nav pages, in nav order, grouped into the four bands the sidebar renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Appearance,
    Editor,
    Keymap,
    Behavior,
    Data,
    Sql,
    Kv,
    Doc,
    Safety,
    Ai,
    About,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 11] = [
        SettingsTab::Appearance,
        SettingsTab::Editor,
        SettingsTab::Keymap,
        SettingsTab::Behavior,
        SettingsTab::Data,
        SettingsTab::Sql,
        SettingsTab::Kv,
        SettingsTab::Doc,
        SettingsTab::Safety,
        SettingsTab::Ai,
        SettingsTab::About,
    ];

    /// This page's name in the active locale.
    ///
    /// Keyed on the variant name, not on [`label`](Self::label): the catalog key
    /// has to survive a copy edit, and "Grids & results" is exactly the kind of
    /// display text that gets reworded. `scripts/i18n-extract.py` reads the same
    /// variant names out of the match below.
    pub(crate) fn title(self) -> SharedString {
        crate::i18n::tr_or(
            &format!("settings.tab.{}", slug(&format!("{self:?}"))),
            self.label(),
        )
    }

    /// The English page name. Identity as well as text: element ids are built
    /// from it, so it must not move when the locale does. Render
    /// [`title`](Self::title).
    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Editor => "Editor",
            SettingsTab::Keymap => "Keymap",
            SettingsTab::Behavior => "Behavior",
            SettingsTab::Data => "Grids & results",
            SettingsTab::Sql => "SQL",
            SettingsTab::Kv => "Key-value",
            SettingsTab::Doc => "Documents",
            SettingsTab::Safety => "Safety",
            SettingsTab::Ai => "AI agent",
            SettingsTab::About => "About",
        }
    }

    /// The nav band this page sits under. Eleven flat rows is a list to scan;
    /// four labelled bands is a map.
    pub(crate) fn band(self) -> &'static str {
        match self {
            SettingsTab::Appearance
            | SettingsTab::Editor
            | SettingsTab::Keymap
            | SettingsTab::Behavior => "App",
            SettingsTab::Data | SettingsTab::Sql | SettingsTab::Kv | SettingsTab::Doc => "Data",
            SettingsTab::Safety | SettingsTab::Ai => "Policy",
            SettingsTab::About => "System",
        }
    }

    /// A line under the page title saying who the page is for, so an engine page
    /// is honest about being global defaults rather than about the connection
    /// currently open.
    /// This page's one-line subtitle in the active locale, if it has one.
    pub(crate) fn subtitle_text(self) -> Option<SharedString> {
        self.subtitle().map(|english| {
            crate::i18n::tr_or(
                &format!("settings.tab.{}.subtitle", slug(&format!("{self:?}"))),
                english,
            )
        })
    }

    /// The English subtitle source. See [`en_label`](SettingDef::en_label);
    /// render [`subtitle_text`](Self::subtitle_text).
    pub(crate) fn subtitle(self) -> Option<&'static str> {
        match self {
            SettingsTab::Data => {
                Some("Defaults for every grid: SQL results, Redis keys, MongoDB documents.")
            }
            SettingsTab::Sql => {
                Some("Applies to SQL connections (PostgreSQL, MySQL, SQLite, ClickHouse).")
            }
            SettingsTab::Kv => {
                Some("Applies to key-value connections (Redis). New tabs start from these.")
            }
            SettingsTab::Doc => {
                Some("Applies to document connections (MongoDB). New tabs start from these.")
            }
            SettingsTab::Safety => {
                Some("One scale for every engine: SQL statements, Redis keys, MongoDB documents.")
            }
            _ => None,
        }
    }
}

/// One registered setting.
pub(crate) struct SettingDef {
    /// The dotted path in `settings.toml` (`data.page_size`). Shown on the row so
    /// the panel teaches the file rather than competing with it, and searched.
    pub key: &'static str,
    pub tab: SettingsTab,
    /// The header this row sits under within its page, in English. Identity as
    /// well as text: [`for_group`] groups on it. Render [`group_label`] instead.
    ///
    /// [`group_label`]: SettingDef::group_label
    pub group: &'static str,
    /// The English source text, which is what `scripts/i18n-extract.py` lifts into
    /// `assets/i18n/en.toml`. Named for the language it is in because rendering it
    /// directly would pin the UI to English; [`label`] is what the panel draws.
    ///
    /// [`label`]: SettingDef::label
    pub en_label: &'static str,
    /// The English source description. See [`en_label`](Self::en_label); render
    /// [`help`](SettingDef::help).
    pub en_help: &'static str,
    pub applies: Applies,
    pub control: Control,
    pub get: fn(&Settings) -> Value,
    pub set: fn(&mut Settings, &Value),
    /// An "at your own risk" note for the *current* value, shown as a red ⚠ with
    /// a danger tooltip. Computed from the value, so the warning appears only for
    /// the choices that earn it.
    pub warn: Option<fn(&Settings) -> Option<&'static str>>,
}

impl SettingDef {
    /// Whether this setting differs from the shipped default.
    pub(crate) fn is_modified(&self, settings: &Settings) -> bool {
        (self.get)(settings) != (self.get)(&Settings::default())
    }

    /// The shipped default, for the per-row Reset.
    pub(crate) fn default_value(&self) -> Value {
        (self.get)(&Settings::default())
    }

    /// This row's name in the active locale.
    ///
    /// The catalog key is derived from [`key`](Self::key) rather than stored,
    /// which is what keeps a translated row the same size as an untranslated one:
    /// the dotted `settings.toml` path a row already carries *is* its namespace.
    pub(crate) fn label(&self) -> SharedString {
        crate::i18n::tr_or(&format!("settings.{}.label", self.key), self.en_label)
    }

    /// This row's description in the active locale.
    pub(crate) fn help(&self) -> SharedString {
        crate::i18n::tr_or(&format!("settings.{}.help", self.key), self.en_help)
    }

    /// This row's group header in the active locale, empty for a row that sits
    /// under no header (`group: ""`, a layout sentinel rather than text).
    pub(crate) fn group_label(&self) -> SharedString {
        if self.group.is_empty() {
            return SharedString::default();
        }
        crate::i18n::tr_or(&format!("settings.group.{}", slug(self.group)), self.group)
    }

    /// One of this row's presets in the active locale.
    pub(crate) fn segment_label(&self, segment: &Segment) -> SharedString {
        crate::i18n::tr_or(
            &format!("settings.{}.seg.{}", self.key, slug(segment.en_label)),
            segment.en_label,
        )
    }

    /// Whether this row matches a lowercased search query, over everything a user
    /// might plausibly type: the label, the description, and the file key.
    ///
    /// Matches the *translated* text, so search works in the language on screen.
    /// The key stays searchable in every locale because it is the file syntax,
    /// not prose.
    pub(crate) fn matches(&self, query: &str) -> bool {
        self.label().to_lowercase().contains(query)
            || self.help().to_lowercase().contains(query)
            || self.key.contains(query)
    }
}

/// The catalog-key form of a display string: lowercase, non-alphanumerics
/// collapsed to `_`. Mirrors `slug()` in `scripts/i18n-extract.py`; the two must
/// agree or a generated key will not be the key the UI looks up, which
/// `tests::every_registry_string_is_in_the_english_catalog` catches.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Every registered setting, in page then group order.
pub(crate) fn defs() -> &'static [SettingDef] {
    DEFS
}

/// The registered settings for one page, in order.
pub(crate) fn for_tab(tab: SettingsTab) -> impl Iterator<Item = &'static SettingDef> {
    DEFS.iter().filter(move |d| d.tab == tab)
}

/// The registered settings on one page under one group header, in order.
pub(crate) fn for_group(
    tab: SettingsTab,
    group: &'static str,
) -> impl Iterator<Item = &'static SettingDef> {
    DEFS.iter()
        .filter(move |d| d.tab == tab && d.group == group)
}

// --- the table ---------------------------------------------------------------

/// The Language row's choices.
///
/// Language names stay in their own language (an endonym: "Deutsch", not
/// "German"), because someone who cannot read the current UI language still has
/// to find their own in this list. Only "System" is translated. The pseudolocale
/// is a coverage audit rather than a language, so it is offered in dev builds
/// only; a user can still reach it by hand-editing `settings.toml`.
#[cfg(debug_assertions)]
static LOCALE_SEGMENTS: &[Segment] = &[
    seg("System", text("system")),
    seg("English", text("en")),
    seg("Pseudo", text(crate::i18n::PSEUDO)),
];
#[cfg(not(debug_assertions))]
static LOCALE_SEGMENTS: &[Segment] = &[seg("System", text("system")), seg("English", text("en"))];

static DEFS: &[SettingDef] = &[
    // --- appearance ---
    SettingDef {
        key: "appearance.locale",
        tab: SettingsTab::Appearance,
        group: "Language",
        en_label: "Language",
        en_help: "The interface language. \"System\" follows the operating system, \
               falling back to English when that language has no translation.",
        applies: Applies::All,
        control: Control::Segments(LOCALE_SEGMENTS),
        get: |s| Value::Text(Cow::Owned(s.appearance.locale.clone())),
        set: |s, v| s.appearance.locale = v.as_text().to_string(),
        warn: None,
    },
    SettingDef {
        key: "appearance.reduce_motion",
        tab: SettingsTab::Appearance,
        group: "Motion",
        en_label: "Reduce motion",
        en_help: "Suppress non-essential animation, for motion sensitivity.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.appearance.reduce_motion),
        set: |s, v| s.appearance.reduce_motion = v.as_bool(),
        warn: None,
    },
    // --- editor ---
    SettingDef {
        key: "editor.line_height",
        tab: SettingsTab::Editor,
        group: "Layout",
        en_label: "Line height",
        en_help: "Editor line spacing, as a multiple of the font size.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("Tight", int(12)),
            seg("Normal", int(15)),
            seg("Loose", int(18)),
        ]),
        // Carried as tenths so the control can stay integer-valued; the stored
        // setting is the float the editor actually uses.
        get: |s| Value::Int((s.editor.line_height * 10.0).round() as i64),
        set: |s, v| s.editor.line_height = v.as_int() as f32 / 10.0,
        warn: None,
    },
    SettingDef {
        key: "editor.tab_width",
        tab: SettingsTab::Editor,
        group: "Layout",
        en_label: "Tab width",
        en_help: "Spaces a tab occupies in the SQL editor.",
        applies: Applies::All,
        control: Control::Segments(&[seg("2", int(2)), seg("4", int(4)), seg("8", int(8))]),
        get: |s| Value::Int(i64::from(s.editor.tab_width)),
        set: |s, v| s.editor.tab_width = v.as_int().clamp(1, 16) as u8,
        warn: None,
    },
    // --- keymap ---
    SettingDef {
        key: "keymap.vim_mode",
        tab: SettingsTab::Keymap,
        group: "Navigation",
        en_label: "Vim navigation",
        en_help: "Adds hjkl / g / G / Ctrl-d / Ctrl-u motions to the result grid and the \
               history dock, alongside the arrow keys. Applies live.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.keymap.vim_mode),
        set: |s, v| s.keymap.vim_mode = v.as_bool(),
        warn: None,
    },
    // --- behavior ---
    SettingDef {
        key: "behavior.restore_last_session",
        tab: SettingsTab::Behavior,
        group: "Startup",
        en_label: "Restore last session",
        en_help: "Reconnect to the most recently used connection on launch (credentials come \
               from the keychain). Takes effect next launch.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.behavior.restore_last_session),
        set: |s, v| s.behavior.restore_last_session = v.as_bool(),
        warn: None,
    },
    // --- data view (every grid) ---
    SettingDef {
        key: "data.density",
        tab: SettingsTab::Data,
        group: "Display",
        en_label: "Row density",
        en_help: "Vertical spacing of rows, in every grid.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("Compact", text("compact")),
            seg("Comfortable", text("comfortable")),
            seg("Spacious", text("spacious")),
        ]),
        get: |s| text(s.data.density.as_str()),
        set: |s, v| s.data.density = Density::from_str(v.as_text()),
        warn: None,
    },
    SettingDef {
        key: "data.null_display",
        tab: SettingsTab::Data,
        group: "Display",
        en_label: "Null display",
        en_help: "How a SQL NULL renders in a cell. Set any other string in the file.",
        applies: Applies::Sql,
        control: Control::Segments(&[
            seg("NULL", text("NULL")),
            seg("∅", text("∅")),
            seg("blank", text("")),
        ]),
        get: |s| Value::Text(Cow::Owned(s.data.null_display.clone())),
        set: |s, v| s.data.null_display = v.as_text().to_string(),
        warn: None,
    },
    SettingDef {
        key: "data.row_numbers",
        tab: SettingsTab::Data,
        group: "Display",
        en_label: "Row numbers",
        en_help: "Show the leading row-number gutter in SQL results.",
        applies: Applies::Sql,
        control: Control::Toggle,
        get: |s| Value::Bool(s.data.row_numbers),
        set: |s, v| s.data.row_numbers = v.as_bool(),
        warn: None,
    },
    SettingDef {
        key: "data.page_size",
        tab: SettingsTab::Data,
        group: "Performance",
        en_label: "Page size",
        en_help: "Rows fetched per page as you scroll, in every grid. Larger means fewer \
               round-trips and more resident rows.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("100", int(100)),
            seg("200", int(200)),
            seg("500", int(500)),
            seg("1000", int(1000)),
        ]),
        get: |s| Value::Int(s.data.page_size as i64),
        set: |s, v| s.data.page_size = v.as_int().max(0) as usize,
        warn: None,
    },
    SettingDef {
        key: "data.max_cell_chars",
        tab: SettingsTab::Data,
        group: "Performance",
        en_label: "Max cell size",
        en_help: "Bytes of a single cell kept resident, the fat-cell memory rail. Over-cap \
               cells are clipped for display only; export stays full.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("1K", int(1024)),
            seg("4K", int(4096)),
            seg("16K", int(16384)),
            seg("64K", int(65536)),
        ]),
        get: |s| Value::Int(s.data.max_cell_chars as i64),
        set: |s, v| s.data.max_cell_chars = v.as_int().max(0) as usize,
        warn: None,
    },
    SettingDef {
        key: "data.stats_distinct_max_rows",
        tab: SettingsTab::Data,
        group: "Performance",
        en_label: "Stats distinct limit",
        en_help: "Result size past which the column-stats bar withholds count(distinct) until \
               you click compute, so it never scans a huge table by accident.",
        applies: Applies::Sql,
        control: Control::Segments(&[
            seg("100K", int(100_000)),
            seg("1M", int(1_000_000)),
            seg("10M", int(10_000_000)),
            seg("Always", int(UNCAPPED)),
        ]),
        // "Always" is `usize::MAX` in the setting and [`UNCAPPED`] on the wire; a
        // plain `usize::MAX as i64` would be `-1` and clamp back to *never*
        // compute, the exact opposite of the label.
        get: |s| Value::Int(from_usize(s.data.stats_distinct_max_rows)),
        set: |s, v| s.data.stats_distinct_max_rows = to_usize(v.as_int()),
        warn: Some(|s| {
            (s.data.stats_distinct_max_rows > 10_000_000)
                .then_some("count(distinct) on a huge result can be a full scan.")
        }),
    },
    SettingDef {
        key: "data.copy_row_limit",
        tab: SettingsTab::Data,
        group: "Performance",
        en_label: "Copy row limit",
        en_help: "Rows a select-all or whole-column copy pulls into the clipboard. Larger \
               copies are clipped to this (with a warning) to bound memory.",
        applies: Applies::Sql,
        control: Control::Segments(&[
            seg("10K", int(10_000)),
            seg("100K", int(100_000)),
            seg("500K", int(500_000)),
            seg("1M", int(1_000_000)),
        ]),
        get: |s| Value::Int(s.data.copy_row_limit as i64),
        set: |s, v| s.data.copy_row_limit = v.as_int().max(0) as usize,
        warn: None,
    },
    // --- sql seam ---
    SettingDef {
        key: "sql.auto_limit",
        tab: SettingsTab::Sql,
        group: "Large-result safety",
        en_label: "Auto-limit",
        en_help: "Append LIMIT to a bare SELECT * so a fat table can't flood the grid.",
        applies: Applies::Sql,
        control: Control::Segments(&[
            seg("Off", int(0)),
            seg("100", int(100)),
            seg("1000", int(1000)),
            seg("10000", int(10000)),
        ]),
        get: |s| Value::Int(i64::from(s.sql.auto_limit)),
        set: |s, v| s.sql.auto_limit = v.as_int().clamp(0, i64::from(u32::MAX)) as u32,
        warn: Some(|s| {
            (s.sql.auto_limit == 0)
                .then_some("With no auto-limit, a bare SELECT * on a huge table streams all of it.")
        }),
    },
    SettingDef {
        key: "sql.statement_timeout",
        tab: SettingsTab::Sql,
        group: "Large-result safety",
        en_label: "Statement timeout",
        en_help: "Abort a query (and its page/run fetches) that runs longer than this.",
        applies: Applies::Sql,
        control: Control::Segments(&[
            seg("Off", int(0)),
            seg("10s", int(10)),
            seg("30s", int(30)),
            seg("60s", int(60)),
        ]),
        get: |s| Value::Int(i64::from(s.sql.statement_timeout)),
        set: |s, v| s.sql.statement_timeout = v.as_int().clamp(0, i64::from(u32::MAX)) as u32,
        warn: None,
    },
    SettingDef {
        key: "sql.watch_default_secs",
        tab: SettingsTab::Sql,
        group: "Watch",
        en_label: "Default watch interval",
        en_help: "Arm watch at this interval on every result that can take one. Off \
               means watch starts only when you ask for it.",
        applies: Applies::Sql,
        control: Control::Segments(&[
            seg("Off", int(0)),
            seg("5s", int(5)),
            seg("10s", int(10)),
            seg("30s", int(30)),
        ]),
        get: |s| Value::Int(s.sql.watch_default_secs as i64),
        set: |s, v| s.sql.watch_default_secs = v.as_int().max(0) as u64,
        warn: Some(|s| {
            (s.sql.watch_default_secs > 0).then_some(
                "Every result you open will re-run on a loop until you turn its watch off.",
            )
        }),
    },
    SettingDef {
        key: "sql.watch_min_secs",
        tab: SettingsTab::Sql,
        group: "Watch",
        en_label: "Minimum watch interval",
        en_help: "Floor under any watch interval. A watch is a query on a loop, so this \
               is a load guard; production connections are floored at 10s regardless.",
        applies: Applies::Sql,
        control: Control::Segments(&[seg("1s", int(1)), seg("2s", int(2)), seg("5s", int(5))]),
        get: |s| Value::Int(s.sql.watch_min_secs as i64),
        set: |s, v| s.sql.watch_min_secs = v.as_int().clamp(1, 3600) as u64,
        warn: Some(|s| {
            (s.sql.watch_min_secs < 2)
                .then_some("A 1s floor re-runs the query 60 times a minute, per watched tab.")
        }),
    },
    // --- kv seam ---
    SettingDef {
        key: "kv.default_query_mode",
        tab: SettingsTab::Kv,
        group: "Browsing",
        en_label: "Default filter mode",
        en_help: "How a new browse tab's filter box reads what you type. Changeable per tab \
               from the filter dropdown.",
        applies: Applies::Kv,
        control: Control::Segments(&[
            seg("Glob", text("glob")),
            seg("Prefix", text("prefix")),
            seg("Exact", text("exact")),
            seg("Fuzzy", text("fuzzy")),
            seg("Value", text("value")),
        ]),
        get: |s| text(s.kv.default_query_mode.as_str()),
        set: |s, v| s.kv.default_query_mode = KvQueryMode::from_str(v.as_text()),
        warn: None,
    },
    SettingDef {
        key: "kv.auto_refresh_secs",
        tab: SettingsTab::Kv,
        group: "Browsing",
        en_label: "Auto-refresh keys",
        en_help: "How often a new key-browser tab re-scans the keyspace. Off by default; \
               change it for an open tab from the browser's actions menu.",
        applies: Applies::Kv,
        control: Control::Segments(&[
            seg("Off", int(0)),
            seg("2s", int(2)),
            seg("5s", int(5)),
            seg("10s", int(10)),
            seg("30s", int(30)),
        ]),
        get: |s| Value::Int(s.kv.auto_refresh_secs as i64),
        set: |s, v| s.kv.auto_refresh_secs = v.as_int().max(0) as u64,
        warn: Some(|s| {
            (s.kv.auto_refresh_secs > 0 && s.kv.auto_refresh_secs < 5).then_some(
                "A fast re-scan keeps a SCAN running against the server almost constantly.",
            )
        }),
    },
    SettingDef {
        key: "kv.max_resident_keys",
        tab: SettingsTab::Kv,
        group: "Performance",
        en_label: "Max resident keys",
        en_help: "Keys one browse tab keeps in memory before evicting the oldest, so a long \
               unfiltered browse can't grow without bound.",
        applies: Applies::Kv,
        control: Control::Segments(&[
            seg("5K", int(5_000)),
            seg("20K", int(20_000)),
            seg("100K", int(100_000)),
            seg("500K", int(MAX_RESIDENT_KEYS as i64)),
        ]),
        get: |s| Value::Int(s.kv.max_resident_keys as i64),
        set: |s, v| s.kv.max_resident_keys = v.as_int().max(0) as usize,
        warn: Some(|s| {
            (s.kv.max_resident_keys > 100_000)
                .then_some("Half a million resident keys is a large memory rail for one tab.")
        }),
    },
    SettingDef {
        key: "kv.preview_count",
        tab: SettingsTab::Kv,
        group: "Performance",
        en_label: "Collection preview size",
        en_help: "Elements of a list or stream the inspector pulls per fetch.",
        applies: Applies::Kv,
        control: Control::Segments(&[
            seg("50", int(50)),
            seg("200", int(200)),
            seg("1000", int(1_000)),
            seg("5000", int(MAX_PREVIEW_COUNT as i64)),
        ]),
        get: |s| Value::Int(s.kv.preview_count as i64),
        set: |s, v| s.kv.preview_count = v.as_int().max(0) as usize,
        warn: None,
    },
    // --- doc seam ---
    SettingDef {
        key: "doc.default_view",
        tab: SettingsTab::Doc,
        group: "Display",
        en_label: "Default view",
        en_help: "How a newly-opened collection renders. Changeable per tab from the \
               collection toolbar.",
        applies: Applies::Doc,
        control: Control::Segments(&[
            seg("Table", text("table")),
            seg("List", text("list")),
            seg("JSON", text("json")),
        ]),
        get: |s| text(s.doc.default_view.as_str()),
        set: |s, v| s.doc.default_view = DocView::from_str(v.as_text()),
        warn: None,
    },
    SettingDef {
        key: "doc.max_columns",
        tab: SettingsTab::Doc,
        group: "Display",
        en_label: "Max table columns",
        en_help: "Top-level fields the sampled-column table shows. A wider document is still \
               whole in List, JSON, and the inspector.",
        applies: Applies::Doc,
        control: Control::Segments(&[
            seg("8", int(8)),
            seg("12", int(12)),
            seg("24", int(24)),
            seg("64", int(MAX_DOC_COLUMNS as i64)),
        ]),
        get: |s| Value::Int(s.doc.max_columns as i64),
        set: |s, v| s.doc.max_columns = v.as_int().max(0) as usize,
        warn: None,
    },
    // --- safety (cross-engine) ---
    SettingDef {
        key: "safety.confirm_from",
        tab: SettingsTab::Safety,
        group: "Confirmations",
        en_label: "Confirm from",
        en_help: "How dangerous an action has to be before RED asks first. \"Risky\" covers an \
               UPDATE or DELETE with no WHERE, a privilege change, a Redis key delete, or a \
               MongoDB document delete; \"Destructive\" is a DROP or TRUNCATE, always \
               confirmed by typing the object's name. A connection marked Prod confirms \
               from Risky whatever this says.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("Any write", text("write")),
            seg("Risky", text("risky")),
            seg("Destructive", text("critical")),
            seg("Never", text("never")),
        ]),
        get: |s| text(s.safety.confirm_from.as_str()),
        set: |s, v| s.safety.confirm_from = ConfirmThreshold::from_str(v.as_text()),
        warn: Some(|s| {
            (s.safety.confirm_from == ConfirmThreshold::Never).then_some(
                "Nothing will be confirmed, including DROP and TRUNCATE. Connections marked \
                 Prod still confirm.",
            )
        }),
    },
    SettingDef {
        key: "safety.confirm_close_tab",
        tab: SettingsTab::Safety,
        group: "Confirmations",
        en_label: "Confirm closing a tab",
        en_help: "Ask before closing a tab that holds unsaved work.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.safety.confirm_close_tab),
        set: |s, v| s.safety.confirm_close_tab = v.as_bool(),
        warn: None,
    },
    SettingDef {
        key: "safety.ai_review",
        tab: SettingsTab::Safety,
        group: "Review",
        en_label: "Ask the assistant to review",
        en_help: "Add a second opinion from your AI agent to the confirmation, for the mistakes \
               a keyword check can't see (a filter that looks inverted, say). This sends the \
               statement and a summary of your schema to the configured provider. It is \
               advice only: it never runs anything, and never unlocks the confirmation.",
        applies: Applies::Sql,
        control: Control::Toggle,
        get: |s| Value::Bool(s.safety.ai_review),
        set: |s, v| s.safety.ai_review = v.as_bool(),
        warn: None,
    },
    // --- ai ---
    SettingDef {
        key: "ai.enabled",
        tab: SettingsTab::Ai,
        group: "",
        en_label: "Enable agent",
        en_help: "The grounded chat sidepanel (⌘L). Off is a true kill switch.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.ai.enabled),
        set: |s, v| s.ai.enabled = v.as_bool(),
        warn: None,
    },
    SettingDef {
        key: "ai.tier",
        tab: SettingsTab::Ai,
        group: "Database access",
        en_label: "Access tier",
        en_help: "How much the agent can see. Off: nothing. Schema: structure only. Read: \
               capped SELECT/EXPLAIN. Write: adds INSERT/UPDATE/DELETE, each needing \
               per-statement approval.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("Off", text("off")),
            seg("Schema", text("schema")),
            seg("Read", text("read")),
            seg("Write", text("write")),
        ]),
        // Normalized through `AiTier::parse`, so a hand-edited `"READ"` still
        // lights the right segment instead of showing as a custom value.
        get: |s| text(red_core::AiTier::parse(&s.ai.tier).label()),
        set: |s, v| s.ai.tier = v.as_text().to_string(),
        warn: Some(|s| {
            (red_core::AiTier::parse(&s.ai.tier) == red_core::AiTier::Write).then_some(
                "The agent can propose writes. Each still needs your per-statement approval.",
            )
        }),
    },
    SettingDef {
        key: "ai.limits.max_rows",
        tab: SettingsTab::Ai,
        group: "Read-tier resource guards",
        en_label: "Max rows per query",
        en_help: "Ceiling on rows one tool SELECT returns; a larger LIMIT is clamped.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("100", int(100)),
            seg("500", int(500)),
            seg("1000", int(1000)),
            seg("5000", int(5000)),
            seg("50000", int(50_000)),
        ]),
        get: |s| Value::Int(s.ai.limits.max_rows as i64),
        set: |s, v| s.ai.limits.max_rows = v.as_int().max(0) as usize,
        warn: Some(|s| {
            (s.ai.limits.max_rows > 5000).then_some(
                "Above the safe default: big result sets mean slower queries and higher cost.",
            )
        }),
    },
    SettingDef {
        key: "ai.limits.statement_timeout_ms",
        tab: SettingsTab::Ai,
        group: "Read-tier resource guards",
        en_label: "Statement timeout",
        en_help: "Abort a tool query that runs longer than this.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("Off", int(0)),
            seg("5s", int(5_000)),
            seg("15s", int(15_000)),
            seg("30s", int(30_000)),
        ]),
        get: |s| Value::Int(s.ai.limits.statement_timeout_ms as i64),
        set: |s, v| s.ai.limits.statement_timeout_ms = v.as_int().max(0) as u64,
        warn: None,
    },
    SettingDef {
        key: "ai.limits.max_result_bytes",
        tab: SettingsTab::Ai,
        group: "Read-tier resource guards",
        en_label: "Result size cap",
        en_help: "Trim a tool result larger than this before handing it to the model.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("64 KB", int(64 * 1024)),
            seg("256 KB", int(256 * 1024)),
            seg("1 MB", int(1024 * 1024)),
            seg("5 MB", int(5 * 1024 * 1024)),
            seg("Off", int(0)),
        ]),
        get: |s| Value::Int(s.ai.limits.max_result_bytes as i64),
        set: |s, v| s.ai.limits.max_result_bytes = v.as_int().max(0) as usize,
        warn: Some(|s| {
            (s.ai.limits.max_result_bytes == 0 || s.ai.limits.max_result_bytes > 1024 * 1024)
                .then_some("Above the safe cap: large results flood the context and drive up cost. “Off” removes the cap.")
        }),
    },
    SettingDef {
        key: "ai.limits.max_tool_calls",
        tab: SettingsTab::Ai,
        group: "Read-tier resource guards",
        en_label: "Tool calls per chat",
        en_help: "Tool-call budget for one conversation; bounds a runaway loop.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("25", int(25)),
            seg("50", int(50)),
            seg("100", int(100)),
            seg("200", int(200)),
            seg("500", int(500)),
            seg("Off", int(0)),
        ]),
        get: |s| Value::Int(s.ai.limits.max_tool_calls as i64),
        set: |s, v| s.ai.limits.max_tool_calls = v.as_int().max(0) as usize,
        warn: Some(|s| {
            (s.ai.limits.max_tool_calls == 0 || s.ai.limits.max_tool_calls > 200)
                .then_some("Above the safe budget: a runaway agent loop can rack up cost. “Off” removes the cap.")
        }),
    },
    SettingDef {
        key: "ai.preview_writes",
        tab: SettingsTab::Ai,
        group: "Safety",
        en_label: "Preview affected rows",
        en_help: "Before you approve an agent write, count the rows it would change and show a \
               few of them. Turning this off leaves the approval prompt showing only the SQL.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.ai.preview_writes),
        set: |s, v| s.ai.preview_writes = v.as_bool(),
        warn: Some(|s| {
            (!s.ai.preview_writes).then_some(
                "Without a row count, “Allow this?” is a question you cannot check, and the \
                 realistic answer is a rubber stamp.",
            )
        }),
    },
    SettingDef {
        key: "ai.sandbox_timeout_secs",
        tab: SettingsTab::Ai,
        group: "Safety",
        en_label: "Review transaction timeout",
        en_help: "How long uncommitted agent changes wait for your Commit or Roll back before \
               rolling back on their own. An open transaction holds locks, so it cannot wait \
               indefinitely.",
        applies: Applies::All,
        control: Control::Segments(&[
            seg("30s", int(30)),
            seg("1m", int(60)),
            seg("2m", int(120)),
            seg("5m", int(300)),
            seg("10m", int(600)),
        ]),
        get: |s| Value::Int(s.ai.sandbox_timeout_secs as i64),
        set: |s, v| s.ai.sandbox_timeout_secs = v.as_int().max(30) as u64,
        warn: Some(|s| {
            (s.ai.sandbox_timeout_secs > 300).then_some(
                "A long-held transaction takes locks and (on Postgres) bloats the table it \
                 touched, whether or not you commit it.",
            )
        }),
    },
    SettingDef {
        key: "ai.show_thinking",
        tab: SettingsTab::Ai,
        group: "Display",
        en_label: "Show thinking",
        en_help: "Show a summarized “thinking…” affordance while the model reasons.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.ai.show_thinking),
        set: |s, v| s.ai.show_thinking = v.as_bool(),
        warn: None,
    },
    // --- about / updates ---
    SettingDef {
        key: "update.auto_update",
        tab: SettingsTab::About,
        group: "Updates",
        en_label: "Automatic updates",
        en_help: "Check GitHub for newer signed builds in the background and stage them for a \
               one-click restart.",
        applies: Applies::All,
        control: Control::Toggle,
        get: |s| Value::Bool(s.update.auto_update),
        set: |s, v| s.update.auto_update = v.as_bool(),
        warn: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The English catalog has to reproduce the registry's source text exactly.
    ///
    /// `assets/i18n/en.toml` is generated from this table, so the two drift the
    /// moment someone edits a label and does not re-run the extractor. The UI
    /// renders the catalog, so that drift would silently ship the *old* wording,
    /// with nothing on screen to suggest the code and the catalog disagree. This
    /// also pins the key derivation: a slug computed differently here than in the
    /// script shows up as a key resolving to itself.
    ///
    /// Looks the key up raw rather than through `label()` and friends: those fall
    /// back to the English source when a key is missing, which is right for the UI
    /// and would hide from this test the exact case it exists to catch.
    #[test]
    fn every_registry_string_is_in_the_english_catalog() {
        crate::i18n::apply(crate::i18n::DEFAULT);

        let mut stale = Vec::new();
        let mut check = |key: String, want: &str| {
            let got = crate::i18n::lookup(&key);
            if got.as_ref() != want {
                stale.push(format!("  {key}\n    catalog: {got}\n    code:    {want}"));
            }
        };

        for tab in SettingsTab::ALL {
            let id = slug(&format!("{tab:?}"));
            check(format!("settings.tab.{id}"), tab.label());
            if let Some(want) = tab.subtitle() {
                check(format!("settings.tab.{id}.subtitle"), want);
            }
        }
        for def in defs() {
            check(format!("settings.{}.label", def.key), def.en_label);
            check(format!("settings.{}.help", def.key), def.en_help);
            if !def.group.is_empty() {
                check(format!("settings.group.{}", slug(def.group)), def.group);
            }
            if let Control::Segments(segments) = &def.control {
                for segment in *segments {
                    check(
                        format!("settings.{}.seg.{}", def.key, slug(segment.en_label)),
                        segment.en_label,
                    );
                }
            }
        }

        assert!(
            stale.is_empty(),
            "assets/i18n/settings/en.ftl is out of date with settings_reg.rs:\n{}\n\n\
             Re-run: python3 scripts/i18n-extract.py",
            stale.join("\n")
        );
    }

    /// Every registry entry must round-trip: applying a segment's value has to
    /// make `get` return it. A typo in a `set` (writing the wrong field, or a
    /// lossy conversion) shows up here rather than as a control that visibly
    /// refuses to move.
    #[test]
    fn every_segment_round_trips() {
        for def in defs() {
            let Control::Segments(segments) = &def.control else {
                continue;
            };
            for segment in *segments {
                let mut s = Settings::default();
                (def.set)(&mut s, &segment.value);
                // Clamp, like the real edit path: a preset must survive it, or the
                // control would snap back and look broken.
                s.clamp();
                assert_eq!(
                    (def.get)(&s),
                    segment.value,
                    "{}: segment “{}” didn't round-trip",
                    def.key,
                    segment.en_label
                );
            }
        }
    }

    /// Both toggle positions must stick, for the same reason.
    #[test]
    fn every_toggle_round_trips() {
        for def in defs() {
            if !matches!(def.control, Control::Toggle) {
                continue;
            }
            for on in [true, false] {
                let mut s = Settings::default();
                (def.set)(&mut s, &Value::Bool(on));
                s.clamp();
                assert_eq!((def.get)(&s), Value::Bool(on), "{}", def.key);
            }
        }
    }

    /// The shipped default must be one of the offered presets. If it isn't, the
    /// panel opens with nothing selected on a fresh install, which reads as a bug.
    #[test]
    fn the_default_is_always_a_preset() {
        for def in defs() {
            let Control::Segments(segments) = &def.control else {
                continue;
            };
            let default = def.default_value();
            assert!(
                segments.iter().any(|s| s.value == default),
                "{}: default {default:?} is not among its presets",
                def.key
            );
        }
    }

    /// Keys are the file paths users hand-edit and the search index; a duplicate
    /// would make Reset and search ambiguous.
    #[test]
    fn keys_are_unique_and_dotted() {
        let mut seen = std::collections::HashSet::new();
        for def in defs() {
            assert!(seen.insert(def.key), "duplicate key {}", def.key);
            assert!(
                def.key.contains('.'),
                "{} should be a dotted section.field path",
                def.key
            );
        }
    }

    /// A fresh install shows no Modified badges anywhere.
    #[test]
    fn nothing_is_modified_at_defaults() {
        let s = Settings::default();
        for def in defs() {
            assert!(!def.is_modified(&s), "{} is modified at defaults", def.key);
        }
    }

    /// The shipped `default-settings.toml` is RED's settings documentation (it's
    /// what "Open settings file" seeds and what the About page opens), and it had
    /// silently drifted: three of the nine sections — `[redis]`, `[update]`,
    /// `[keymap]` — appeared nowhere in it, so `vim_mode` and `auto_update` were
    /// invisible to anyone reading the file. Assert every section and every
    /// registered key is actually documented.
    #[test]
    fn the_shipped_template_documents_every_setting() {
        let template = crate::assets::DEFAULT_SETTINGS;

        // It must still parse as the real thing, or the seeded file is broken.
        let parsed: Settings = toml::from_str(template).expect("template is valid Settings");
        let _ = parsed;

        let mut missing = Vec::new();
        for section in section_names() {
            if section_span(template, &section).is_none() {
                missing.push(format!("[{section}] (section header)"));
            }
        }
        for def in defs() {
            // Look for the assignment *inside its own section's span*, not just
            // anywhere in the file: `page_size` documented under `[kv]` would
            // otherwise pass for `data.page_size`.
            let (section, leaf) = def.key.rsplit_once('.').expect("dotted key");
            let documented = section_span(template, section)
                .is_some_and(|body| body.contains(&format!("\n{leaf} = ")));
            if !documented {
                missing.push(def.key.to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "assets/default-settings.toml doesn't document: {missing:?}"
        );
    }

    /// The top-level section headers in the current file format. Nested tables
    /// aren't enumerated here — `[ai.limits]` is reached through its own registry
    /// keys below, and `appearance.theme` is an inline-table *value*, not a
    /// section, so walking one level deeper would demand a header that shouldn't
    /// exist.
    fn section_names() -> Vec<String> {
        let toml = toml::to_string(&Settings::default()).expect("serialize defaults");
        let value: toml::Value = toml.parse().expect("reparse defaults");
        let toml::Value::Table(sections) = value else {
            panic!("settings should serialize as a table");
        };
        sections.keys().cloned().collect()
    }

    /// The template text belonging to `[section]`: from its header to the next
    /// one (of any depth), so a key is only credited to the section it's under.
    fn section_span<'a>(template: &'a str, section: &str) -> Option<&'a str> {
        let header = format!("\n[{section}]\n");
        let start = template.find(&header)? + header.len();
        let rest = &template[start..];
        // The next header starts a line with `[`; `[[ai.agents]]` counts too, and
        // it is only ever inside a comment block here.
        let end = rest.find("\n[").map_or(rest.len(), |i| i + 1);
        Some(&rest[..end])
    }

    /// The guard against the failure this whole overhaul is about: a setting
    /// added to the file format but never surfaced in the UI. Every leaf field of
    /// every section is either registered here or listed as deliberately
    /// file-only / custom-rendered.
    ///
    /// Compares against the serialized default so it reads the *file's* field
    /// names — the thing a user actually sees — and can't drift from them.
    #[test]
    fn every_section_is_covered() {
        /// Settings with no registry row, each for a stated reason. Adding a field
        /// to `Settings` without a row means adding it here on purpose.
        const EXEMPT: &[(&str, &str)] = &[
            // Rendered by hand: not simple value edits.
            ("appearance.theme", "custom light/dark pair pickers"),
            ("appearance.ui_font_family", "searchable font combo"),
            ("appearance.ui_mono_family", "searchable font combo"),
            ("appearance.ui_font_size", "numeric stepper"),
            ("editor.font_family", "searchable font combo"),
            ("editor.font_size", "numeric stepper"),
            ("ai.report_dir", "native folder picker"),
            ("ai.default_agent", "chosen in the Accounts list"),
            // Plumbing: deliberately file-only (see AiSettings' docs).
            ("ai.provider", "legacy; superseded by ai.agents"),
            ("ai.model", "legacy; superseded by ai.agents"),
            ("ai.agent_command", "legacy; superseded by ai.agents"),
            ("ai.agents", "agent profiles, edited in the file"),
            ("ai.subscription_model", "last-used state, not a preference"),
            (
                "ai.subscription_reasoning",
                "last-used state, not a preference",
            ),
            ("ai.subscription_mode", "last-used state, not a preference"),
            // Clamped internals with no useful preset row.
            ("update.check_interval_hours", "file-only cadence"),
        ];

        let toml = toml::to_string(&Settings::default()).expect("serialize defaults");
        let value: toml::Value = toml.parse().expect("reparse defaults");
        let registered: std::collections::HashSet<&str> = defs().iter().map(|d| d.key).collect();

        let mut missing = Vec::new();
        let toml::Value::Table(sections) = &value else {
            panic!("settings should serialize as a table");
        };
        for (section, body) in sections {
            let toml::Value::Table(fields) = body else {
                continue;
            };
            for (field, sub) in fields {
                // One level of nesting (`ai.limits.*`) is enough for today's shape.
                let leaves: Vec<String> = match sub {
                    toml::Value::Table(inner) if section == "ai" && field == "limits" => inner
                        .keys()
                        .map(|k| format!("{section}.{field}.{k}"))
                        .collect(),
                    _ => vec![format!("{section}.{field}")],
                };
                for key in leaves {
                    let exempt = EXEMPT.iter().any(|(k, _)| *k == key);
                    if !exempt && !registered.contains(key.as_str()) {
                        missing.push(key);
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "settings with no panel row and no stated exemption: {missing:?}\n\
             Add a SettingDef in settings_reg.rs, or an EXEMPT entry saying why not."
        );
    }
}
