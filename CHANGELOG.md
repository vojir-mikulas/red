# Changelog

All notable changes to RED are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Server panel for SQL connections, listing what the database is doing right now:
  every session with its user, database, state, elapsed time, and statement,
  longest-running first. Sessions waiting on a lock are marked, the ones blocking
  them are marked too, and a summary line names how many are stuck behind how
  few. A session can be stopped either by cancelling its statement or by
  terminating it outright, behind a confirmation that names who and what is being
  stopped; RED never offers to stop its own connection, never offers either on a
  read-only connection, and never exposes stopping a session to the AI assistant.
  The ClickHouse mutations list now lives in this panel as a second view rather
  than a dock of its own. It opens from the connection itself: click
  `user@host:port` in the status bar, where the green dot is. Background work
  still running shows its count on that same chip, so an edit that outlived its
  submit stays visible with the panel shut.
- Show DDL, on any object in the schema tree. Tables, views, routines, triggers,
  sequences and types open their definition in a read-only tab with the SQL
  highlighted, plus Copy and "Open as query", which pastes the text into an
  ordinary tab rather than running it. Three engines answer with their own `SHOW
  CREATE` verbatim; PostgreSQL, which has no such statement, gets a definition
  assembled from the catalog that states in a header comment exactly what it does
  and does not cover.
- Schema comparison. Compare the current schema against another and get what
  differs: objects on one side only, and per table the columns, indexes and
  foreign keys added, removed or changed. Columns are compared by type rather
  than by spelling, so a `varchar(255)` and a `character varying(255)` are
  recognised as the same column across engines, and anything RED's type model
  cannot classify is flagged as uncertain instead of asserted. The reconciling
  DDL can be generated into a query tab; it is additive by default, comments out
  every destructive statement unless you ask for them, and never runs itself.
- Connection health report, the SQL counterpart to the Redis keyspace analysis:
  what is big, what is unused, and what is about to hurt. Table and index sizes,
  indexes that have never been used, foreign keys with no supporting index,
  tables with no primary key, vacuum lag and sequential-scan-heavy tables on
  PostgreSQL; sizes, missing keys, MyISAM tables and unused or redundant indexes
  on MySQL; parts-per-partition on ClickHouse; free pages on SQLite. Checks that
  cannot run on your server are listed as such rather than silently skipped, so
  an empty findings list means the checks ran. The report is saved per connection
  and survives a restart, and each finding's suggested fix is text to copy, never
  something RED runs for you.
- Watch mode for a result. Re-run this tab's query every few seconds, with the
  cells that changed flashing, a row-count delta, and the scroll position kept.
  It only ever re-runs read-only queries, pauses when the window is in the
  background or the tab has unsaved edits, skips a tick while the previous run is
  still going, stops itself after three consecutive failures, and floors its
  interval higher on connections tagged as production. Configurable under
  `[sql] watch_default_secs` / `watch_min_secs`.
- The schema tree now shows materialized views, functions, procedures, triggers,
  sequences and user-defined types, not just tables and views, grouped by kind
  under each namespace. The programmatic kinds load when their group is expanded
  rather than at connect, so connecting to a server with many schemas is exactly
  as fast as before.
- In-grid editing on ClickHouse. New rows can be added and submitted like on any
  other engine, and existing rows can be updated and deleted under a contract
  that matches what an OLAP engine can actually promise: each change is checked
  against the rows it currently matches before it runs, is applied on its own
  rather than inside a transaction, and reports its own outcome, so a submit says
  "3 of 5 changes applied" and names what stopped the rest instead of claiming a
  success it can't back. Changes that can't run safely, such as editing a
  sorting-key column or a row that has since changed, are refused before the
  confirmation rather than by the engine afterwards.
- The confirmation for a ClickHouse submit shows the statements that will really
  run, how many rows each currently matches, and the three things worth knowing
  before agreeing: the writes are asynchronous, they are not one transaction, and
  the engine rewrites data by part, so a one-cell edit can cost far more than the
  row it changes. Where a change matches several rows -- normal on an engine with
  no unique row identity -- it is refused until you explicitly say to apply it to
  all of them.
- Mutations panel for ClickHouse connections, listing the background work the
  engine is still applying after a submit returns, with the parts remaining, any
  failure reason, and a per-row cancel. The status bar tints while anything is
  running, so an edit that outlived its submit stays visible without the panel
  open.
- Tables with a composite primary key are now editable on PostgreSQL, MySQL and
  SQLite. Rows are addressed by the whole key rather than a single column, so
  join tables and other multi-column keys no longer browse as read-only.
- Bulk import into a ClickHouse table from the result toolbar, alongside the new
  draft-row insert.
- Search across every setting, in the settings panel's sidebar. Typing a name
  finds the setting wherever it is filed, so finding one no longer means guessing
  which category it was put under. Each row also shows the key you would edit in
  `settings.toml`, marks itself when it differs from the shipped default, and
  offers a Reset that puts just that one back.
- Settings pages for Redis and MongoDB. Redis gains the filter mode new browse
  tabs start in, how many keys one tab keeps in memory, and how much of a list or
  stream the inspector pulls at a time; MongoDB gains the view a collection opens
  in (Table, List, or JSON) and how many columns the table samples. MongoDB had
  no settings at all before, and Redis had a single row filed under Behavior.
- Stored routines can be written in the editor. A `CREATE TRIGGER`, `PROCEDURE`,
  `FUNCTION` or `EVENT` whose body is a `BEGIN … END` block now runs as the one
  statement it is, instead of being cut at the first `;` inside the body and
  bounced back as a syntax error. `DELIMITER $$` is honoured too, being a client
  directive rather than SQL, so a script pasted from documentation runs as written
  instead of reaching a server that has never heard of it. The caret's statement,
  the gutter run markers, and the confirmation all treat the whole body as one
  statement.

### Changed
- The app is now called RED, in capitals, everywhere it names itself: the
  welcome screen, the macOS menu bar, the window title, and the About page. The
  application itself is renamed to match, so the icon in the Dock or the
  applications menu reads RED. Saved connections, settings, and stored passwords
  are untouched and carry over as they are.
- Settings are now organised by what a setting is about rather than by which
  engine came first. Grid settings apply to every grid, and the confirmation
  rules that already governed Redis and MongoDB deletes now live under Safety
  instead of under Query. In the file, `[grid]` is now `[data]`, `[redis]` is
  `[kv]`, and `[query]` has split into `[sql]` and `[safety]`. Existing settings
  files are migrated on load and saved in the new shape, so nothing needs
  changing by hand.
- Row density, page size, and the maximum cell size now apply to the Redis key
  browser and the MongoDB document grid as well as to SQL results. Those two
  previously ignored your settings and used fixed values.
- The reference settings file shipped with RED documents every section again.
  Automatic updates, vim navigation, and the Redis settings had been missing from
  it entirely.
- Editing affordances now follow the table, not just the connection: a table the
  engine cannot modify, such as a ClickHouse `Memory` table or a view, shows a
  one-line reason under the grid instead of edit controls that would fail on use.
  Columns the engine computes for itself are shown as computed in a new row and
  never offered for editing.
- The AI assistant no longer describes a multi-statement changeset as always
  committing together or not at all, which was untrue on ClickHouse. It now says
  which engines roll back and warns that on ClickHouse the statements before a
  failure may have applied.

### Fixed
- A write or DDL statement run from the editor ignored the database picked for the
  tab, so on a MySQL connection that dialled no database anything naming no
  database of its own — a `CREATE TRIGGER`, most visibly — failed with "no
  Database selected" while the `SELECT` beside it worked. Writes now resolve
  unqualified names in the same database reads do.
- On MySQL 8, a query run against one database could leave the connection there
  for whatever ran next, because the server does not restore a pooled
  connection's original database when it is reused. A tab left on the connection's
  own database could therefore resolve unqualified names in another tab's
  database. Every statement now binds the database it means, and a connection that
  never switches database pays nothing for it.
- A definition can now be edited from the tab that shows it. Views, triggers,
  functions and procedures — the objects a database replaces wholesale rather than
  alters — get an Edit button beside Copy, which unlocks the definition and
  pre-fills the drop that has to precede re-creating it. Apply runs the buffer, so
  what executes is what you read, through the same confirmation and read-only lock
  as a typed statement, and the tab then re-reads the object so it shows what the
  server stored. Tables are deliberately not editable here: changing one means
  `ALTER`, which is a different feature.
- Clicking a trigger, routine, sequence or type in the schema tree now opens its
  definition, the way clicking a table opens its rows. Those rows previously did
  nothing at all on a click, leaving the definition reachable only by right-click.
  Their menu also no longer offers "New query here", which belongs to a table and
  says nothing about a trigger.
- Creating an object no longer leaves the schema tree showing the old one. A write
  refreshed only the tables and views, so a newly created trigger, routine,
  sequence or type stayed invisible until reconnect — and the first one of its kind
  in a database doubly so, since a group whose count still said zero is not drawn
  at all. A write now refreshes the tree the same way ⌘R does.
- The editor no longer underlines a trigger or stored-routine definition as if its
  syntax were unknown columns. `BEFORE`, `FOR EACH ROW`, and the trigger's own
  name were each flagged as a column of whichever table the body happened to read.
  Trigger and routine words are also highlighted as keywords now.
- Staging more than six new rows in the grid no longer hides the ones past the
  sixth. The zone below the grid stops growing there so it never eats the results,
  but it now scrolls through the whole change-set, with a scrollbar when there is
  more than fits, and adding or tabbing into a row brings that row into view.

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
