# Vendored report assets

`report-charts.js` is the trusted bundle injected into AI-generated reports that
contain interactive charts. It is **Chart.js v4 (UMD, minified)** concatenated
with `report-renderer.js` (our small renderer). It is the only executable code in
a report — it runs under the report's CSP nonce; the model's HTML and chart specs
never execute. The report CSP also sets `connect-src 'none'`, so the bundle has no
network egress.

This file is checked in deliberately (it is not produced by `cargo build`). To
regenerate after a Chart.js bump:

```sh
cd "$(mktemp -d)" && npm init -y && npm install chart.js@4
# then, from the repo:
cat <chart.js>/dist/chart.umd.min.js report-renderer.js > report-charts.js
```

Keep `report-renderer.js` (the source of the renderer half) in version control
alongside this README so the bundle can be rebuilt; the concatenated
`report-charts.js` is what the crate `include_str!`s.
