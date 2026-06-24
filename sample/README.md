# Sample database

`sample.db` is the small, read-only SQLite database Red seeds on first launch as
the **Sample database** preview, so a new user can explore the app without
configuring a connection. It's embedded into the binary at build time
(`crates/red/src/sample.rs`) and written to the app data directory on first run.

It's a tiny e-commerce schema — `customers`, `categories`, `products`, `orders`,
`order_items`, and a `customer_spend` view — with foreign keys, mixed column
types, and some NULLs: enough to show off the schema explorer, joins, filtering,
and the result grid, while staying small enough to load instantly.

## Regenerate

`sample.db` is generated from `sample.sql` (the canonical, reviewable source):

```sh
rm -f sample/sample.db && sqlite3 sample/sample.db < sample/sample.sql
```

The seed is deterministic, so regenerating produces the same data every time.
