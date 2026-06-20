/*!
 * Red report chart renderer — runs after the vendored Chart.js UMD bundle.
 *
 * Trust model: this is the ONLY executable code in a report (it carries the
 * report's CSP nonce; the model's HTML and the chart specs do not). It reads the
 * model-authored chart specs from an inert `application/json` block and draws
 * each one with Chart.js. The specs are pure DATA — Chart.js never evals them —
 * and the report CSP forbids all network egress (`connect-src 'none'`), so a spec
 * cannot run code or leak the data it visualizes. Each chart slots into a
 * `<div data-red-chart="N">` placeholder the model placed in its report body.
 *
 * This is the renderer source. The bundle the crate actually embeds is
 * `report-charts.js` = Chart.js UMD min + this file (see README.md).
 */
(function () {
  "use strict";

  function readSpecs() {
    var node = document.getElementById("red-report-data");
    if (!node) return [];
    try {
      var parsed = JSON.parse(node.textContent || "{}");
      return Array.isArray(parsed.charts) ? parsed.charts : [];
    } catch (e) {
      return [];
    }
  }

  // Match the report shell's light/dark base so charts read on either theme.
  function applyTheme() {
    if (typeof Chart === "undefined") return;
    var dark =
      window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches;
    Chart.defaults.color = dark ? "#e6e6e6" : "#1a1a1a";
    Chart.defaults.borderColor = dark ? "#262a31" : "#e5e7eb";
    Chart.defaults.font.family =
      "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif";
    Chart.defaults.maintainAspectRatio = false;
  }

  function render() {
    if (typeof Chart === "undefined") return;
    applyTheme();
    var specs = readSpecs();
    var slots = document.querySelectorAll("[data-red-chart]");
    for (var i = 0; i < slots.length; i++) {
      var slot = slots[i];
      var idx = parseInt(slot.getAttribute("data-red-chart"), 10);
      var spec = specs[idx];
      if (!spec || typeof spec !== "object") continue;
      // Give the chart a sized box; the model can override via the slot's style.
      if (!slot.style.height) slot.style.height = "360px";
      slot.style.position = slot.style.position || "relative";
      var canvas = document.createElement("canvas");
      slot.appendChild(canvas);
      try {
        new Chart(canvas, spec);
      } catch (e) {
        slot.textContent = "Could not render this chart.";
      }
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", render);
  } else {
    render();
  }
})();
