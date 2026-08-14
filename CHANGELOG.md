# Changelog

All notable changes to RED are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Export documents from MongoDB: "Export documents…" on the collection tree's
  right-click menu and in the Documents toolbar's Actions menu writes the whole
  collection, or just what the current filter matches, to JSON, NDJSON, CSV or
  Excel. It streams one `_id`-keyset window at a time, so collection size is not
  a limit, and the toast's ✕ cancels it and removes the partial file. CSV and
  Excel flatten onto dotted columns sampled from the collection, and say so when
  a document carried a field the sample never saw.
- Import documents into a MongoDB collection from a JSON array, NDJSON, or CSV
  file. The dialog previews the first documents exactly as they will be written
  (parsed through the connection, so an extended-JSON `$oid` shows up as one),
  and you choose whether a repeated `_id` is a collision or an update.
- Copy a MongoDB collection into another collection, on the same connection or
  any other open one: append, upsert on `_id`, or replace the target outright.
  It streams window by window with progress and cancel, like the SQL table copy.
- A right-click menu on the MongoDB collection tree: open (in this tab or a new
  one), export, import, copy, drop, and refresh a database's collection list.
- A fast filter for MongoDB: `status:active age>30 created:last7d` compiles to a
  filter document, so the common narrowing costs no JSON. It understands `>`,
  `>=`, `<`, `<=`, `!=`, `field:*` for "has this field", `field:~text` for a
  case-insensitive contains, quoted values, relative dates (`last7d`, `last24h`,
  `today`), and typed scalars including ObjectIds in `_id`. A half-typed term is
  left alone rather than flagged; only a real mistake is called out. The JSON box
  is still there behind the Fast/JSON toggle.
- Field-name completion in the MongoDB filter box, from the collection's sampled
  schema: type a prefix, pick with ↑/↓ and Enter.
- Sort a MongoDB browse by clicking a column header (again for descending, a
  third time to clear); shift-click adds a second key. The active keys show as
  chips under the toolbar.
- Choose which MongoDB fields to load with the toolbar's Fields dropdown, so a
  wide collection can be narrowed to the handful you are reading. `_id` always
  comes along.
- Create and drop MongoDB indexes from the Indexes panel. The dialog covers
  compound keys, ascending/descending/text/hashed/2dsphere, unique, sparse, TTL,
  a partial filter, and a collation locale. Dropping one asks first, because the
  queries that relied on it become collection scans.
- The Indexes panel suggests an index when the current filter is scanning the
  whole collection, and creating it is one click.
- A stage builder for MongoDB aggregations, beside the raw pipeline editor: one
  editor per stage, reorder with ↑/↓, an operator palette that leads with the
  stages that fit the position, and "Preview" on any stage to run the pipeline
  truncated there. The two modes are two views of one pipeline, so switching
  never loses what you wrote.
- A History dock for MongoDB (⌘Y), listing the filters and pipelines you have
  run, badged with the collection each ran against. Clicking one points a tab at
  that collection and puts the query back.
- Saved queries now cover MongoDB: ⇧⌘S keeps the pipeline you are looking at (or
  the applied filter), ⇧⌘O opens it back into the right box at the right
  collection. They live as readable `.mongo.json` files beside the saved `.sql`
  ones.
- The row-number gutter now holds the left edge instead of scrolling away with
  the columns, so a row stays identifiable however far right you scroll.
- Freeze result columns: "Pin <column> left" in the header menu holds a column
  (or several) against the left edge while the rest scroll under it, with
  "Unpin" and "Unpin all columns" to release them. Frozen columns keep their own
  order, survive hiding and reordering, and can't take more than half the grid.
- Pin result rows (⌥⌘P, or "Pin row" in the cell menu) to hold them under the
  header while you scroll the rest of the result away. A pinned row appears
  above the grid only once it scrolls out of view, so it is never on screen
  twice; while it is in view its ordinal carries a pin in the row-number gutter,
  which releases it on click. Up to six rows can be pinned; they keep their place
  when you sort, filter or re-run the query, show staged edits and deletions
  exactly as the grid does, and stay copyable when the row itself is far off
  screen.

### Fixed
- An export that fails now says so and clears its progress toast, instead of
  leaving "Exporting…" on screen for the rest of the session.
- The result filter bar belongs to its tab. A term you were typing — a `WHERE`
  expression, a contains term, the column chips you were building — stays with
  the result it was written for instead of following you to the next tab, where
  Apply would have run it against a different grid.
- The SQL editor follows the caret sideways on a line wider than the pane, so
  typing past the right edge, jumping to the end of the line, or clicking a
  match no longer leaves the caret off-screen. A sideways trackpad swipe (or
  Shift+wheel) pans the text, and the line numbers stay put while it does.
- A sideways swipe over the SQL editor no longer scrolls it up and down
  instead.
- Selecting text with the mouse inside a result cell you are editing keeps the
  selection: releasing the button no longer commits the edit and throws the
  caret to the end of the value.
- Clicking near the top or bottom edge of any text field now puts the caret
  under the pointer instead of jumping it to the start or end of the value.
- The cell you are editing is drawn on the input background, so the text you
  have selected inside it stands out against the cell's own highlight.
- Typing in a dropdown's search field no longer triggers shortcuts underneath
  it: filtering the welcome screen by "redis" types the word instead of opening
  the edit form on the letter `e`.
- Esc with a settings dropdown open closes the dropdown and leaves the settings
  panel where it was.

## [0.22.0] - 2026-08-12

### Added
- Result columns size themselves to their contents, and drag by the header edge
  to resize; double-click that edge to fit, or use the header menu to fit one,
  fit all, or reset every width.
- Hide and reorder result columns from the header's right-click menu. The
  arrangement is per result and never re-runs the query.
- "Copy as" in the cell menu: TSV with headers, CSV, JSON, a Markdown table, an
  `IN (…)` list, or one `INSERT` per row on a table browse. Plain ⌘C is
  unchanged.
- Run a whole buffer as a script with ⌥⌘↵. Each statement reports its own
  outcome in a log beside the editor, a trailing `SELECT` opens in the grid, and
  the run stops at the first failure.
- Jump to any table or view with ⌘O, matched fuzzily, so `usrpref` finds
  `user_preferences`.
- Manual transactions: "Transaction: begin" pins the connection and holds every
  write, with the pending count in the status bar, until you commit or roll
  back. PostgreSQL, MySQL and SQLite only. Reads still come from the pool, so
  they show committed data.
- "Restore last session" now reopens the tabs you left a connection with: their
  SQL, focus, pins, per-tab database, browsed table and pane layout. Quitting no
  longer discards unsaved SQL.
- Filter welcome-screen connections by engine and environment, several values at
  once, with a count per option and a "3 of 12" heading. Sort and filters are
  remembered between launches.
- Dropdowns can be multi-select: picks toggle with the list open, the closed
  control summarises the set, and a Clear resets that dropdown alone.
- Right-click any editing surface for Cut, Copy, Paste and Select All. Clicking
  inside a highlight keeps it.
- The query editor's right-click menu leads with Run selection, Explain and
  Format selection, falling back to the statement under the caret; formatting a
  selection no longer reformats the whole tab.

### Changed
- The welcome screen's search box also matches engine and environment, and
  carries a clear button. `/` jumps into it, `←`/`→` page the roster and
  `Home`/`End` jump to its ends.

### Fixed
- Hiding a result column no longer crashes the app; the footer reads "12 of 17
  columns" while any are hidden.
- Right-clicking inside a multi-cell selection keeps it, so Copy and "Copy as"
  act on everything selected.
- Opening a PostgreSQL sequence from the schema explorer shows its definition
  instead of failing with "driver error: db error".
- PostgreSQL failures carry the server's own message and SQLSTATE wherever they
  surface, instead of a bare "db error".
- Assistant settings picked before the first message now apply to that message,
  and a fast-mode flip is remembered per agent.
- The review-transaction control is no longer shown on subscription chats, which
  are never handed write tools.
- Splitting the Redis or MongoDB browser no longer empties the key/document
  list; the browse toolbar wraps in a narrow pane instead of pushing Actions and
  refresh out of reach.

## [0.21.0] - 2026-08-03

### Added
- Server panel Overview on every engine: memory, throughput, connections,
  replication lag and uptime drawn as bars, counters that only make sense as
  rates shown as rates, and metrics your role cannot read labelled rather than
  shown as zero. Postgres, MySQL, Redis, ClickHouse and MongoDB.
- MongoDB gets a Sessions view (`$currentOp`) with a guarded kill, and Redis
  clients (`CLIENT LIST` / `CLIENT KILL`) now appear there too. Sessions are
  ordered longest-running first; RED never offers to kill its own connection.
- Server panel auto-refresh, off by default, interval shown in the panel itself,
  floor of two seconds (`behavior.server_refresh_secs`).
- Excel (`.xlsx`) export for any query result: streamed like every other export,
  numbers stay numbers and numeric-looking strings stay strings, stops at
  Excel's 1,048,576-row limit with a toast saying so.
- Redis key export, in Commands (`.redis`), JSON, or DUMP (`.rdbdump`) format,
  scoped to the visible keys, the current filter, or the whole database. Streams,
  cancels cleanly, writes expiry as absolute deadlines, reports values a text
  format cannot carry, and auto-detects `.rdbdump` on import.
- RedisJSON documents are first-class: they read as `json`, filter as their own
  type, and open in a document tree walked a level at a time, so a 200 MB
  document costs no more to open than a small one. Edit a single node or the
  whole document, create one from "New key", and two read-only agent tools map a
  document's shape or read one path. Offered only where the module is loaded.
- Point the agent at things instead of describing them: drag a table, column,
  schema, query tab or selected rows onto the assistant panel, or use "Ask AI
  about this" in the schema tree. References resolve on send, not on drop.
- File attachments in chat, dropped anywhere on the panel or added with the `+`
  beside the composer: text, images and PDFs, read off the UI thread at send
  time, with unsupported or oversized files refused in the composer with a
  reason. Attachments are always treated as data, never as instructions.
- Reply length is a setting (Settings → AI, 16K by default, up to 64K) and a
  reply that still hits it is continued rather than failed. Conversations are
  compacted as they fill, with local trimming noted in the activity trace.
- The usage gauge works on the API-key path too and reads the whole conversation,
  with the cost so far on its tooltip.
- The agent pages through a large result with a cursor over the same streaming
  machinery the grid uses, so windows tile the result with no rows repeated or
  skipped and without re-running the query.
- Answers show their sources: a numbered line above the prose, one chip per
  query, each linked to the exact call in the activity trace.
- Queries are checked for join fan-out before the agent believes the number.
  Suspicious shapes get one extra pair of counts, and the agent is told the real
  numbers before it writes its answer. Nothing is blocked.
- Review mode runs a turn's writes inside one open transaction that you commit or
  roll back at the end, on PostgreSQL, MySQL/MariaDB and SQLite. An unanswered
  review rolls itself back after two minutes.
- Write approvals count rows first ("Affects 4,213 of 812,004 rows in
  public.orders", with three shown) and warn when a statement matches none. The
  count runs on a short budget and never delays the prompt.
- The agent reads the queries you have run and your saved-query library for this
  connection, so it uses the join paths, filters and metric definitions you
  actually use. Same on Redis, for commands run and keys opened.
- Per-connection knowledge file: plain markdown that rides in every chat's prompt
  on that connection, with "Learn this database" to draft one for review.
- Many new agent tools. On SQL: FK graph, object DDL, cross-column value search,
  EXPLAIN with actual rows, health report, live sessions, schema and data diff,
  index advice. On Redis: key templates, deep collection and stream paging,
  consumer-group lag, clients, topology. On MongoDB: reference discovery, fetch
  by `_id`, current operations.
- `kv_set` for the Redis agent: write a string, hash, set, sorted set, list or
  stream entry with optional expiry, behind the usual approval, which shows the
  exact commands.
- The agent can hand you a file: a query result, a set of Redis keys or a
  collection, as CSV or JSON, offered as a card in the chat. Exported query
  results are not row-capped.
- The agent can stop a runaway session, Redis client or MongoDB operation and
  create an index, each behind an approval naming the target. Destructive DDL
  stays unavailable.
- MongoDB collections show their JSON-schema validator where one is declared, and
  the collection list reports storage size.
- The work area holds any number of panes, as columns, rows or any nesting of the
  two, in the SQL, Redis and MongoDB shells. Dragging a tab shows exactly where
  it would land, including the first split of an undivided work area.
- Pane keys: ⌘\ splits right, ⌘⇧\ splits down (both repeatable), ⌥⌘\ cycles
  focus, ⌥⌘W closes a pane into its neighbour, ⌘⇧↩ zooms a pane, ⌥⌘0 resets every
  divider to even shares.
- The AI composer offers whichever on/off session settings the agent exposes as
  switches, including fast mode on Claude Code.
- Hold Alt and every panel you can type or navigate in shows the single key that
  jumps to it, in all three shells (Redis had no keyboard focus movement at all
  before). Hint keys are letters from the home row out, so they stay typable on
  every layout. Trigger, hold delay and keys are settings
  (`keymap.focus_overlay`), including turning it off.

### Changed
- The AI composer's model, thinking level and permission mode sit in three equal
  slots on one line, keeping their place from the first frame instead of
  reshuffling as the agent reports what it supports.
- Context usage is a ring, amber past three quarters and red past nine tenths,
  replacing the strip of token counts below the composer. The full breakdown and
  session cost are on its tooltip.
- The prompt box starts four lines tall and grows with what you type, up to
  eight, before it scrolls.
- ⌘\ splits the focused pane rather than toggling a fixed two-pane split;
  existing `keymap.toml` bindings keep working. Pane widths are held as
  proportions, so resizing the window widens every pane in step.
- F6 and ⇧F6 cycle every panel on screen, in every shell, instead of the SQL
  shell's three fixed stops.
- ⌥⌘1 / ⌥⌘2 / ⌥⌘3 are retired in favour of holding Alt. A `keymap.toml` binding
  them still loads, and those keys now cycle focus. The command palette lists
  every panel it can focus by name.

### Fixed
- Fixed the bug where ⌘W stopped closing tabs, along with most other shortcuts,
  until the window was clicked. Collapsing a panel, closing a pane or a focus
  shortcut on Redis could leave focus pointing at something no longer on screen;
  focus is now checked every frame and re-anchored to a real panel.
- The breadcrumb's database picker belongs to its own pane. In a split it used to
  open in every pane at once and then do nothing, since each duplicate menu
  dismissed the others' click; each crumb now names and changes its own pane's
  database.
- Each pane keeps its own tab-strip scroll position and editor/result divider,
  instead of sharing both across a split.
- Superseding a Postgres query reliably cancels it at the server, including a
  cancel arriving between the statement being prepared and being run.
- Importing from DBeaver, DBGate and DataGrip offers your Redis and MongoDB
  entries, not just the SQL ones, including entries stored as a single connection
  string, which now arrive filled in rather than as empty stubs.
- A Redis glob or prefix search keeps scanning until it finds your keys, instead
  of reporting "No keys match this filter" when they sit beyond the first scan
  window. Combining a pattern with a TTL, favourite or tag filter no longer lets
  unmatched keys leak in from the second page on.
- Assistant answers no longer run off the right edge of the panel: list items,
  plan steps, tool-call errors and long chips all wrap or ellipsize inside it.

## [0.20.0] - 2026-07-30

### Added
- Server panel listing live database sessions, with lock waits and their
  blockers marked and a guarded cancel/terminate per session; opens from the
  status-bar connection chip. The ClickHouse mutations list now lives here too.
- Show DDL on any schema-tree object, in a read-only highlighted tab with Copy
  and "Open as query".
- Schema comparison: diff the current schema against another, with type-aware
  column matching and additive reconciling DDL generated into a query tab.
- Connection health report for SQL engines: sizes, unused indexes, missing
  keys and other per-engine checks, saved per connection.
- Watch mode: re-run a tab's read-only query every few seconds, with changed
  cells flashing, a row-count delta, and the scroll position kept.
- The schema tree now shows materialized views, functions, procedures,
  triggers, sequences and user-defined types, loaded lazily per group.
- In-grid editing on ClickHouse, with per-change safety checks and honest
  per-statement outcomes instead of a claimed all-or-nothing success.
- The ClickHouse submit confirmation shows the real statements and matched row
  counts; a change matching several rows needs explicit approval.
- Mutations panel for ClickHouse listing background work still applying after
  a submit, with per-row cancel and a status-bar indicator.
- Tables with a composite primary key are now editable on PostgreSQL, MySQL
  and SQLite.
- Bulk import into a ClickHouse table from the result toolbar.
- Search across every setting, with each row showing its `settings.toml` key,
  a modified marker, and a per-setting Reset.
- Settings pages for Redis and MongoDB.
- Stored routines can be written in the editor: a `BEGIN … END` body runs as
  the one statement it is, and `DELIMITER $$` is honoured.
- Groundwork for translating the interface: a Language setting under
  Appearance (English-only for now), with per-string fallback to English.

### Changed
- The app is now called RED, in capitals, everywhere it names itself; saved
  connections, settings and passwords carry over unchanged.
- Settings are reorganised by topic: `[grid]` is now `[data]`, `[redis]` is
  `[kv]`, and `[query]` split into `[sql]` and `[safety]`; existing files
  migrate automatically.
- Row density, page size and maximum cell size now apply to the Redis key
  browser and MongoDB document grid as well as SQL results.
- The reference settings file documents every section again.
- Editing affordances follow the table: a table the engine cannot modify shows
  a one-line reason instead of edit controls, and computed columns are never
  offered for editing.
- The AI assistant no longer claims a multi-statement changeset always commits
  together or not at all, which was untrue on ClickHouse.

### Fixed
- Writes and DDL run from the editor now resolve unqualified names in the
  tab's database, fixing MySQL's "No database selected" on `CREATE TRIGGER`.
- On MySQL 8, a query no longer leaves its database bound to the pooled
  connection for whatever runs next.
- Views, triggers, functions and procedures can be edited from the tab showing
  their definition, through the usual confirmation and read-only lock.
- Clicking a trigger, routine, sequence or type in the schema tree opens its
  definition, the way clicking a table opens its rows.
- Creating a trigger, routine, sequence or type now refreshes the schema tree
  immediately instead of only after a reconnect.
- The editor no longer underlines trigger and routine syntax as unknown
  columns, and highlights those words as keywords.
- Staging more than six new rows no longer hides the ones past the sixth: the
  change-set zone below the grid scrolls through the whole set.

## [0.19.0] - 2026-07-28

### Added
- Database context for MySQL and ClickHouse connections, which browse a whole
  server rather than one database. The editor breadcrumb reads
  `connection / database / table`, where the database is a picker showing what an
  unqualified `FROM users` resolves against; browsing a table from the tree moves
  the tab into that table's database. The target is per tab, so a split view can
  compare two databases over one connection.
- Right-click menu in the schema tree: browse a table, open a query bound to a
  namespace, set the active database, and copy a plain or qualified name.
- Connections can be marked Local, Dev, Staging, or Prod, and the write guards
  follow the marker. Local asks about nothing short of destroying an object, so a
  scratch database stays out of your way; Prod confirms from the first risky
  statement, makes every confirmation typed rather than clicked, and offers no
  "Don't ask again" at all. Unmarked connections behave exactly as before. The
  marker shows as a coloured badge on the connection card and beside the
  connection name, and the connection form guesses one from the host and name.
- Optional second opinion from your AI agent on a statement a confirmation has
  stopped, for the mistakes a keyword check cannot see: an inverted filter, a
  value that reads wrong against the schema, a join that fans out. Off by
  default, since enabling it sends the statement and a schema summary to the
  configured provider. It is advice and nothing more: it runs nothing, reads no
  rows, and can never unlock or shorten a confirmation, so a slow or mistaken
  model costs you a line of text rather than a table. Settings → Query → Safety.
- The confirmation for a destructive statement now asks the database how much it
  will touch: "This affects 8,412 rows", or "orders holds 8,412 rows" before a
  `DROP TABLE`. The count runs over the same table and `WHERE` clause the
  statement uses, so a predicate that matches more than you expected shows up
  before the run rather than after. It never holds up the dialog: it fills in a
  moment later, gives up after two seconds, and says plainly when it can't tell.

### Changed
- Statements are graded before they run, and the confirmation matches how
  dangerous they actually are. A filtered `UPDATE ... WHERE id = 42` or an
  `ALTER TABLE ... ADD COLUMN` runs uninterrupted; an `UPDATE` or `DELETE` with
  no `WHERE`, a privilege change, or a `MERGE` stops and says what it noticed
  ("No WHERE clause: this removes every row in orders"); a `DROP TABLE` or
  `TRUNCATE` asks you to type the object's name before the run button enables.
  `GRANT`/`REVOKE`, `CREATE USER`, stored-procedure calls,
  `ALTER TABLE ... DROP COLUMN`, and a `WITH` query whose CTE writes are now
  caught too, where they used to run unasked.
- "Don't ask again" now silences only the kind of statement in front of you, so
  hiding the routine prompts no longer hides the one before a `DROP DATABASE`.
  The `query.confirm_destructive` setting is replaced by `query.confirm_from`,
  which takes `write`, `risky`, `critical`, or `never` and is migrated on first
  launch; `confirm_destructive = false` becomes `critical` rather than `never`,
  since it used to be the only way to stop being asked about ordinary writes.

### Fixed
- The Tables group in the schema tree could not be collapsed: it reopened itself
  on the next repaint. Its default expansion is now applied once when the schema
  loads, so collapsing it sticks, including across a refresh.
- An object group with nothing in it (Triggers on a server with no triggers) is
  no longer shown at all. It used to appear like any other group, and clicking it
  was the only way to find out it was empty. RED now counts the routines,
  triggers, sequences and types of every namespace in a single query when it
  connects, so a group you can see always has something in it, and its count is
  on the row before you open it.
- Connecting crashed the app outright, on every engine. Building the workspace
  for a new connection tried to read the application's own settings back through
  a handle it was already holding, which aborts the process rather than failing
  gracefully. The setting is now passed in by the caller, and a test fails the
  build if that pattern reappears anywhere.
- A multi-statement script run from the editor (a `CREATE TABLE` followed by its
  indexes and seed rows, say) executed only its first statement and reported
  success on SQLite, and failed with a bare engine error elsewhere. The whole
  script now runs as one transaction, and the toast reports how many statements
  committed.
- An unqualified query on a MySQL connection that named no database reported the
  server's bare "No database selected" with no way to act on it. The results pane
  now explains what happened and lists the databases on the server, so picking
  one sets the target and re-runs the query.
- ClickHouse connections that named no database silently resolved unqualified
  queries against `default`, even while browsing another database, which could
  return a same-named table's rows rather than an error. The database is now
  explicit and selectable.

## [0.18.0] - 2026-07-20

### Added
- MongoDB support: connect to a `mongodb://` or `mongodb+srv://` deployment and
  explore it in a dedicated document shell. A database → collection tree lists
  collections with estimated counts and view / time-series badges; selecting one
  streams its documents into a continuously scrolling grid (a column per
  top-level field) that fetches a window at a time and never loads a collection
  whole. An extended-JSON find filter narrows the view, and the inspector shows a
  document as pretty-printed extended JSON that preserves BSON types (ObjectId,
  dates, decimals, binary).
- MongoDB analysis panels: a Schema panel samples documents to show each field's
  path, type distribution ("string 82% / int 18%"), and how often it is present;
  an Indexes panel lists keys and unique / sparse / ttl / partial properties; a
  Query panel runs an aggregation pipeline into a results grid; and Explain
  reports the winning plan, the index used, and documents examined / returned.
- MongoDB editing (writable connections): edit a document with a field-by-field
  Form editor — editable names, a searchable type picker, collapsible nested
  objects and arrays, `_id` kept read-only — or a raw extended-JSON editor.
  Insert, delete, clone, and drop are available; destructive operations sit
  behind an explicit confirm and every write is refused on a read-only
  connection.
- MongoDB workspace: a tabbed workspace like the SQL and Redis browsers.
  Collections open in reorderable, pinnable tabs (the same collection can open
  several times, each with its own filter and edits), a split view shows two at
  once (⌘\), and Table / List / JSON render modes switch how documents display.
  Standard window chrome — footer status bar, collapsible sidebar (⌘B), docked
  AI panel — plus full keyboard and vim (hjkl, g/G, Ctrl-d/Ctrl-u) navigation.
- MongoDB AI assistant (⌘L): grounded in the connection, it inspects the
  deployment, profiles schemas and type drift, samples and queries documents,
  runs aggregations, and explains queries to flag missing indexes; at the write
  tier it proposes document, index, or collection operations to approve behind
  the same gates as the manual path.
- Searchable, grouped History dock: the left History panel (SQL and Redis shells)
  gains a live search box and collapsible grouped sections — SQL by Today /
  Yesterday / Earlier, Redis by "Recently viewed keys" and "Commands" — each
  showing its row count and force-expanding on a match.
- Compare tables (data diff): a new "table: compare against…" command reports
  which rows are added, removed, or changed between two tables, aligned by the
  left table's primary key, as a full-screen read-only report (summary, filter,
  changed cells shown old → new). Both tables are read key-ordered and streamed,
  so neither loads whole; it never writes.
- `red mcp <connection>`: a headless stdio MCP server. Point Claude Code (or any
  MCP client) at `red mcp my-connection` for Red's read-only database tools
  (schema, describe, profile, SELECT, explain) grounded in that connection, with
  no GUI and no ports. Writes are withheld and a tool-call budget bounds a
  runaway client, like the in-app MCP path.
- Redis batch console: a Line / Batch toggle adds a multi-line composer that runs
  many commands at once with per-command output, a live "running N / M" readout,
  and a Stop button. A destructive command is confirmed once up front rather than
  per line, and each still passes the read-only and destructive gates. Load a
  `.redis`/`.txt` file or save the buffer back out.
- Connect through a proxy: a new "Connect via proxy" section (network engines)
  reaches a database via a SOCKS5 or HTTP CONNECT proxy, with optional auth whose
  password is stored in the OS keychain. A connection uses either a proxy or an
  SSH tunnel, not both.
- Import connections from more tools: alongside DBeaver and DBGate, the import
  wizard now reads JetBrains DataGrip / IntelliJ (`dataSources.xml`),
  RedisInsight, and plain credential files (`~/.pgpass`, `~/.my.cnf`,
  `~/.pg_service.conf`). Passwords are imported when recoverable and otherwise
  flagged for re-entry, never silently dropped.
- Vim navigation: an optional keymap setting adds `hjkl`, `g`/`G`, `0`/`$`, and
  `Ctrl-d`/`Ctrl-u` motions to the result grid, schema tree, and history dock,
  alongside the arrow keys. Off by default; applies live.
- Remove all RED data: a "Remove all RED data" action (Settings → Behavior, the
  palette, or `red reset`) deletes RED's config and cached-data directories and
  every keychain secret (connection passwords, SSH keys, AI keys) in one step. It
  shows what will be removed, is irreversible, and leaves the binary untouched.

### Changed
- Delete/destructive confirmations are unified across the SQL, Redis, and MongoDB
  shells under one setting. Deleting a Redis key or a MongoDB document now asks
  first like a destructive SQL statement, and every confirm dialog carries a
  "Don't ask again" checkbox (Settings → Query → "Confirm destructive
  operations", on by default, turns it back on). The MongoDB drop-collection
  confirm stays server-gated and always asks.

## [0.17.0] - 2026-07-14

### Added
- Engine icons: the welcome screen's saved-connection cards and the connection
  form's engine picker now show each engine's own brand logo (PostgreSQL,
  SQLite, MySQL, ClickHouse, Redis) in place of the generic database glyph. On
  the welcome cards the logo takes the connection's own accent colour; in the
  engine picker (trigger and every dropdown option) it takes the engine's
  colour.
- Welcome screen: the saved-connection list now paginates (8 per page) with
  Previous / Next controls, so a large roster stays a single screen instead of
  one long scroll. 
- Tab strip: middle-click a tab to close it, right-click for a context menu
  (Close, Close Others, Close All, Close Left, Close Right, Pin tab), and pin a
  tab to keep it visible at the start of the strip no matter how far you've
  scrolled. The close-with-unsaved-work prompt gained a "Don't ask again"
  checkbox.
- Redis key browser: an Actions dropdown in the toolbar — Refresh keys (also
  ⌘/Ctrl+R), Find biggest keys, Import keys, and Expand all / Collapse all for
  the namespace tree.
- Redis key browser: the filter bar is now a single combined search field —
  the query-mode picker (Glob / Prefix / Exact / Fuzzy / Value) sits inside the
  input as one control instead of a separate dropdown beside it.
- Redis key browser: a TTL filter dropdown beside the type filter narrows the
  visible keys by remaining expiry — Permanent (no TTL), ending in ≤ 3 minutes,
  or under an hour / day / week, or a week or more. It filters the loaded keys,
  so pair it with a prefix or type filter on very large keyspaces.
- Redis key browser: filter the list to favourites only (a star toggle in the
  toolbar) or to a single tag (a tag dropdown that appears once any key is
  tagged). Both compose with the other filters.
- Redis value inspector: a star button in the preview header favourites /
  unfavourites the open key in place, without going back to the list's
  right-click menu.
- Redis key browser: auto-refresh is now a dedicated toolbar button — click to
  turn periodic re-scanning on/off (shown accent-tinted with its interval while
  live), and use its caret to pick the interval (Off / 2s / 5s / 10s / 30s). A
  new `[redis] auto_refresh_secs` setting (Settings → Behavior) sets the interval
  new browse tabs start at.
- Redis key browser: Import keys (Actions → Import keys…) — choose a text file of
  Redis commands (one per line, e.g. `SET user:1 alice`; blank lines and `#`
  comments ignored) and run them in order against the current database, with a
  summary of how many succeeded/failed. Disabled on read-only connections.

### Changed
- Redis key browser: the value-preview panel is now resizable — drag the divider
  between the key list and the preview to set its width (matching the SQL detail
  inspector).
- Redis key browser: the filter bar's fuzzy and value-search toggles are
  replaced by a single query-mode dropdown at the head of the bar — Glob (`*`),
  Prefix, Exact, Fuzzy, and Value. Prefix matches keys starting with the typed
  text; Exact jumps straight to one key by name (no scan); Glob/Fuzzy/Value keep
  their old behaviour. The placeholder and result count follow the mode. Every
  mode now filters as you type (debounced) — pressing Enter is an optional
  accelerator, no longer required for Exact or Value lookups.
- Redis value inspector: in a narrow preview pane, the format-lens row
  (Auto / Raw / JSON / Hex / MsgPack / …) now scrolls horizontally instead of
  clipping the trailing lenses off-panel, and deleting a key is confirmed in a
  centred modal rather than an inline banner whose buttons could overflow.
- Redis key browser: the browse toolbar is decluttered — the keyspace size
  ("~N keys") moved to the status bar at the bottom (shown only while browsing,
  and always the stable unfiltered count), "Find biggest keys" moved into the
  Actions dropdown, and the toolbar's dropdown menus no longer open off-screen
  near the window edge.
- Redis "New key": now a centred modal with a labelled, form-friendly layout and
  a segmented type picker (String · Hash · List · Set · ZSet · Stream). The
  fields adapt to the chosen type — a string gains an optional expiry (TTL), a
  hash/stream a field, a sorted set a score, and a list a Head/Tail push choice.
- AI assistant: pick which agent runs a chat from the panel itself. When more
  than one agent is set up, the agent name in the panel header becomes a dropdown
  (switch the current chat's agent), and the "+" button opens a "New chat with
  <agent>" menu so you choose the agent up front; the command palette gained the
  same "agent: new chat with <name>" entries. A new chat starts on whichever
  agent you last used. Settings → AI is now purely account management — sign in
  or add API keys per agent; choosing the active agent no longer lives there.
- Connection form: the host may now be left blank; it falls back to `localhost`
  (as `psql` and `redis-cli` do) instead of being rejected.
- Redis connections now carry a red badge (matching the app's accent) while
  keeping the "Redis" label.

## [0.16.0] - 2026-07-10

### Added
- ER diagram: a read-only, pannable/zoomable map of the schema - every table a
  box (columns marked PK/FK), every foreign key a connector. Open it from the
  schema panel's diagram button, the Query menu, or the command palette
  (`schema: ER diagram`). Drag boxes to arrange, scroll to pan, ⌘/Ctrl+scroll to
  zoom, Fit to frame it all; double-click a table to browse it.
- Format SQL: beautify the editor's query in place (⌥⌘F, the Query menu, or the
  command palette) - re-indents, upper-cases keywords, and puts each clause on
  its own line.
- Export a result as SQL `INSERT` statements (Export -> SQL). The table name comes
  from the file you save to.
- Import a `.json` file that holds a single top-level array of objects, not only
  newline-delimited JSON.

### Changed
- Schema tree: a single click now acts on the row.
- Query history: click an entry to open it in a new query tab; ⌘/Ctrl-click to
  replace the current tab's editor instead. Nothing runs until you do, so a past
  write is never re-executed by a click.

### Fixed
- Result grid: a cell holding multi-line text (embedded newlines, tabs, or other
  control characters) now shows its beginning on a single line, instead of a
  vertically-centered slice from the middle of the value. The full text is still
  available by copying the cell or opening it in the detail inspector.

## [0.15.0] - 2026-07-09

### Added
- Duplicate a saved connection from the welcome screen.
- Edit the connection you're currently using straight from the connection
  switcher.
- A scrollbar for the SQL editor.

### Changed
- A more compact welcome screen: smaller header, tighter spacing, and the
  import / bug-report links folded into one footer line.
- Importing saved connections from other database tools is now a wizard: pick a
  source, then tick exactly which connections to bring in from a dense checklist.
  Only tools actually found on this machine are offered, and installs kept inside
  a Flatpak sandbox are found too.
- The selected connection on the welcome screen is outlined in its own colour.
- Reports the AI assistant generates now stay in the chat as a card with an
  "Open" button, instead of flashing open in your browser on their own. Open
  them when you want; the card persists with the conversation.

### Fixed
- Windows: clicks on the toolbar controls (the Settings gear, the connection
  switcher) no longer get swallowed by window dragging. Dragging the window
  works as before.
- Autocomplete suggestions scroll now: arrow past the ones on screen, or use
  the mouse wheel to preview the rest.
- Wide values in the row-number column are no longer clipped.
- Right-click menus in the result grid close when you click anywhere outside
  them, instead of lingering over other windows.
- Square corners no longer show inside the settings panel's rounded frame.
- Linux: the app shows its icon in the GNOME app switcher and dock.
- Linux: the window no longer draws its own rounded corners.

## [0.13.0] - 2026-07-03

### Added
- Follow foreign keys from the grid: jump to the referenced row, list the rows
  that reference this one, or open a row as an expandable relation tree.
- Expand a foreign-key column in place to see the referenced table's columns
  alongside your result.
- Query history that persists across restarts, in its own panel next to the
  schema.
- Import data into a table from CSV or JSONL files.
- Column stats at a glance: count, distinct, nulls, min/max, sum/avg for the
  selected column.
- Split view: work in two query tabs side by side (⌘\).
- Copy a result or table into another table - in the same database or across
  connections.
- Migrate tables into a new database, with foreign keys, indexes, and
  auto-increment settings carried over.
- Import your saved connections from DBeaver or DBGate.
- A command-line mode for scripting: run queries, copy tables, and manage
  connections without opening the app.
- A What's New panel, and a heads-up toast after RED updates itself.
- Quick actions on the result grid.

## [0.12.0] - 2026-06-24

### Added
- Find in results and in the editor (⌘F).
- A small sample database on first launch, so you can try RED without setting
  anything up.
- Easier Claude sign-in for the AI assistant, with the signed-in account shown
  in Settings.

### Fixed
- Linux: the window now has a proper titlebar - move, resize, minimize, and
  close work on desktops that don't draw one themselves.
- Editing JSON and other typed cells now works reliably, and inline editing in
  the inspector is seamless.
- Cleaner, more readable notifications.
- Security fixes.

## [0.11.0] - 2026-06-23

### Changed
- Smarter SQL autocompletion.

## [0.10.2] - 2026-06-21

### Fixed
- Settings and keyboard shortcuts on Windows.

## [0.10.1] - 2026-06-21

### Fixed
- Windows and Linux downloads.

## [0.10.0] - 2026-06-21

### Added
- ClickHouse support (read-only).

## [0.9.0] - 2026-06-21

### Added
- RED now runs on Linux and Windows, and keeps itself up to date there too.

## [0.8.0] - 2026-06-21

### Added
- AI assistant (⌘L): chat about your schema and data using the Claude API or
  your Claude subscription. You approve every tool it uses, conversations are
  saved, and it can draw chart reports.
- Optionally let the assistant change data on a specific connection - every
  statement still needs your approval.
- Connecting through an unknown SSH host now shows its fingerprint and offers
  "Trust & retry".

### Fixed
- Read-only connections are enforced more strictly.

## [0.7.0] - 2026-06-19

### Added
- SSH tunneling: connect to databases behind a jump host, with password, key,
  or agent authentication.

## [0.6.0] - 2026-06-18

### Added
- Edit data right in the grid: change cells, add and delete rows, review the
  staged changes, then submit them together or revert.

## [0.5.5] - 2026-06-14

### Added
- Custom keyboard shortcuts (`keymap.toml`, applied live).

### Fixed
- Editing a saved connection no longer loses its password; SQL editor
  shortcuts on macOS.

## [0.5.4] - 2026-06-13

Maintenance release.

## [0.5.2] - 2026-06-13

### Fixed
- SQL editor fixes.

## [0.5.1] - 2026-06-13

### Fixed
- Stability fixes.

## [0.5.0] - 2026-06-13

### Added
- The settings panel is fully keyboard-accessible.

### Fixed
- Table and settings scrolling.

## [0.4.0] - 2026-06-13

### Changed
- A refreshed welcome screen and accessibility improvements.

## [0.3.0] - 2026-06-13

### Fixed
- Tab cycling and switching back to recent connections.

## [0.2.0] - 2026-06-13

### Added
- Edit a cell's value, with a confirmation before the change runs.
- RED updates itself on macOS.

## [0.1.3] - 2026-06-13

### Added
- Saved queries: keep snippets and reopen them from the palette.

## [0.1.1] - 2026-06-13

### Added
- Filter a result to a `WHERE` clause without rewriting the query (⌘⇧F).

## [0.1.0] - 2026-06-13

The first release: explore a schema, run SQL, browse large tables, and export.

### Added
- Connect to SQLite, PostgreSQL, and MySQL/MariaDB; passwords live in your
  system keychain, never in a plain file.
- Schema explorer, SQL editor with schema-aware completion, and query tabs.
- Browse huge tables smoothly - rows stream in as you scroll, so even
  million-row results stay fast and light on memory.
- Export results to CSV or JSON.
- Safety rails: read-only connections, query timeouts, cancellable queries,
  and a confirmation before destructive statements.
- Command palette (⌘K) and full keyboard operability.
- Cell/row detail inspector (⌘I).
- Keep several connections open and switch instantly (⌘P).
- Themes and font settings.
- Native macOS menu bar; signed and notarized macOS builds.
