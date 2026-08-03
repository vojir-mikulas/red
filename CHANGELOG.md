# Changelog

All notable changes to RED are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- The Server panel now answers the same three questions on every engine RED
  connects to: what the server is using, what it is doing, and who is connected.
  A new Overview reads live metrics - memory against its ceiling, throughput,
  connections against `max_connections`, replication lag, uptime - drawn as bars
  you can read at a glance instead of numbers you have to compare, and coloured
  only where a real ceiling is close. Counters that only make sense as a rate
  (statements run, transactions committed, commands processed) show the rate,
  derived from the previous refresh. Redis fills it from `INFO`, which RED has
  parsed for the assistant for a while but never showed to a human; Postgres from
  `pg_stat_database` and `pg_stat_activity`, including the oldest open
  transaction, which is the number that actually predicts an outage; MySQL from
  `SHOW GLOBAL STATUS`; ClickHouse from its system tables; MongoDB from
  `serverStatus`. Whatever your role is not allowed to see is listed as such,
  because a metric quietly left out reads as a zero.
- MongoDB gets a server UI for the first time: `$currentOp` in the Sessions view,
  and a runaway operation can be stopped from the panel through the same confirm
  that guards a Postgres terminate. Redis clients appear there too, so
  `CLIENT LIST` and `CLIENT KILL` are no longer only in the Monitor tab. Sessions
  are ordered longest-running first and capped, so a busy server does not push the
  one operation you are looking for off the list. RED never offers to kill its own
  connection, and a read-only connection is offered nothing at all.
- The Server panel can auto-refresh, off by default, on an interval shown in the
  panel itself rather than buried in settings, with a floor of two seconds -
  polling `pg_stat_activity` or `CLIENT LIST` against production is real load, so
  it is opt-in and visible. The default for new connections is
  `behavior.server_refresh_secs`.
- Any query result can now be exported as an Excel workbook. It streams like
  every other export, so a result too large to hold in memory is still too large
  to hold in memory, and it stops at Excel's 1,048,576-row limit with the toast
  saying so rather than quietly dropping the rest. The care is in the cells: a
  number is a number and a numeric-looking *string* stays a string, so an account
  number keeps its leading zero and a long id keeps its last digits instead of
  being rounded off by a spreadsheet. Nulls are empty cells, not the word "NULL",
  and a stray control character in a text column is escaped rather than producing
  a file that opens as "unreadable content". Nothing is compressed, so the file
  is larger than the CSV of the same data. The assistant can write one too.
- Redis can finally export keys, not just import them. "Export keys…" sits under
  the browse Actions menu and "Export key…" on any key's right-click, with a
  choice of scope (the keys shown, everything matching the current filter, or the
  whole database) and three formats. Commands (`.redis`) is the default and the
  exact inverse of the import that already shipped - readable, version-agnostic,
  and re-importable in one click; JSON is for feeding another tool or attaching
  to a ticket; DUMP (`.rdbdump`) is byte-exact and the only one that carries
  binary values, with its version caveat on the dialog rather than buried. Every
  format streams: a million-element set is written as repeated commands and never
  assembled, and Cancel leaves no partial file. Expiry times are written as
  absolute deadlines, so importing an hour later does not silently extend every
  TTL. A value the text formats genuinely cannot carry is skipped and *reported*,
  pointing you at the format that would carry it, rather than being written
  mangled - and importing a file RED exported now reconstructs values containing
  newlines, tabs and other escapes byte for byte. RED recognises a `.rdbdump`
  file on its own, so there is no import format to pick wrong.
- RedisJSON documents are now first-class. A `JSON.*` key used to appear in the
  keyspace with an unreadable `ReJSON-RL` type and no value at all; it now reads
  as `json`, filters as its own type in the browse toolbar, and opens in a
  document tree you navigate rather than receive. That distinction is the point:
  a small document is fetched whole, but a large one is walked a level at a
  time, so opening a 200 MB document costs the same as opening a small one and
  RED never pulls one into memory just to show you a preview. A big array pages
  in place like a list. You can edit a single node - the safe default, since it
  leaves the parts of the document you did not look at alone - or the whole
  document from the Raw view, with the JSON validated before it is sent so a
  typo fails in RED pointing at the character rather than as a bare error from
  the server. "New key" can create a JSON document, node deletion rides the same
  confirmation as every other destructive Redis action, and a read-only
  connection refuses both. Everything module-specific is offered only where the
  server actually has RedisJSON loaded, detected once when you connect. The
  assistant learns the same two moves: it can map a document's shape (paths and
  types, no values) and read any single path, so it can answer "what is in these
  documents" without downloading any of them, and write one node with your
  approval.
- You can point the agent at things instead of describing them. Drag a table, a
  column, a schema or a query tab onto the assistant panel and it becomes a chip
  on your next message; right-click anything in the schema tree and pick "Ask AI
  about this" for the same result without the drag. Selected rows in the grid
  have their own entry too. The agent then gets the object's real definition -
  the same text `describe_table` would have returned, not a paraphrase - so
  "the one with the unit price, no wait, `public.order_items`" stops being how a
  question starts. References resolve when you hit Enter, not when you drop, so
  a tab you edit in between sends what is actually there; one that is gone, or a
  table that has been dropped, says so rather than failing the turn. Structure
  works at any access tier; row data still needs the read tier, and says so at
  the point you ask rather than silently arriving empty.
- You can now attach files to a chat. Drop them anywhere on the assistant panel,
  or use the `+` beside the composer: a CSV the vendor sent, a screenshot of the
  dashboard that shows the wrong number, a PDF spec, a stack trace from prod.
  Text files ride as fenced blocks, images and PDFs as themselves, and every
  attachment shows as a chip you can remove before sending. Nothing is guessed
  at: an unsupported type or an oversized file is refused in the composer, with
  the reason and what to do instead - a CSV over the limit points you at
  importing it into a table and asking about that, which is both better and
  something RED already does (there is an "import" action on the chip for
  exactly that). Files are read off the UI thread at send time, so attaching a
  20 MB PDF does not stall the window. Saved chats remember what was attached by
  name and size, never by content. On the subscription path images go to agents
  that accept them and are described by name to those that do not, rather than
  disappearing. Attachments are treated as data throughout: the agent is told to
  read them as information to analyse and never as instructions, and every write
  still needs your approval.
- Long investigations no longer end in an error. The assistant's reply length is
  now a setting (Settings → AI, 16K by default, up to 64K) instead of a fixed
  8K ceiling, and a reply that still hits it is asked to carry on from where it
  stopped rather than failing the turn - so a long answer or a generated report
  arrives whole. A turn may take twice as many tool steps as before, and running
  out of them now ends with the agent summarising what it found instead of a
  trace that simply stops mid-investigation and settles as though it were
  finished. On top of that the conversation is kept inside the model's context
  window: old tool results the agent has already drawn its conclusions from are
  cleared, and the conversation is summarised as it fills, both by the model
  provider rather than by guesswork here. Where that is not available RED trims
  locally and says so in the activity trace, because context lost silently is
  how an agent starts contradicting itself for no visible reason.
- The usage gauge in the assistant now works on the API-key path too, not just
  the subscription one, and reads the whole conversation rather than the last
  exchange. It fills as the chat fills, turns amber at three fifths - early
  enough to finish your thought - and red at nine tenths, and its tooltip shows
  what the chat has cost so far. A model whose context window RED does not
  recognise shows the token count rather than a percentage it would have had to
  invent.
- The assistant can now read through a large result instead of stopping at the
  first page. A query that returns more than fits comes back with a cursor the
  agent continues, window by window, through the same streaming machinery the
  result grid uses - so the windows tile the result exactly, with no rows
  repeated and none skipped, and without re-running the query. Previously the
  read was truncated and whatever fell off the end was simply gone, which left
  the agent either reasoning over the first page as though it were the data or
  hand-rolling `OFFSET` paging (slow, and silently wrong whenever the ordering
  is not total). Windows are bounded by size rather than row count, so a table
  with a wide text column returns fewer, larger rows. Open cursors are capped,
  closed when you stop a turn or close the chat, and expire on their own after
  five minutes.
- Every answer the assistant gives now shows its sources. A turn that read data
  gets a numbered "Sources" line above the prose, one chip per query, and the
  agent is asked to mark the figures it read with the matching `[3]`. Clicking a
  chip rings the exact call in the activity trace; hovering it shows the tool, its
  arguments and what came back, without leaving the paragraph. The point is what
  becomes visible at a glance: "this paragraph cites three queries" and "this
  paragraph cites nothing" used to render identically. It is labelled Sources, not
  Verified, and deliberately so - a citation shows you where a number came from,
  not that the sentence around it is right.
- The assistant now checks the *shape* of a query before it believes the number
  it got back. The most common way an answer about a database is wrong is a join
  that quietly multiplies rows: the query runs, nothing errors, and the total is
  three times too big. RED now reads the query for the structures that cause
  that (an aggregate over a join with no DISTINCT, a join with no predicate, a
  join on something other than equality, `SELECT *` across a join) and, when it
  finds one, runs one extra pair of counts to see whether the join *actually*
  fanned out. If it did, the agent is told so with the real numbers before it
  writes its answer, so it corrects itself against what executed rather than
  against a second reading of its own SQL. Flagged queries are marked in the
  activity timeline. Nothing is blocked: a cross join is sometimes exactly what
  you meant.
- A chat can now run the agent's writes inside a single transaction that you
  commit or roll back at the end, instead of approving each statement as it
  comes. Turn on "review" in the composer before the first message; every write
  that turn runs against a real open transaction, the agent's own reads see its
  uncommitted changes (so it can check its work), and nothing is durable until
  you answer. At the end you get the list of what it did with the rows each
  statement touched, and two buttons. Rolling back is free, which is the point:
  five separately reasonable approvals can still add up to the wrong change, and
  until now clicking Allow was final. Available on PostgreSQL, MySQL/MariaDB and
  SQLite; ClickHouse has no multi-statement transaction, and the option says so
  rather than quietly falling back. An unanswered review rolls itself back after
  two minutes (configurable), because an open transaction holds locks.
- The approval prompt for an agent write now tells you what it would actually
  do. Instead of asking you to mentally run a `WHERE` clause against a database
  you cannot see, it counts the rows first: "Affects 4,213 of 812,004 rows in
  public.orders", with three of them shown so you can tell at a glance whether
  they are the ones you meant. A statement that matches **no** rows is called out
  as a warning rather than a reassuring small number, because that almost always
  means the predicate is wrong, and the agent is told so too. Each statement of a
  multi-statement changeset gets its own count. The counting runs on a short
  budget and never delays or blocks the prompt: if it cannot finish, the prompt
  still appears and says the count was unavailable rather than showing a zero.
  Turn it off under Settings → AI agent → Safety if you are on a slow link.
- The assistant can now read what you have already written. It searches the
  statements you ran against this connection for the tables and concepts it is
  about to query, so it picks up the join path people actually use (often not the
  one the foreign keys declare), the date column you filter on, and the values
  your status columns really hold, instead of inferring all of it from column
  names. It also reads your saved-query library, so when you ask about a metric
  you have already defined it uses your definition rather than inventing a fourth
  one. Same on Redis, where it reads the commands you have run and the keys you
  have been opening. All of it is scoped to the connection at hand, and nothing
  the agent runs itself is ever recorded, so it is reading your work, not its own.
- Each connection can now carry a knowledge file: plain markdown where you write
  down what the schema cannot tell an agent, such as what "active customer"
  actually means, which join path is the real one, that amounts are in cents, or
  which table not to count. It rides in the assistant's prompt for every chat on
  that connection, so the agent stops re-deriving your business logic and stops
  guessing at it. Open it from the assistant panel's header, or from
  `connection: database knowledge…` in the command palette; the panel shows a
  chip when one is in play. If you would rather not start from a blank page,
  "Learn this database" has the agent explore the connection and draft one for
  you, which opens for review rather than being saved: it inferred it from
  structure and sampling, so it will be right about shape and wrong about
  intent. Works on SQL databases, Redis, and MongoDB alike.
- The AI assistant can now reach the parts of RED it was previously blind to.
  Against a SQL database it can read the foreign-key graph in one call, pull an
  object's real definition, search a table for a value without knowing which
  column holds it, run EXPLAIN with actual row counts, produce a health report,
  see what the server is running right now, compare two schemas or two tables'
  rows, and suggest an index. Against Redis it can infer the keyspace's key
  templates, page deep into a large collection or stream, read consumer-group
  lag and pending entries, list connected clients, and see the topology up
  front. Against MongoDB it can discover which fields reference which
  collections and how reliably they resolve, fetch a document by `_id`, and see
  what is running now.
- The Redis agent can finally write a value. Previously it could delete a
  thousand keys after one approval but could not set a single one; it now has a
  `kv_set` that writes a string, hash, set, sorted set, list, or stream entry,
  with an optional expiry, behind the same approval every other write rides.
  The approval shows the exact commands that will run.
- The assistant can hand you a file: a query's whole result, a set of Redis
  keys, or a collection's documents, written out as CSV or JSON and offered as
  a card in the chat you can open. Unlike what it reads into the conversation,
  an exported query result is not row-capped.
- The assistant can stop a runaway session, Redis client, or MongoDB operation,
  and create an index, each behind an explicit approval that names the target
  and what stopping it costs. Destructive DDL stays unavailable.
- MongoDB collections now show their JSON-schema validator, where one is
  declared, alongside the inferred schema and indexes, and the collection list
  reports each collection's storage size.
- The work area can now hold any number of panes, arranged as columns, rows, or
  any nesting of the two, in the SQL, Redis and MongoDB shells alike. Dragging a
  tab shows where it would land — into the pane under the cursor, or into a new
  pane on whichever edge you are nearest — and dropping it there creates that
  pane, including the very first split of an undivided work area. Nothing
  highlights where nothing would happen, and nothing happens where the highlight
  said it would not, so a drop is never offered and then quietly ignored or
  turned into something else.
- Panes from the keyboard: ⌘\ splits the focused pane to the right and ⌘⇧\
  splits it downward (both repeatable), ⌥⌘\ cycles focus, ⌥⌘W closes a pane and
  folds its tabs into its neighbour, ⌘⇧↩ zooms a pane to fill the work area, and
  ⌥⌘0 resets every divider to even shares.
- The AI assistant's composer now offers whichever on/off settings the agent
  exposes for the session as switches — on Claude Code that includes fast mode,
  the higher-throughput decode available on the models that support it.
- Hold Alt on its own and every panel you can type or navigate in shows the
  single key that jumps to it; press the key to go there, or let go to leave
  focus where it was. It reaches everything the shell is showing — the schema or
  collection tree, each pane's editor and result grid, the Redis key list, the
  history dock, the filter and find bars, the assistant — and works the same in
  the SQL, Redis and MongoDB shells, which is new: Redis previously had no way
  to move focus by keyboard at all. The digits come first, so a plain window
  labels the sidebar `1`, the editor `2` and the grid `3`. The trigger and the
  hold delay are settings (`keymap.focus_overlay`), including turning it off.

### Changed
- The AI assistant's composer settings now read as a set: model, thinking level
  and permission mode sit in three equal slots on one line, keeping their place
  from the first frame instead of resizing and reshuffling as the agent reports
  what it supports. Each names itself on hover; a setting the agent has not
  offered yet shows as an inert slot rather than appearing out of nowhere once
  the session is up.
- Context usage in the AI assistant is now a ring showing how full the context
  window is, amber past three quarters and red past nine tenths, in place of the
  strip of token counts below the composer. The full breakdown — tokens in
  context, cached and out, and the running session cost — is on its tooltip.
- The AI assistant's prompt box starts four lines tall and grows with what you
  type, up to eight, before it starts scrolling. Wrapped lines count, so a long
  paragraph opens the box up the same way pressing Return does.
- ⌘\ now splits the focused pane rather than toggling a fixed two-pane split;
  existing `keymap.toml` files that bind it keep working unchanged. Pane widths
  are held as proportions, so resizing the window now widens every pane in step
  instead of stretching only the last one.
- F6 and ⇧F6 now cycle through every panel on screen — docks, trees, editors,
  grids, the open filter or find bar, the assistant — instead of the SQL shell's
  three fixed stops, and they work in the Redis and MongoDB shells too.
- The ⌥⌘1 / ⌥⌘2 / ⌥⌘3 jumps to the schema, editor and grid are retired in favour
  of holding Alt, which reaches far more than three places and works in every
  shell. A `keymap.toml` that binds them still loads, and those keys now cycle
  focus rather than doing nothing. The command palette lists every panel it can
  focus by name, so nothing needs a shortcut to be reachable.

### Fixed
- Fixed the bug where ⌘W stopped closing tabs — along with most other shortcuts
  — until the window was clicked. Collapsing a panel that held the keyboard,
  closing a pane, or pressing a focus shortcut on a Redis connection could leave
  focus pointing at something no longer on screen, and RED then matched none of
  its own shortcuts. The tell was that ⌘K still opened the palette while ⌘W, ⌘T,
  ⌘B, ⌘Y, ⌘I, ⌘L and Esc all did nothing. Focus is now checked every frame and
  handed to a real panel if it has come adrift, so the keyboard cannot be lost.
- The breadcrumb's database picker now belongs to the pane it sits in. In a
  split it used to open in every visible pane at once, and picking a database
  then did nothing at all — each of the duplicate menus dismissed the click
  meant for the others. Each pane's crumb also names that pane's own database
  rather than the focused pane's, and picking one changes the half you picked
  it in.
- Each pane now keeps its own tab-strip scroll position and editor/result
  divider; previously the two halves of a split shared both, so scrolling one
  strip scrolled the other.
- Superseding a Postgres query now reliably stops it at the server. A cancel
  that arrived in the moment between the statement being prepared and being run
  was discarded by the engine, so a flung scrollbar or a re-sort could leave the
  old query scanning to completion on the server while RED had already moved on.
- Importing connections from DBeaver, DBGate, and DataGrip now offers your Redis
  and MongoDB entries as well as the SQL ones. They were listed as "unsupported
  engine" and skipped, even though RED speaks both. Connections the source tool
  stores as a single connection string - the default for DBGate's Mongo and Redis
  plugins, and DBeaver's URL mode - now arrive with their host, port, database
  number, credentials and TLS setting filled in rather than as an empty stub.
- A Redis glob or prefix search keeps scanning until it finds your keys. On a
  large keyspace a selective pattern such as `user:*` would report "No keys
  match this filter" whenever the matching keys sat beyond the first bounded
  scan window, and with nothing on screen there was no scrolling left to pull
  the rest. The search now walks on in the background until it has a screenful
  of matches or the keyspace runs out, exactly as fuzzy search already did.
  Combining a pattern with a TTL, favourite or tag filter also no longer lets
  unmatched keys leak into the list from the second page on.
- Assistant answers no longer run off the right edge of the panel. A bulleted or
  numbered list wrapped only as far as the first line and then kept going past
  the panel, so the ends of the agent's notes were simply unreadable; the same
  applied to a long plan step, a failed tool call's error line in the activity
  trace, and a chip carrying a long filename or table name. All of them now wrap
  or ellipsize inside the panel.

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
