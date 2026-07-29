#!/usr/bin/env python3
"""Regenerate the English catalog and the pseudolocale from the settings registry.

The registry in `crates/red/src/settings_reg.rs` describes every simple setting
as data, so the English source text can be lifted straight out of it rather than
hand-copied into a catalog that then drifts. Run after adding or renaming a
setting:

    python3 scripts/i18n-extract.py

Writes `assets/i18n/<domain>/en.ftl` (the canonical source every other locale
mirrors) and `assets/i18n/<domain>/en-XA.ftl` (a pseudolocale: same catalog,
letters accented and the whole string bracketed). Switching RED to `en-XA` makes
any string that was never extracted obvious, because it is the only text still in
plain ASCII.

**Domain is the key's first segment**, so `settings.tab.editor` lives in
`settings/`. The locale is the *file stem* and merges every file
it finds, so the directory is free organisation: one catalog per UI area keeps any
single file reviewable, and lets two translators work without touching the same
file. Deriving the domain from the key rather than tracking it separately is what
makes the same key in two domains impossible rather than merely detectable.

Key scheme, derived from the registry's existing dotted `key` so no per-row key
literal is needed:

    settings.tab.<tab>              page name in the sidebar
    settings.group.<slug>           group header within a page
    settings.<key>.label            the row's name
    settings.<key>.help             the row's one-line description
    settings.<key>.seg.<slug>       one preset in a segmented control
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REG = ROOT / "crates" / "red" / "src" / "settings_reg.rs"
KEYMAP = ROOT / "crates" / "red" / "src" / "keymap.rs"
PALETTE = ROOT / "crates" / "red" / "src" / "palette.rs"
OUT_DIR = ROOT / "assets" / "i18n"

# A Rust string literal, including the `\` line continuations rustfmt inserts to
# keep long `help` text inside the line limit.
STR = r'"((?:[^"\\]|\\.)*)"'

# Rust drops a trailing `\`, the newline after it, and the next line's indent.
# Reproducing that here is what keeps a wrapped `help` one sentence in the
# catalog instead of a string with a hole in the middle.
CONTINUATION = re.compile(r"\\\n\s*")


UNICODE_ESCAPE = re.compile(r"\\u\{([0-9a-fA-F]{1,6})\}")


def unescape(literal):
    r"""A Rust string literal's source text as the string it denotes.

    `\\u{2318}` has to become `\u{2318}` here: left as source text it would reach the
    catalog verbatim, and its `{2318}` could then be read as a Fluent placeable.
    Decoding also lets the "is there any letter in this?" filter see `\u{2014}` for
    the em dash it is, rather than as the letter `u`.
    """
    text = CONTINUATION.sub("", literal)
    text = UNICODE_ESCAPE.sub(lambda m: chr(int(m.group(1), 16)), text)
    return (
        text.replace('\\"', '"')
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\\", "\\")
    )


def slug(text):
    """A stable, readable key fragment: lowercase, non-alphanumerics collapsed."""
    return re.sub(r"_+", "_", re.sub(r"[^a-z0-9]+", "_", text.lower())).strip("_")


def parse_tabs(src):
    """`SettingsTab::label()` match arms, as (tab_variant, english)."""
    body = re.search(
        r"fn label\(self\) -> &'static str \{(.*?)\n    \}", src, re.S
    )
    if not body:
        sys.exit("could not find SettingsTab::label(); the registry shape changed")
    return re.findall(rf"SettingsTab::(\w+) => {STR}", body.group(1))


def parse_subtitles(src):
    """`SettingsTab::subtitle()` arms, as (tab_variant, english)."""
    body = re.search(
        r"fn subtitle\(self\) -> Option<&'static str> \{(.*?)\n    \}", src, re.S
    )
    if not body:
        sys.exit("could not find SettingsTab::subtitle(); the registry shape changed")
    return re.findall(rf"SettingsTab::(\w+) =>\s*\{{?\s*Some\({STR}\)", body.group(1))


def parse_defs(src):
    """Every `SettingDef { .. }` entry, as a dict of its string fields."""
    table = src.split("static DEFS: &[SettingDef] = &[", 1)
    if len(table) != 2:
        sys.exit("could not find the DEFS table; the registry shape changed")

    defs = []
    for block in re.findall(r"SettingDef \{(.*?)\n    \},", table[1], re.S):
        got = {}
        for field in ("key", "group", "en_label", "en_help"):
            m = re.search(rf"\b{field}: {STR}", block, re.S)
            if m:
                got[field] = unescape(m.group(1))
        missing = {"key", "group", "en_label", "en_help"} - got.keys()
        if missing:
            sys.exit(f"SettingDef missing {sorted(missing)}:\n{block[:200]}")
        got["segments"] = [unescape(s) for s in re.findall(rf"seg\({STR}", block)]

        # A row whose presets live in a named `static ..: &[Segment]` rather than
        # inline. `LOCALE_SEGMENTS` is defined twice under opposing `cfg`s, so
        # every definition contributes: the catalog has to cover the union, or a
        # dev-only preset resolves to its key.
        for name in re.findall(r"Control::Segments\((\w+)\)", block):
            for body in re.findall(
                rf"static {name}: &\[Segment\] = &\[(.*?)\];", src, re.S
            ):
                for seg in re.findall(rf"seg\({STR}", body):
                    seg = unescape(seg)
                    if seg not in got["segments"]:
                        got["segments"].append(seg)

        defs.append(got)
    return defs


def build_keymap_catalog(src):
    """`keymap.rs`'s `DEFAULTS` table: `def(keystroke, action, label, context)`.

    Keyed on the action name, so the two rows of an action with two default
    keystrokes (`BeginEdit` is Enter and F2) collapse to one string to translate
    rather than two that could drift apart.
    """
    table = src.split("const DEFAULTS: &[ActionDef] = &[", 1)
    if len(table) != 2:
        sys.exit("could not find the DEFAULTS table; keymap.rs shape changed")
    body = table[1].split("\n];", 1)[0]

    catalog = {}
    rows = re.findall(rf"\bdef\(\s*{STR}\s*,\s*{STR}\s*,\s*{STR}\s*,", body, re.S)
    if not rows:
        sys.exit("parsed no rows out of DEFAULTS; the `def(..)` shape changed")

    for _keystroke, action, label in rows:
        label = unescape(label)
        key = f"keymap.{unescape(action)}.label"
        # Same action, two labels: the key cannot represent both, and picking one
        # would silently relabel a row in the editor.
        if catalog.get(key, label) != label:
            sys.exit(f"{key} has two different labels: {catalog[key]!r} vs {label!r}")
        catalog[key] = label
    return catalog


def build_shortcuts_catalog(src):
    """`keymap.rs`'s `SHORTCUTS` table: the `⌘/` keyboard reference.

    Keyed on the ids the table carries, not on the description text, so rewording
    a line does not orphan its translations. Keystrokes are symbols and are never
    extracted.
    """
    table = src.split("static SHORTCUTS: &[ShortcutGroup] = &[", 1)
    if len(table) != 2:
        sys.exit("could not find the SHORTCUTS table; keymap.rs shape changed")
    body = table[1].split("\n];", 1)[0]

    catalog = {}
    for group in re.findall(rf"\(\s*{STR}\s*,\s*{STR}\s*,\s*&\[(.*?)\n        \],", body, re.S):
        gid, gname, rows = unescape(group[0]), unescape(group[1]), group[2]
        catalog[f"shortcuts.{gid}.title"] = gname
        for rid, _keys, desc in re.findall(
            rf"\(\s*{STR}\s*,\s*{STR}\s*,\s*{STR}\s*,?\s*\)", rows, re.S
        ):
            catalog[f"shortcuts.{gid}.{unescape(rid)}"] = unescape(desc)
    if not catalog:
        sys.exit("parsed no rows out of SHORTCUTS; the table shape changed")
    return catalog


def build_palette_catalog(src):
    """`palette.rs`'s `item(id, label)` rows.

    Keyed on the row's own id, so the key and the command it names move together
    and no call site writes the key out a second time. Rows with an interpolated
    label are deliberately not routed through `item`, so they do not appear here.
    """
    catalog = {}
    for cmd_id, label in re.findall(
        rf"\bitem\(\s*{STR}\s*,\s*{STR}\s*\)", strip_comment_lines(src), re.S
    ):
        cmd_id, label = unescape(cmd_id), unescape(label)
        key = f"palette.{slug(cmd_id)}"
        if catalog.get(key, label) != label:
            sys.exit(f"{key} has two different labels: {catalog[key]!r} vs {label!r}")
        catalog[key] = label
    if not catalog:
        sys.exit("parsed no rows out of palette.rs; the `item(..)` shape changed")
    return catalog


def strip_comment_lines(text):
    """Blank out whole-line comments before scanning for `tr!`.

    A doc comment showing how to *use* the macro is still a `tr!(..)` as far as a
    regex is concerned, and commented-out code is worse: both would ship a key
    nothing renders. Only whole-line comments are removed, so a `//` inside a
    string literal (a URL, say) survives.
    """
    return "\n".join(
        "" if line.lstrip().startswith("//") else line for line in text.split("\n")
    )


def build_callsite_catalog(src_root):
    """Every `tr!("key", "English")` in the tree.

    The second extraction shape: a literal in a `render` with no identity of its
    own, so the key is written beside the text and this lifts the pair out. The
    macro takes both as literals precisely so a regex can find them.
    """
    catalog = {}
    for path in sorted(src_root.rglob("*.rs")):
        text = strip_comment_lines(path.read_text(encoding="utf-8"))
        for key, english in re.findall(rf"\btr!\(\s*{STR}\s*,\s*{STR}\s*[,)]", text, re.S):
            key, english = unescape(key), unescape(english)
            if catalog.get(key, english) != english:
                sys.exit(
                    f"{path.relative_to(ROOT)}: {key} is used with two different "
                    f"English strings: {catalog[key]!r} vs {english!r}"
                )
            catalog[key] = english
    return catalog


def build_catalog(src):
    """The full key -> English map, in a stable order."""
    catalog = {}
    for variant, english in parse_tabs(src):
        catalog[f"settings.tab.{slug(variant)}"] = english
    for variant, english in parse_subtitles(src):
        catalog[f"settings.tab.{slug(variant)}.subtitle"] = english

    for d in parse_defs(src):
        # `group: ""` means "this row has no header", a layout sentinel rather
        # than text. It has nothing to translate, and Fluent cannot represent an
        # empty message anyway.
        if d["group"]:
            catalog[f"settings.group.{slug(d['group'])}"] = d["group"]
        catalog[f"settings.{d['key']}.label"] = d["en_label"]
        catalog[f"settings.{d['key']}.help"] = d["en_help"]
        for seg in d["segments"]:
            catalog[f"settings.{d['key']}.seg.{slug(seg)}"] = seg
    return catalog


# Latin-1/Extended-A lookalikes: readable as English while visibly not English.
PSEUDO = str.maketrans(
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ",
    "ábçdéfghíjklmñöpqrstúvwxyzÁBÇDÉFGHÍJKLMÑÖPQRSTÚVWXYZ",
)
# `{rows}` and friends must survive untouched: accenting a placeholder name would
# rename the argument and the value would never land.
PLACEHOLDER = re.compile(r"\{[^}]*\}")


def pseudo(text):
    parts = PLACEHOLDER.split(text)
    holes = PLACEHOLDER.findall(text)
    out = parts[0].translate(PSEUDO)
    for hole, part in zip(holes, parts[1:]):
        out += hole + part.translate(PSEUDO)
    return f"[{out}]"


ATTRIBUTES = ("label", "help", "subtitle")


def fluent_id(key):
    """A dotted key as a Fluent message id plus an optional attribute.

    Mirrors `fluent_id` in `crates/red/src/i18n.rs`; the drift tests fail if the
    two ever derive different ids from one key. A Fluent id cannot contain a dot
    (there it means "attribute"), so dots become hyphens, and a trailing
    label/help/subtitle becomes a real attribute so a row's text groups under one
    message where the translator can see it together.
    """
    head, _, tail = key.rpartition(".")
    if head and tail in ATTRIBUTES:
        return head.replace(".", "-"), tail
    return key.replace(".", "-"), None


NAMED_ARG = re.compile(r"\{([a-zA-Z_][a-zA-Z0-9_]*)\}")


def escape_ftl(value):
    """A call-site string as a Fluent pattern.

    The English source is written in Rust `format!` syntax (`{rows}`) so it can
    serve as the fallback verbatim; Fluent spells the same thing `{ $rows }`.
    Converting here means one syntax at the call site and no second copy for a
    translator to keep in sync. Braces that are not a named argument are literal,
    and Fluent has no escape character, so they ride as string placeables.
    """
    out, last = [], 0
    for m in NAMED_ARG.finditer(value):
        out.append(_escape_literal(value[last : m.start()]))
        out.append(f"{{ ${m.group(1)} }}")
        last = m.end()
    out.append(_escape_literal(value[last:]))
    return "".join(out)


def _escape_literal(text):
    """Braces that are not a placeholder, as Fluent string placeables.

    One pass, not two `replace`s: escaping `{` produces a `{"{"}` that itself
    contains a `}`, so a second pass over `}` would re-escape what the first pass
    just wrote.
    """
    return re.sub(r"[{}]", lambda m: '{"' + m.group(0) + '"}', text)


def render(domain, catalog, transform):
    lines = [
        f"# The `{domain}` catalog. Generated by scripts/i18n-extract.py; do not",
        "# edit by hand. The locale is this file's name, the folder is the UI area.",
        "# Every locale mirrors these messages in its own file next to this one, so",
        "# two translators never edit the same lines.",
        "#",
        "# Plural forms belong in the translation, not in the code:",
        "#",
        "#   some-message = { $n ->",
        "#       [one] { $n } row",
        "#      *[other] { $n } rows",
        "#   }",
        "",
    ]

    # Group attributes under their message, which is the whole reason to use them.
    messages = {}
    for key, english in catalog.items():
        msg, attr = fluent_id(key)
        messages.setdefault(msg, {"value": None, "attrs": {}})
        if attr:
            messages[msg]["attrs"][attr] = english
        else:
            messages[msg]["value"] = english

    for msg, body in messages.items():
        value = body["value"]
        if value is not None:
            lines.append(f"{msg} = {escape_ftl(transform(value))}")
        else:
            lines.append(f"{msg} =")
        for attr, text in body["attrs"].items():
            lines.append(f"    .{attr} = {escape_ftl(transform(text))}")
        lines.append("")

    return "\n".join(lines).rstrip("\n") + "\n"


# The locales this script owns. Everything else in a domain folder is a human's
# translation and is never written or deleted here.
GENERATED = {"en": lambda s: s, "en-XA": pseudo}


def main():
    check = "--check" in sys.argv[1:]

    catalog = build_catalog(REG.read_text(encoding="utf-8"))
    catalog.update(build_keymap_catalog(KEYMAP.read_text(encoding="utf-8")))
    catalog.update(build_shortcuts_catalog(KEYMAP.read_text(encoding="utf-8")))
    catalog.update(build_palette_catalog(PALETTE.read_text(encoding="utf-8")))
    catalog.update(build_callsite_catalog(ROOT / "crates" / "red" / "src"))

    by_domain = {}
    for key, english in catalog.items():
        by_domain.setdefault(key.split(".", 1)[0], {})[key] = english

    # A key that moves between domains would otherwise leave its old definition
    # behind in the old file, where rust-i18n would merge it back in.
    stale_files = [
        f
        for f in OUT_DIR.glob("*/*.ftl")
        if f.stem in GENERATED and f.parent.name not in by_domain
    ]

    drifted = list(stale_files)
    for domain, keys in sorted(by_domain.items()):
        out = OUT_DIR / domain
        for locale, transform in GENERATED.items():
            path = out / f"{locale}.ftl"
            want = render(domain, keys, transform)
            if not path.exists() or path.read_text(encoding="utf-8") != want:
                drifted.append(path)
            if not check:
                out.mkdir(parents=True, exist_ok=True)
                path.write_text(want, encoding="utf-8")
        if not check:
            print(f"{len(keys):4d} keys -> {out.relative_to(ROOT)}/{{{','.join(GENERATED)}}}.ftl")

    if check:
        # The catalogs are generated, so "committed output matches the source" is
        # the one check that covers every domain, including the call-site strings
        # that no runtime test can enumerate.
        if drifted:
            print("catalogs are out of date with the source:")
            for path in drifted:
                print(f"  {path.relative_to(ROOT)}")
            print("\nRe-run: python3 scripts/i18n-extract.py")
            return 1
        print(f"catalogs up to date ({len(catalog)} keys, {len(by_domain)} domains)")
        return 0

    for path in stale_files:
        path.unlink()
    return 0


if __name__ == "__main__":
    sys.exit(main())
