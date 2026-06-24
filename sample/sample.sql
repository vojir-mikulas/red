-- Sample database for Red — a small, self-contained e-commerce schema used as
-- the "Sample database" preview that ships with the app (see crates/red/src/sample.rs).
--
-- Regenerate the binary with:
--   rm -f sample/sample.db && sqlite3 sample/sample.db < sample/sample.sql
--
-- It is deliberately tiny (a few thousand rows) so it loads instantly, yet has
-- foreign keys, a view, mixed column types, and some NULLs — enough to show off
-- the schema explorer, joins, filtering, and the result grid.

PRAGMA foreign_keys = ON;

-- ── Schema ───────────────────────────────────────────────────────────────────

CREATE TABLE categories (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE customers (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    email       TEXT NOT NULL,
    country     TEXT,                 -- nullable on purpose (some rows are NULL)
    signup_date TEXT NOT NULL         -- ISO date
);

CREATE TABLE products (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    category_id INTEGER NOT NULL REFERENCES categories(id),
    price       REAL NOT NULL,
    stock       INTEGER NOT NULL
);

CREATE TABLE orders (
    id          INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    status      TEXT NOT NULL,        -- paid | shipped | pending | refunded
    ordered_at  TEXT NOT NULL,        -- ISO timestamp
    total       REAL NOT NULL DEFAULT 0
);

CREATE TABLE order_items (
    id         INTEGER PRIMARY KEY,
    order_id   INTEGER NOT NULL REFERENCES orders(id),
    product_id INTEGER NOT NULL REFERENCES products(id),
    quantity   INTEGER NOT NULL,
    unit_price REAL NOT NULL
);

CREATE INDEX idx_orders_customer ON orders(customer_id);
CREATE INDEX idx_items_order ON order_items(order_id);
CREATE INDEX idx_items_product ON order_items(product_id);

-- ── Seed data (deterministic, generated with recursive CTEs) ──────────────────

INSERT INTO categories(id, name) VALUES
    (1, 'Books'), (2, 'Electronics'), (3, 'Home & Kitchen'), (4, 'Toys'),
    (5, 'Clothing'), (6, 'Sports'), (7, 'Garden'), (8, 'Office');

-- 150 customers, ~1 in 13 with an unknown (NULL) country.
INSERT INTO customers(id, name, email, country, signup_date)
WITH RECURSIVE seq(n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 150
)
SELECT
    n,
    'Customer ' || n,
    'customer' || n || '@example.com',
    CASE WHEN n % 13 = 0 THEN NULL ELSE
        CASE n % 8
            WHEN 0 THEN 'US' WHEN 1 THEN 'GB' WHEN 2 THEN 'DE' WHEN 3 THEN 'FR'
            WHEN 4 THEN 'CZ' WHEN 5 THEN 'JP' WHEN 6 THEN 'BR' ELSE 'CA'
        END
    END,
    date('2023-01-01', '+' || (n * 2) || ' days')
FROM seq;

-- 60 products across the 8 categories.
INSERT INTO products(id, name, category_id, price, stock)
WITH RECURSIVE seq(n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 60
)
SELECT
    n,
    'Product ' || printf('%03d', n),
    (n % 8) + 1,
    round(4.99 + ((n * 37) % 200), 2),
    (n * 7) % 500
FROM seq;

-- 600 orders, totals filled in after the line items exist.
INSERT INTO orders(id, customer_id, status, ordered_at, total)
WITH RECURSIVE seq(n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 600
)
SELECT
    n,
    (n % 150) + 1,
    CASE n % 4
        WHEN 0 THEN 'paid' WHEN 1 THEN 'shipped' WHEN 2 THEN 'pending' ELSE 'refunded'
    END,
    datetime('2024-01-01', '+' || (n * 7) || ' hours'),
    0
FROM seq;

-- ~3 line items per order (1800 total), priced from the product catalogue.
INSERT INTO order_items(order_id, product_id, quantity, unit_price)
WITH RECURSIVE seq(n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1800
),
items AS (
    SELECT
        ((n - 1) / 3) + 1 AS order_id,
        ((n - 1) % 3) + 1 AS slot
    FROM seq
)
SELECT
    i.order_id,
    p.id,
    ((i.order_id + i.slot) % 5) + 1,
    p.price
FROM items i
JOIN products p ON p.id = ((i.order_id * 7 + i.slot * 13) % 60) + 1;

UPDATE orders SET total = (
    SELECT round(COALESCE(SUM(quantity * unit_price), 0), 2)
    FROM order_items WHERE order_id = orders.id
);

-- A view, so the schema explorer shows more than tables.
CREATE VIEW customer_spend AS
SELECT
    c.id,
    c.name,
    c.country,
    COUNT(DISTINCT o.id)                AS orders,
    round(COALESCE(SUM(o.total), 0), 2) AS lifetime_value
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
GROUP BY c.id, c.name, c.country;
