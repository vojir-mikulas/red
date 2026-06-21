/*!
 * Red report chart renderer — runs after the vendored Chart.js UMD bundle.
 *
 * Trust model: this is the ONLY executable code in a report (it carries the
 * report's CSP nonce; the model's HTML, chart specs, datasets, and filter defs do
 * not). It reads the model-authored payload from an inert `application/json` block
 * and builds interactive charts, filterable tables, and a Grafana-style filter bar
 * from it. The payload is pure DATA — Chart.js never evals it, and every table
 * cell is written via `textContent` (never innerHTML), so a data value cannot
 * inject HTML. The report CSP forbids all network egress (`connect-src 'none'`),
 * so nothing here can leak the data; ALL filtering/sorting happens client-side over
 * the embedded rows — the report never calls back to the database.
 *
 * Placeholders the model drops in its report body:
 *   <div data-red-chart="N">    → chart N from the `charts` array
 *   <div data-red-table="NAME"> → an interactive table over dataset NAME
 *   <div data-red-filters>      → the report-wide filter bar (auto-inserted at the
 *                                 top if `filters` are declared but no placeholder)
 * Report-wide filters (`filters`: multiselect/select/range/search bound to a
 * dataset column) drive EVERY table and bound chart at once.
 *
 * Look & feel: shadcn-style (muted palette, card containers, subtle grid, rounded
 * bars, light tooltip, clean controls).
 *
 * This is the renderer source. The embedded bundle is `report-charts.js` =
 * Chart.js UMD min + this file (see README.md).
 */
(function () {
  "use strict";

  // Every Chart instance we create, so we can re-fit them after layout / when the
  // tab becomes visible (guards against blank canvases from a 0px-at-creation
  // container — see resizeAllCharts).
  var CHARTS = [];

  // Hard cap on DOM rows we materialize for a table's filtered view. The grid is
  // not virtualized, so an unbounded <tbody> rebuild on every keystroke can lock
  // up the page on large result sets; we render the first N and footer the rest.
  var TABLE_RENDER_CAP = 500;

  // Trailing debounce — coalesces rapid filter keystrokes into one re-render.
  function debounce(fn, ms) {
    var h = null;
    return function () {
      if (h) clearTimeout(h);
      h = setTimeout(function () { h = null; fn(); }, ms);
    };
  }

  var LIGHT = {
    fg: "#18181b",
    muted: "#71717a",
    grid: "rgba(24,24,27,0.07)",
    border: "#e4e4e7",
    card: "#ffffff",
    tipBg: "#ffffff",
    tipBorder: "#e4e4e7",
    shadow: "0 1px 2px rgba(24,24,27,0.05)",
    hover: "rgba(24,24,27,0.04)",
    ring: "rgba(232,112,79,0.25)",
    palette: ["#e8704f", "#29a297", "#2d4f5e", "#e6c265", "#f3a05e"],
  };
  var DARK = {
    fg: "#fafafa",
    muted: "#a1a1aa",
    grid: "rgba(250,250,250,0.08)",
    border: "#262a31",
    card: "#14171c",
    tipBg: "#18181b",
    tipBorder: "#27272a",
    shadow: "0 1px 2px rgba(0,0,0,0.4)",
    hover: "rgba(250,250,250,0.06)",
    ring: "rgba(90,141,238,0.30)",
    palette: ["#5a8dee", "#2eb888", "#e58a33", "#ad53d6", "#e23670"],
  };

  function isDark() {
    return !!(
      window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches
    );
  }

  function hexToRgba(hex, a) {
    if (typeof hex !== "string" || hex.charAt(0) !== "#") return hex;
    var h = hex.slice(1);
    if (h.length === 3) h = h[0] + h[0] + h[1] + h[1] + h[2] + h[2];
    var n = parseInt(h, 16);
    return "rgba(" + ((n >> 16) & 255) + "," + ((n >> 8) & 255) + "," + (n & 255) + "," + a + ")";
  }

  // Apply alpha to a color whether it's hex (#rgb/#rrggbb), hsl(...) or rgb(...)
  // — theme palettes arrive as hsl() strings, so hexToRgba alone isn't enough.
  function withAlpha(c, a) {
    if (typeof c !== "string") return c;
    if (c.charAt(0) === "#") return hexToRgba(c, a);
    var m = c.match(/^hsla?\(([^)]*)\)$/i);
    if (m) return "hsl(" + m[1].split("/")[0].trim() + " / " + a + ")";
    var r = c.match(/^rgba?\(([^)]*)\)$/i);
    if (r) return "rgb(" + r[1].split("/")[0].trim() + " / " + a + ")";
    return c;
  }

  function readPayload() {
    var node = document.getElementById("red-report-data");
    if (!node) return { charts: [], data: {}, filters: [], theme: null };
    try {
      var p = JSON.parse(node.textContent || "{}");
      return {
        charts: Array.isArray(p.charts) ? p.charts : [],
        data: p.data && typeof p.data === "object" ? p.data : {},
        filters: Array.isArray(p.filters) ? p.filters : [],
        theme: p.theme && typeof p.theme === "object" ? p.theme : null,
      };
    } catch (e) {
      return { charts: [], data: {}, filters: [], theme: null };
    }
  }

  // Map the app theme (CSS color strings from Red) onto the renderer's token shape.
  // Falls back to the built-in light/dark when no theme was supplied.
  function themeFrom(pt) {
    if (!pt) return isDark() ? DARK : LIGHT;
    var base = pt.is_dark ? DARK : LIGHT;
    return {
      fg: pt.fg || base.fg,
      muted: pt.muted || base.muted,
      grid: pt.grid || base.grid,
      border: pt.border || base.border,
      card: pt.surface || base.card,
      tipBg: pt.surface || base.tipBg,
      tipBorder: pt.border || base.tipBorder,
      shadow: pt.is_dark ? DARK.shadow : LIGHT.shadow,
      hover: pt.hover || base.hover,
      ring: pt.ring || base.ring,
      palette: pt.palette && pt.palette.length ? pt.palette : base.palette,
    };
  }

  // ---- styling ---------------------------------------------------------------

  function injectStyles(t) {
    var css =
      ".red-view{margin:16px 0}" +
      ".red-toolbar{display:flex;align-items:center;gap:10px;margin-bottom:10px;flex-wrap:wrap}" +
      ".red-search{width:240px;max-width:60%;padding:7px 10px;border:1px solid " + t.border + ";border-radius:8px;background:" + t.card + ";color:" + t.fg + ";font:inherit;font-size:13px}" +
      ".red-search:focus,.red-colfilter:focus,.red-dd:focus,.red-rangein:focus{outline:none;border-color:" + t.palette[0] + ";box-shadow:0 0 0 3px " + t.ring + "}" +
      ".red-count{font-size:12px;color:" + t.muted + ";font-variant-numeric:tabular-nums}" +
      ".red-clear{margin-left:auto;padding:6px 10px;border:1px solid " + t.border + ";border-radius:8px;background:transparent;color:" + t.muted + ";font:inherit;font-size:12px;cursor:pointer}" +
      ".red-clear:hover{color:" + t.fg + ";background:" + t.hover + "}" +
      ".red-tblscroll{overflow:auto;border:1px solid " + t.border + ";border-radius:12px;max-height:520px}" +
      ".red-tblscroll table{margin:0}" +
      ".red-tblscroll thead th{position:sticky;top:0;z-index:1}" +
      ".red-th{cursor:pointer;user-select:none;white-space:nowrap}" +
      ".red-th:hover{background:" + t.hover + "}" +
      ".red-th[data-sort=asc]::after{content:' \\25B2';font-size:9px;color:" + t.muted + "}" +
      ".red-th[data-sort=desc]::after{content:' \\25BC';font-size:9px;color:" + t.muted + "}" +
      ".red-filterrow th{padding-top:0;padding-bottom:8px}" +
      ".red-colfilter{width:100%;min-width:80px;padding:4px 6px;border:1px solid " + t.border + ";border-radius:6px;background:" + t.card + ";color:" + t.fg + ";font:inherit;font-size:12px;font-weight:400}" +
      ".red-num{text-align:right;font-variant-numeric:tabular-nums}" +
      // Report-wide filter bar
      ".red-filters{position:sticky;top:0;z-index:5;display:flex;flex-wrap:wrap;align-items:center;gap:14px;padding:12px 14px;margin:0 0 18px;background:" + t.card + ";border:1px solid " + t.border + ";border-radius:12px;box-shadow:" + t.shadow + "}" +
      ".red-filter{display:flex;align-items:center;gap:6px;position:relative}" +
      ".red-filter-label{font-size:12px;font-weight:600;color:" + t.muted + "}" +
      ".red-dd{padding:6px 10px;border:1px solid " + t.border + ";border-radius:8px;background:" + t.card + ";color:" + t.fg + ";font:inherit;font-size:13px;cursor:pointer}" +
      ".red-dd-panel{position:absolute;top:calc(100% + 4px);left:0;z-index:10;min-width:170px;max-height:280px;overflow:auto;padding:6px;background:" + t.card + ";border:1px solid " + t.border + ";border-radius:10px;box-shadow:0 6px 24px rgba(0,0,0,0.18)}" +
      ".red-dd-opt{display:flex;align-items:center;gap:8px;padding:5px 8px;border-radius:6px;font-size:13px;color:" + t.fg + ";cursor:pointer;white-space:nowrap}" +
      ".red-dd-opt:hover{background:" + t.hover + "}" +
      ".red-rangein{width:64px;padding:6px 8px;border:1px solid " + t.border + ";border-radius:8px;background:" + t.card + ";color:" + t.fg + ";font:inherit;font-size:13px}" +
      // Dual-handle range slider (two overlaid native range inputs + a fill).
      ".red-rangewrap{display:flex;align-items:center;gap:8px}" +
      ".red-slider{position:relative;width:150px;height:20px;flex:0 0 auto}" +
      ".red-slider .red-trk{position:absolute;left:0;right:0;top:8px;height:4px;border-radius:2px;background:" + t.border + "}" +
      ".red-slider .red-fill{position:absolute;top:8px;height:4px;border-radius:2px;background:" + t.palette[0] + "}" +
      ".red-slider input[type=range]{-webkit-appearance:none;appearance:none;position:absolute;left:0;top:0;width:100%;height:20px;margin:0;background:transparent;pointer-events:none}" +
      ".red-slider input[type=range]:focus{outline:none}" +
      ".red-slider input[type=range]::-webkit-slider-runnable-track{height:20px;background:transparent}" +
      ".red-slider input[type=range]::-webkit-slider-thumb{-webkit-appearance:none;appearance:none;height:14px;width:14px;margin-top:3px;border-radius:50%;background:" + t.card + ";border:2px solid " + t.palette[0] + ";box-shadow:" + t.shadow + ";cursor:pointer;pointer-events:auto}" +
      ".red-slider input[type=range]::-moz-range-track{height:4px;background:transparent}" +
      ".red-slider input[type=range]::-moz-range-thumb{height:14px;width:14px;border:2px solid " + t.palette[0] + ";border-radius:50%;background:" + t.card + ";cursor:pointer;pointer-events:auto}" +
      ".red-dd-search{width:100%;margin-bottom:6px;padding:5px 8px;border:1px solid " + t.border + ";border-radius:6px;background:" + t.card + ";color:" + t.fg + ";font:inherit;font-size:12px}";
    var el = document.createElement("style");
    el.textContent = css;
    document.head.appendChild(el);
  }

  // ---- chart theming ---------------------------------------------------------

  function applyTheme(t) {
    if (typeof Chart === "undefined" || !Chart.defaults) return;
    var D = Chart.defaults;
    try {
      D.set({
        maintainAspectRatio: false,
        responsive: true,
        color: t.muted,
        borderColor: t.border,
        animation: { duration: 600 },
      });
      D.set("font", {
        family:
          "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif",
        size: 12,
      });
      D.set("layout", { padding: 8 });
      D.set("plugins.legend", {
        position: "bottom",
        labels: {
          usePointStyle: true,
          pointStyle: "circle",
          boxWidth: 8,
          boxHeight: 8,
          padding: 16,
          color: t.fg,
        },
      });
      D.set("plugins.title", {
        color: t.fg,
        font: { size: 14, weight: "600" },
        padding: { top: 2, bottom: 12 },
      });
      D.set("plugins.tooltip", {
        backgroundColor: t.tipBg,
        titleColor: t.fg,
        bodyColor: t.fg,
        borderColor: t.tipBorder,
        borderWidth: 1,
        cornerRadius: 8,
        padding: 10,
        boxPadding: 6,
        caretSize: 5,
        usePointStyle: true,
        titleFont: { size: 12, weight: "600" },
        bodyFont: { size: 12 },
        titleMarginBottom: 6,
      });
      D.set("scale", {
        ticks: { color: t.muted, padding: 8 },
        border: { display: false },
        grid: { color: t.grid, drawTicks: false },
      });
      D.set("scales.category", { grid: { display: false } });
      D.set("scales.linear", { grid: { color: t.grid, drawTicks: false } });
      D.set("scales.radialLinear", {
        grid: { color: t.grid },
        angleLines: { color: t.grid },
        ticks: { color: t.muted, backdropColor: "transparent" },
      });
      D.set("elements.bar", { borderRadius: 6, maxBarThickness: 48 });
      D.set("elements.line", { tension: 0.35, borderWidth: 2 });
      D.set("elements.point", {
        radius: 0,
        hoverRadius: 5,
        hoverBorderWidth: 2,
        hitRadius: 12,
      });
      D.set("elements.arc", { borderWidth: 2, borderColor: t.card });
      D.set("datasets.doughnut", { cutout: "62%" });
    } catch (e) {
      // best-effort
    }
  }

  function colorize(spec, t) {
    if (!spec.data || !Array.isArray(spec.data.datasets)) return;
    var type = spec.type;
    var arc = type === "pie" || type === "doughnut" || type === "polarArea";
    spec.data.datasets.forEach(function (ds, i) {
      if (!ds || typeof ds !== "object") return;
      var c = t.palette[i % t.palette.length];
      if (arc) {
        if (ds.backgroundColor == null) {
          var n = (ds.data && ds.data.length) || 0,
            arr = [];
          for (var j = 0; j < n; j++)
            arr.push(
              type === "polarArea"
                ? withAlpha(t.palette[j % t.palette.length], 0.65)
                : t.palette[j % t.palette.length]
            );
          ds.backgroundColor = arr;
        }
        if (ds.borderColor == null) ds.borderColor = t.card;
      } else if (type === "line" || type === "radar") {
        if (ds.borderColor == null) ds.borderColor = c;
        if (ds.backgroundColor == null)
          ds.backgroundColor = withAlpha(c, ds.fill ? 0.15 : 0.1);
        if (ds.pointBackgroundColor == null) ds.pointBackgroundColor = c;
        if (ds.pointBorderColor == null) ds.pointBorderColor = c;
      } else if (type === "scatter" || type === "bubble") {
        if (ds.backgroundColor == null) ds.backgroundColor = withAlpha(c, 0.6);
        if (ds.borderColor == null) ds.borderColor = c;
      } else {
        if (ds.backgroundColor == null) ds.backgroundColor = c;
      }
    });
  }

  function cardify(slot, t) {
    var s = slot.style;
    if (!s.height) s.height = "360px";
    if (!s.position) s.position = "relative";
    if (!s.border) s.border = "1px solid " + t.border;
    if (!s.borderRadius) s.borderRadius = "12px";
    if (!s.padding) s.padding = "16px 16px 12px";
    if (!s.background) s.background = t.card;
    if (!s.boxShadow) s.boxShadow = t.shadow;
    if (!s.margin) s.margin = "16px 0";
  }

  // ---- datasets, filtering, sorting ------------------------------------------

  function buildRegistry(data) {
    var reg = {};
    if (!data || typeof data !== "object") return reg;
    Object.keys(data).forEach(function (name) {
      var d = data[name];
      if (!d || typeof d !== "object") return;
      var cols = Array.isArray(d.columns) ? d.columns.map(String) : [];
      var rows = Array.isArray(d.rows) ? d.rows.filter(Array.isArray) : [];
      if (!cols.length && rows.length)
        for (var i = 0; i < rows[0].length; i++) cols.push("col " + (i + 1));
      var ds = {
        columns: cols,
        rows: rows,
        // search/colFilters: per-table; facets: report-wide filter bar (keyed by
        // column index → {type:'set'|'range'|'text', …}); sort: per-table.
        state: { search: "", colFilters: {}, facets: {}, sort: { col: -1, dir: 0 } },
        subs: [],
        // Each report-wide filter control bound to this dataset registers a
        // resetter here, so a table's "Clear filters" can also reset the
        // shared facet controls back to their default (all rows) and re-sync
        // their UI — see renderTable's clear handler and the make* controls.
        facetResets: [],
      };
      ds.notify = function () {
        for (var k = 0; k < ds.subs.length; k++) ds.subs[k]();
      };
      reg[name] = ds;
    });
    return reg;
  }

  // Returns a FINITE number or null. NaN/Infinity are treated as null so they
  // can't poison range-slider domains, aggregates, or chart axis scaling
  // (callers already skip nulls). A raw NaN/Infinity number cell counts as null.
  function toNum(v) {
    if (typeof v === "number") return Number.isFinite(v) ? v : null;
    if (typeof v === "string" && v.trim() !== "") {
      var n = Number(v);
      return Number.isFinite(n) ? n : null;
    }
    return null;
  }

  function fmtLabel(v) {
    return v == null ? "" : String(v);
  }

  function fmtNum(n) {
    return Number.isInteger(n) ? String(n) : String(Math.round(n * 100) / 100);
  }

  function passesFacets(st, r) {
    var facets = st.facets;
    for (var fc in facets) {
      var f = facets[fc];
      var cell = r[+fc];
      if (f.type === "set") {
        if (f.set && !f.set.has(cell == null ? "" : String(cell))) return false;
      } else if (f.type === "range") {
        var n = toNum(cell);
        if (f.min != null && (n == null || n < f.min)) return false;
        if (f.max != null && (n == null || n > f.max)) return false;
      } else if (f.type === "text") {
        if (
          f.text &&
          String(cell == null ? "" : cell).toLowerCase().indexOf(String(f.text).toLowerCase()) === -1
        )
          return false;
      }
    }
    return true;
  }

  function viewRows(ds) {
    var st = ds.state;
    var search = st.search ? st.search.toLowerCase() : "";
    var rows = ds.rows.filter(function (r) {
      if (!passesFacets(st, r)) return false;
      if (search) {
        var hay = "";
        for (var i = 0; i < r.length; i++) hay += (r[i] == null ? "" : r[i]) + "";
        if (hay.toLowerCase().indexOf(search) === -1) return false;
      }
      for (var c in st.colFilters) {
        var f = st.colFilters[c];
        if (!f) continue;
        var v = r[+c] == null ? "" : String(r[+c]);
        if (v.toLowerCase().indexOf(String(f).toLowerCase()) === -1) return false;
      }
      return true;
    });
    var s = st.sort;
    if (s.dir !== 0 && s.col >= 0) {
      rows = rows.slice().sort(function (a, b) {
        var x = a[s.col],
          y = b[s.col],
          nx = toNum(x),
          ny = toNum(y),
          cmp;
        if (nx !== null && ny !== null) cmp = nx - ny;
        else cmp = String(x == null ? "" : x).localeCompare(String(y == null ? "" : y));
        return s.dir * cmp;
      });
    }
    return rows;
  }

  function renderTable(slot, ds, t) {
    slot.classList.add("red-view");

    var bar = document.createElement("div");
    bar.className = "red-toolbar";
    var search = document.createElement("input");
    search.type = "search";
    search.placeholder = "Filter rows…";
    search.className = "red-search";
    var count = document.createElement("span");
    count.className = "red-count";
    var clear = document.createElement("button");
    clear.type = "button";
    clear.className = "red-clear";
    clear.textContent = "Clear filters";
    bar.appendChild(search);
    bar.appendChild(count);
    bar.appendChild(clear);

    var scroll = document.createElement("div");
    scroll.className = "red-tblscroll";
    var tbl = document.createElement("table");
    var thead = document.createElement("thead");
    var htr = document.createElement("tr");
    var ftr = document.createElement("tr");
    ftr.className = "red-filterrow";

    ds.columns.forEach(function (col, ci) {
      var th = document.createElement("th");
      th.className = "red-th";
      th.textContent = col;
      th.addEventListener("click", function () {
        var s = ds.state.sort;
        if (s.col !== ci) {
          s.col = ci;
          s.dir = 1;
        } else if (s.dir === 1) s.dir = -1;
        else if (s.dir === -1) {
          s.dir = 0;
          s.col = -1;
        } else s.dir = 1;
        syncHeaders();
        ds.notify();
      });
      htr.appendChild(th);

      var fth = document.createElement("th");
      var inp = document.createElement("input");
      inp.type = "text";
      inp.className = "red-colfilter";
      inp.placeholder = "—";
      var notifyCol = debounce(function () { ds.notify(); }, 120);
      inp.addEventListener("input", function () {
        ds.state.colFilters[ci] = inp.value;
        notifyCol();
      });
      fth.appendChild(inp);
      ftr.appendChild(fth);
    });

    thead.appendChild(htr);
    thead.appendChild(ftr);
    var tbody = document.createElement("tbody");
    tbl.appendChild(thead);
    tbl.appendChild(tbody);
    scroll.appendChild(tbl);
    slot.appendChild(bar);
    slot.appendChild(scroll);

    function syncHeaders() {
      for (var i = 0; i < htr.children.length; i++) {
        var th = htr.children[i];
        if (ds.state.sort.col === i && ds.state.sort.dir !== 0)
          th.setAttribute("data-sort", ds.state.sort.dir === 1 ? "asc" : "desc");
        else th.removeAttribute("data-sort");
      }
    }

    var notifySearch = debounce(function () { ds.notify(); }, 120);
    search.addEventListener("input", function () {
      ds.state.search = search.value;
      notifySearch();
    });
    clear.addEventListener("click", function () {
      ds.state.search = "";
      ds.state.colFilters = {};
      search.value = "";
      var inputs = ftr.querySelectorAll("input");
      for (var i = 0; i < inputs.length; i++) inputs[i].value = "";
      // Also drop report-wide facet selections and refresh each shared filter
      // control's UI, so the table truly resets to "no filters applied".
      ds.state.facets = {};
      for (var f = 0; f < ds.facetResets.length; f++) ds.facetResets[f]();
      ds.notify();
    });

    ds.subs.push(function () {
      var rows = viewRows(ds);
      tbody.textContent = "";
      var frag = document.createDocumentFragment();
      // No virtualization: cap how many rows we materialize per render so a large
      // filtered view can't lock up the page (small tables are unaffected).
      var shown = rows.length > TABLE_RENDER_CAP ? TABLE_RENDER_CAP : rows.length;
      for (var r = 0; r < shown; r++) {
        var tr = document.createElement("tr");
        for (var c = 0; c < ds.columns.length; c++) {
          var td = document.createElement("td");
          var v = rows[r][c];
          td.textContent = v == null ? "" : String(v);
          if (typeof v === "number") td.className = "red-num";
          tr.appendChild(td);
        }
        frag.appendChild(tr);
      }
      if (rows.length > shown) {
        var ftrTr = document.createElement("tr");
        var ftrTd = document.createElement("td");
        ftrTd.colSpan = ds.columns.length || 1;
        ftrTd.className = "red-count";
        ftrTd.style.textAlign = "center";
        ftrTd.style.padding = "8px";
        ftrTd.textContent =
          "… showing first " + shown + " of " + rows.length + " rows";
        ftrTr.appendChild(ftrTd);
        frag.appendChild(ftrTr);
      }
      tbody.appendChild(frag);
      count.textContent =
        rows.length +
        (rows.length === 1 ? " row" : " rows") +
        (rows.length !== ds.rows.length ? " of " + ds.rows.length : "");
    });
  }

  // ---- charts ----------------------------------------------------------------

  function colIndex(ds, name) {
    if (name == null) return -1;
    if (typeof name === "number") return name;
    return ds.columns.indexOf(name);
  }

  function aggregate(vals, how) {
    if (how === "count") return vals.length;
    if (!vals.length) return null;
    switch (how) {
      case "avg":
        return vals.reduce(function (a, b) { return a + b; }, 0) / vals.length;
      // reduce (not Math.min/max.apply) so a very large group can't blow the
      // argument-count limit and throw a RangeError.
      case "min":
        return vals.reduce(function (a, b) { return b < a ? b : a; }, vals[0]);
      case "max":
        return vals.reduce(function (a, b) { return b > a ? b : a; }, vals[0]);
      default:
        return vals.reduce(function (a, b) { return a + b; }, 0);
    }
  }

  function resolveY(ds, spec, xi) {
    var y = spec.y;
    if (y == null) {
      var out = [];
      for (var c = 0; c < ds.columns.length; c++) if (c !== xi) out.push(c);
      return out;
    }
    if (!Array.isArray(y)) y = [y];
    return y
      .map(function (n) { return colIndex(ds, n); })
      .filter(function (i) { return i >= 0; });
  }

  // Thrown when a chart names a dataset column that doesn't resolve, so the
  // caller can show the "Could not render this chart." fallback (with which
  // column was bad) instead of an empty axes-only canvas.
  function badColumn(name) {
    var e = new Error("unknown column");
    e.redBadColumn = name == null ? "(unnamed)" : String(name);
    return e;
  }

  function buildConfig(ds, spec) {
    var rows = viewRows(ds);
    var type = spec.type || "bar";
    var xi = colIndex(ds, spec.x);
    var opts = spec.options || {};
    // An explicit x that doesn't resolve is a hard error (don't silently fall
    // back to row-index labels). A null/omitted x keeps its index behavior.
    if (spec.x != null && xi === -1) throw badColumn(spec.x);

    if (type === "scatter" || type === "bubble") {
      var yi = colIndex(ds, Array.isArray(spec.y) ? spec.y[0] : spec.y);
      var ri = colIndex(ds, spec.r);
      if (yi === -1) throw badColumn(Array.isArray(spec.y) ? spec.y[0] : spec.y);
      var pts = rows.map(function (r) {
        var p = { x: toNum(r[xi]), y: toNum(r[yi]) };
        if (type === "bubble") p.r = ri >= 0 ? toNum(r[ri]) || 4 : 4;
        return p;
      });
      return {
        type: type,
        data: { datasets: [{ label: ds.columns[yi] || "", data: pts }] },
        options: opts,
      };
    }

    var ycols = resolveY(ds, spec, xi);
    // An explicit y that resolves to no valid column → bad column name.
    if (spec.y != null && !ycols.length)
      throw badColumn(Array.isArray(spec.y) ? spec.y.join(", ") : spec.y);
    var how = spec.aggregate || "none";
    var labels, series;
    if (how === "none") {
      labels = rows.map(function (r, i) {
        return xi >= 0 ? fmtLabel(r[xi]) : String(i + 1);
      });
      series = ycols.map(function (ci) {
        return rows.map(function (r) { return toNum(r[ci]); });
      });
    } else {
      var map = {}, order = [];
      rows.forEach(function (r) {
        var key = xi >= 0 ? fmtLabel(r[xi]) : String(order.length);
        if (!(key in map)) { map[key] = []; order.push(key); }
        map[key].push(r);
      });
      labels = order;
      series = ycols.map(function (ci) {
        return order.map(function (k) {
          var vals = map[k]
            .map(function (r) { return toNum(r[ci]); })
            .filter(function (v) { return v !== null; });
          return aggregate(vals, how);
        });
      });
    }

    var arc = type === "pie" || type === "doughnut" || type === "polarArea";
    var datasets = arc
      ? [{ label: ds.columns[ycols[0]] || "", data: series[0] || [] }]
      : ycols.map(function (ci, k) {
          return { label: ds.columns[ci] || "series " + (k + 1), data: series[k] };
        });
    return { type: type, data: { labels: labels, datasets: datasets }, options: opts };
  }

  function renderChart(slot, spec, t, registry) {
    cardify(slot, t);
    var canvas = document.createElement("canvas");
    slot.appendChild(canvas);

    var bound = spec.dataset && registry[spec.dataset];
    try {
      if (bound) {
        var ds = registry[spec.dataset];
        var cfg = buildConfig(ds, spec);
        colorize(cfg, t);
        var chart = new Chart(canvas, cfg);
        CHARTS.push(chart);
        ds.subs.push(function () {
          var next = buildConfig(ds, spec);
          colorize(next, t);
          chart.data = next.data;
          chart.update();
        });
      } else {
        colorize(spec, t);
        CHARTS.push(new Chart(canvas, spec));
      }
    } catch (e) {
      slot.textContent =
        e && e.redBadColumn
          ? "Could not render this chart. Unknown column: " + e.redBadColumn
          : "Could not render this chart.";
    }
  }

  // Re-fit every chart to its (now laid-out) container. Chart.js sizes a
  // responsive chart from its parent at creation time; if the report is still
  // being laid out — or opened in a background tab — the parent can read as 0px,
  // leaving a blank canvas until something forces a resize. We force it here.
  function resizeAllCharts() {
    for (var i = 0; i < CHARTS.length; i++) {
      try {
        CHARTS[i].resize();
      } catch (e) {
        /* ignore */
      }
    }
  }

  // ---- report-wide filter bar (Grafana-style variables) ----------------------

  function closeAllPanels() {
    var ps = document.querySelectorAll(".red-dd-panel");
    for (var i = 0; i < ps.length; i++) ps[i].style.display = "none";
  }

  // Datasets a filter applies to: those that have the column (optionally narrowed
  // to one via `def.dataset`). A filter with no `dataset` drives every dataset
  // that has a column of that name — so one "Region" control filters all panels.
  function resolveTargets(reg, def) {
    var targets = [];
    Object.keys(reg).forEach(function (name) {
      if (def.dataset && def.dataset !== name) return;
      var ci = reg[name].columns.indexOf(def.column);
      if (ci >= 0) targets.push({ ds: reg[name], ci: ci });
    });
    return targets;
  }

  function distinctFor(targets) {
    var seen = {}, out = [];
    targets.forEach(function (tg) {
      var rows = tg.ds.rows, ci = tg.ci;
      for (var i = 0; i < rows.length; i++) {
        var v = rows[i][ci], s = v == null ? "" : String(v);
        if (!(s in seen)) { seen[s] = 1; out.push(s); }
      }
    });
    var allNum = out.length > 0 && out.every(function (s) {
      return s !== "" && Number.isFinite(Number(s));
    });
    out.sort(
      allNum
        // Guard non-finite (NaN/Infinity) so the comparator never returns NaN.
        ? function (a, b) {
            var na = Number(a), nb = Number(b);
            if (!Number.isFinite(na) || !Number.isFinite(nb))
              return String(a).localeCompare(String(b));
            return na - nb;
          }
        : function (a, b) { return a.localeCompare(b); }
    );
    return out;
  }

  function labelOf(def) {
    return (def.label || def.column) + ":";
  }

  function optRow(text, checked, onToggle) {
    var row = document.createElement("label");
    row.className = "red-dd-opt";
    var input = document.createElement("input");
    input.type = "checkbox";
    input.checked = !!checked;
    input.addEventListener("change", function () { onToggle(input.checked); });
    var span = document.createElement("span");
    span.textContent = text;
    row.appendChild(input);
    row.appendChild(span);
    return { row: row, input: input };
  }

  // Multi-select dropdown (the "show only selected regions" control). All values
  // are selected by default (or `def.default` if given); a row passes when its
  // value is in the selected set, so the default selection shows everything.
  function makeMulti(def, targets) {
    var wrap = document.createElement("div");
    wrap.className = "red-filter";
    var label = document.createElement("span");
    label.className = "red-filter-label";
    label.textContent = labelOf(def);
    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "red-dd";
    var panel = document.createElement("div");
    panel.className = "red-dd-panel";
    panel.style.display = "none";
    panel.addEventListener("click", function (e) { e.stopPropagation(); });

    var values = distinctFor(targets);
    var defaults =
      Array.isArray(def.default) && def.default.length
        ? def.default.map(String)
        : values;
    var selected = {};
    for (var i = 0; i < values.length; i++) selected[values[i]] = false;
    for (var j = 0; j < defaults.length; j++)
      if (defaults[j] in selected) selected[defaults[j]] = true;

    function selSet() {
      var s = new Set();
      values.forEach(function (v) { if (selected[v]) s.add(v); });
      return s;
    }
    function apply() {
      var set = selSet();
      // An empty selection means "no filter applied" (show all rows), not
      // "match nothing". This also covers a `def.default` that matches none of
      // the column's distinct values — without it, such a default would hide
      // every row. passesFacets skips a set facet whose `set` is null.
      var facetSet = set.size ? set : null;
      targets.forEach(function (tg) {
        tg.ds.state.facets[tg.ci] = { type: "set", set: facetSet };
        tg.ds.notify();
      });
      var on = values.filter(function (v) { return selected[v]; }).length;
      btn.textContent =
        on === values.length ? "All" : on === 0 ? "None" : on + " selected";
    }

    // A search box inside the dropdown once there are many options to scan.
    if (values.length > 8) {
      var find = document.createElement("input");
      find.type = "search";
      find.className = "red-dd-search";
      find.placeholder = "Find…";
      find.addEventListener("click", function (e) { e.stopPropagation(); });
      find.addEventListener("input", function () {
        var q = find.value.toLowerCase();
        checks.forEach(function (ch) {
          ch.row.style.display =
            !q || String(ch.v).toLowerCase().indexOf(q) !== -1 ? "" : "none";
        });
      });
      panel.appendChild(find);
    }

    var checks = [];
    var all = optRow("(All)", defaults.length === values.length, function (c) {
      values.forEach(function (v) { selected[v] = c; });
      checks.forEach(function (ch) { ch.input.checked = c; });
      apply();
    });
    panel.appendChild(all.row);
    values.forEach(function (v) {
      var o = optRow(v === "" ? "(empty)" : v, !!selected[v], function (c) {
        selected[v] = c;
        all.input.checked = values.every(function (x) { return selected[x]; });
        apply();
      });
      checks.push({ v: v, input: o.input, row: o.row });
      panel.appendChild(o.row);
    });

    btn.addEventListener("click", function (e) {
      e.stopPropagation();
      var open = panel.style.display !== "none";
      closeAllPanels();
      panel.style.display = open ? "none" : "block";
    });

    // Reset to defaults + re-sync the checkbox UI (used by a table's
    // "Clear filters"); does not re-apply, since the caller resets facets.
    function reset() {
      for (var i = 0; i < values.length; i++) selected[values[i]] = false;
      for (var j = 0; j < defaults.length; j++)
        if (defaults[j] in selected) selected[defaults[j]] = true;
      checks.forEach(function (ch) { ch.input.checked = !!selected[ch.v]; });
      all.input.checked = values.every(function (x) { return selected[x]; });
      var on = values.filter(function (v) { return selected[v]; }).length;
      btn.textContent =
        on === values.length ? "All" : on === 0 ? "None" : on + " selected";
    }
    targets.forEach(function (tg) { tg.ds.facetResets.push(reset); });

    wrap.appendChild(label);
    wrap.appendChild(btn);
    wrap.appendChild(panel);
    apply();
    return wrap;
  }

  function makeSelect(def, targets) {
    var wrap = document.createElement("div");
    wrap.className = "red-filter";
    var label = document.createElement("span");
    label.className = "red-filter-label";
    label.textContent = labelOf(def);
    var sel = document.createElement("select");
    sel.className = "red-dd";
    var values = distinctFor(targets);
    var allSet = new Set(values);
    var ALL = " all";
    var optAll = document.createElement("option");
    optAll.value = ALL;
    optAll.textContent = "(All)";
    sel.appendChild(optAll);
    values.forEach(function (v) {
      var o = document.createElement("option");
      o.value = v;
      o.textContent = v === "" ? "(empty)" : v;
      sel.appendChild(o);
    });
    if (def.default != null && def.default !== "") sel.value = String(def.default);
    function apply() {
      var v = sel.value;
      targets.forEach(function (tg) {
        tg.ds.state.facets[tg.ci] =
          v === ALL ? { type: "set", set: allSet } : { type: "set", set: new Set([v]) };
        tg.ds.notify();
      });
    }
    sel.addEventListener("change", apply);
    // Reset to default (or "All") + re-sync the UI for "Clear filters".
    targets.forEach(function (tg) {
      tg.ds.facetResets.push(function () {
        sel.value =
          def.default != null && def.default !== "" ? String(def.default) : ALL;
      });
    });
    wrap.appendChild(label);
    wrap.appendChild(sel);
    apply();
    return wrap;
  }

  // Numeric range: a dual-handle slider over the column's data domain, kept in
  // sync with min/max number inputs (typing moves the handles and vice-versa).
  function makeRange(def, targets, t) {
    var wrap = document.createElement("div");
    wrap.className = "red-filter";
    var label = document.createElement("span");
    label.className = "red-filter-label";
    label.textContent = labelOf(def);

    var vals = [];
    targets.forEach(function (tg) {
      tg.ds.rows.forEach(function (r) {
        var n = toNum(r[tg.ci]);
        if (n !== null) vals.push(n);
      });
    });
    // reduce (not Math.min/max.apply) — a huge column can't throw RangeError.
    // vals are already finite (toNum drops NaN/Infinity), so the domain is sane.
    var dmin = vals.length
      ? vals.reduce(function (a, b) { return b < a ? b : a; }, vals[0])
      : 0;
    var dmax = vals.length
      ? vals.reduce(function (a, b) { return b > a ? b : a; }, vals[0])
      : 100;
    if (dmin === dmax) dmax = dmin + 1;
    var span = dmax - dmin;
    var intish = vals.length > 0 && vals.every(function (v) { return Number.isInteger(v); });
    // Float step: ~1/1000 of the span so a large domain isn't dragged in coarse
    // span/100 jumps, with a tiny floor to avoid a zero/degenerate step.
    var step = intish ? 1 : Math.max(span / 1000, 1e-9);

    var box = document.createElement("div");
    box.className = "red-rangewrap";
    var loNum = document.createElement("input");
    loNum.type = "number";
    loNum.className = "red-rangein";
    var slider = document.createElement("div");
    slider.className = "red-slider";
    var trk = document.createElement("div");
    trk.className = "red-trk";
    var fill = document.createElement("div");
    fill.className = "red-fill";
    var loR = document.createElement("input");
    loR.type = "range";
    var hiR = document.createElement("input");
    hiR.type = "range";
    [loR, hiR].forEach(function (s) {
      s.min = String(dmin);
      s.max = String(dmax);
      s.step = String(step);
    });
    loR.value = String(dmin);
    hiR.value = String(dmax);
    slider.appendChild(trk);
    slider.appendChild(fill);
    slider.appendChild(loR);
    slider.appendChild(hiR);
    var hiNum = document.createElement("input");
    hiNum.type = "number";
    hiNum.className = "red-rangein";

    var curLo = dmin,
      curHi = dmax;

    function apply() {
      var p1 = ((curLo - dmin) / span) * 100,
        p2 = ((curHi - dmin) / span) * 100;
      fill.style.left = p1 + "%";
      fill.style.width = p2 - p1 + "%";
      loR.value = String(curLo);
      hiR.value = String(curHi);
      loNum.value = fmtNum(curLo);
      hiNum.value = fmtNum(curHi);
      var min = curLo <= dmin ? null : curLo;
      var max = curHi >= dmax ? null : curHi;
      targets.forEach(function (tg) {
        tg.ds.state.facets[tg.ci] = { type: "range", min: min, max: max };
        tg.ds.notify();
      });
    }
    function clamp(v, lo, hi) {
      return v < lo ? lo : v > hi ? hi : v;
    }

    loR.addEventListener("input", function () {
      curLo = Math.min(Number(loR.value), curHi);
      apply();
    });
    hiR.addEventListener("input", function () {
      curHi = Math.max(Number(hiR.value), curLo);
      apply();
    });
    loNum.addEventListener("input", function () {
      var v = Number(loNum.value);
      if (!isNaN(v)) {
        curLo = clamp(v, dmin, curHi);
        apply();
      }
    });
    hiNum.addEventListener("input", function () {
      var v = Number(hiNum.value);
      if (!isNaN(v)) {
        curHi = clamp(v, curLo, dmax);
        apply();
      }
    });

    // Reset both handles to the full domain for "Clear filters" (curLo/curHi at
    // the edges make apply() emit null min/max → no filter). Caller resets the
    // facet itself, so we only re-sync the visible controls here.
    targets.forEach(function (tg) {
      tg.ds.facetResets.push(function () {
        curLo = dmin;
        curHi = dmax;
        loR.value = String(dmin);
        hiR.value = String(dmax);
        loNum.value = fmtNum(dmin);
        hiNum.value = fmtNum(dmax);
        fill.style.left = "0%";
        fill.style.width = "100%";
      });
    });

    box.appendChild(loNum);
    box.appendChild(slider);
    box.appendChild(hiNum);
    wrap.appendChild(label);
    wrap.appendChild(box);
    apply();
    return wrap;
  }

  function makeSearch(def, targets) {
    var wrap = document.createElement("div");
    wrap.className = "red-filter";
    var label = document.createElement("span");
    label.className = "red-filter-label";
    label.textContent = labelOf(def);
    var inp = document.createElement("input");
    inp.type = "search";
    inp.className = "red-search";
    inp.placeholder = "contains…";
    inp.addEventListener("input", function () {
      targets.forEach(function (tg) {
        tg.ds.state.facets[tg.ci] = { type: "text", text: inp.value };
        tg.ds.notify();
      });
    });
    // Clear the box for "Clear filters" (caller resets the facet itself).
    targets.forEach(function (tg) {
      tg.ds.facetResets.push(function () { inp.value = ""; });
    });
    wrap.appendChild(label);
    wrap.appendChild(inp);
    return wrap;
  }

  function renderFilters(filters, registry, t) {
    if (!Array.isArray(filters) || !filters.length) return;
    var controls = [];
    filters.forEach(function (def) {
      if (!def || typeof def !== "object" || !def.column) return;
      var targets = resolveTargets(registry, def);
      if (!targets.length) return;
      var type = def.type || "multiselect";
      var el =
        type === "select" ? makeSelect(def, targets)
        : type === "range" ? makeRange(def, targets, t)
        : type === "search" ? makeSearch(def, targets)
        : makeMulti(def, targets);
      if (el) controls.push(el);
    });
    if (!controls.length) return;

    var host = document.querySelector("[data-red-filters]");
    var created = false;
    if (!host) { host = document.createElement("div"); created = true; }
    host.classList.add("red-filters");
    controls.forEach(function (c) { host.appendChild(c); });
    if (created && document.body)
      document.body.insertBefore(host, document.body.firstChild);
    document.addEventListener("click", closeAllPanels);
  }

  // ---- entry -----------------------------------------------------------------

  function render() {
    if (typeof Chart === "undefined") return;
    var payload = readPayload();
    // Paint in Red's active theme when supplied, else the built-in light/dark.
    var t = themeFrom(payload.theme);
    injectStyles(t);
    applyTheme(t);
    var registry = buildRegistry(payload.data);

    var tables = document.querySelectorAll("[data-red-table]");
    for (var i = 0; i < tables.length; i++) {
      var tslot = tables[i];
      var name = tslot.getAttribute("data-red-table");
      var ds = registry[name];
      if (!ds) {
        tslot.textContent = "Unknown dataset: " + name;
        continue;
      }
      try {
        renderTable(tslot, ds, t);
      } catch (e) {
        tslot.textContent = "Could not render this table.";
      }
    }

    var charts = payload.charts || [];
    var slots = document.querySelectorAll("[data-red-chart]");
    for (var j = 0; j < slots.length; j++) {
      var cslot = slots[j];
      var idx = parseInt(cslot.getAttribute("data-red-chart"), 10);
      var spec = charts[idx];
      if (!spec || typeof spec !== "object") continue;
      renderChart(cslot, spec, t, registry);
    }

    // Filter bar last, so tables/charts are already subscribed before it applies
    // its defaults.
    renderFilters(payload.filters, registry, t);

    // Initial paint of any table without an active filter default.
    for (var k in registry) registry[k].notify();

    // Backstops against blank charts: re-fit once the document has fully loaded
    // and whenever the tab becomes visible (a report opened in a background tab
    // can be created at 0px and stay blank until then).
    window.addEventListener("load", resizeAllCharts);
    document.addEventListener("visibilitychange", function () {
      if (!document.hidden) resizeAllCharts();
    });
  }

  // Run after a layout pass so chart containers report their real size at
  // creation time (the bundle executes while the document is still "interactive",
  // before the just-set card heights are laid out).
  function start() {
    if (typeof requestAnimationFrame === "function") {
      requestAnimationFrame(function () {
        requestAnimationFrame(render);
      });
    } else {
      render();
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
