// Shared by every page.

// A whole number with a (non-breaking) space between the thousands: 1 234 567.
function fmtInt(n) {
  if (n == null || Number.isNaN(Number(n))) return "–";
  return Math.round(Number(n)).toLocaleString("en-US").replace(/,/g, " ");
}

// Sortable tables. Click a column header to sort by that column, click again to reverse. Every table
// with a <thead> gets it (opt a header out with data-nosort). A cell is sorted by its `data-sort`
// attribute when it has one, which is how times, sizes and amounts that are shown as text sort by
// their real value, and by its text otherwise. Empty cells ("", "—", "–") always go last.
//
// The pages rebuild a table's body on every refresh, so the sort is applied again after each one. It
// is remembered per page and table, in this browser. With no column chosen, the page's own order
// stays.
(function () {
  var STYLE =
    "th.sortable{cursor:pointer;user-select:none;white-space:nowrap}" +
    "th.sortable:hover{color:var(--text,#d7dde8)}" +
    "th.sortable::after{content:' \\21C5';opacity:.3;font-size:.85em}" +
    "th.sorted-asc::after{content:' \\25B2';opacity:1}" +
    "th.sorted-desc::after{content:' \\25BC';opacity:1}";

  function cellKey(row, col) {
    var cell = row.cells[col];
    if (!cell) return "";
    var raw = cell.dataset.sort !== undefined ? cell.dataset.sort : cell.textContent;
    return String(raw).trim();
  }

  function isEmpty(key) { return key === "" || key === "—" || key === "–"; }

  // "1 234 567 sat" -> 1234567; anything that is not just a number with an optional unit -> null.
  function asNumber(key) {
    var s = key.replace(/[\s ]/g, "");
    return /^-?\d+(\.\d+)?(msat|sat|%)?$/.test(s) ? parseFloat(s) : null;
  }

  function compare(a, b) {
    var x = asNumber(a), y = asNumber(b);
    if (x !== null && y !== null) return x - y;
    return a.localeCompare(b, undefined, { numeric: true, sensitivity: "base" });
  }

  // A row that holds only a message ("loading…", "no peers") spans the table and is not data.
  function isMessageRow(row) { return row.cells.length === 1 && row.cells[0].colSpan > 1; }

  function storageKey(table) {
    return "sort:" + location.pathname + ":" + (table.dataset.sortId || table.tBodies[0].id || "table");
  }

  function mark(table) {
    var state = table._sort;
    Array.prototype.forEach.call(table.tHead.querySelectorAll("th"), function (th, i) {
      th.classList.remove("sorted-asc", "sorted-desc");
      th.removeAttribute("aria-sort");
      if (state && state.col === i) {
        th.classList.add(state.dir === "asc" ? "sorted-asc" : "sorted-desc");
        th.setAttribute("aria-sort", state.dir === "asc" ? "ascending" : "descending");
      }
    });
  }

  function apply(table) {
    var state = table._sort;
    if (!state) return;
    var body = table.tBodies[0];
    var rows = Array.prototype.slice.call(body.rows);
    if (rows.some(isMessageRow)) return;
    var keyed = rows.map(function (row, i) { return { row: row, key: cellKey(row, state.col), i: i }; });
    keyed.sort(function (p, q) {
      var pe = isEmpty(p.key), qe = isEmpty(q.key);
      if (pe || qe) return pe && qe ? p.i - q.i : pe ? 1 : -1;
      var c = compare(p.key, q.key);
      return (state.dir === "desc" ? -c : c) || p.i - q.i;
    });
    // Moving rows is a change the observer sees; when they are already in place nothing is moved,
    // so it does not loop.
    if (keyed.every(function (e, j) { return e.row === rows[j]; })) return;
    keyed.forEach(function (e) { body.appendChild(e.row); });
  }

  // Numbers start with the largest, text with A.
  function firstDirection(table, col) {
    var rows = Array.prototype.slice.call(table.tBodies[0].rows).filter(function (r) { return !isMessageRow(r); });
    for (var i = 0; i < rows.length; i++) {
      var key = cellKey(rows[i], col);
      if (!isEmpty(key)) return asNumber(key) !== null ? "desc" : "asc";
    }
    return "asc";
  }

  function choose(table, col) {
    var state = table._sort;
    var dir = state && state.col === col ? (state.dir === "asc" ? "desc" : "asc") : firstDirection(table, col);
    table._sort = { col: col, dir: dir };
    try { localStorage.setItem(storageKey(table), JSON.stringify(table._sort)); } catch (e) {}
    mark(table);
    apply(table);
  }

  function setup(table) {
    if (!table.tHead || !table.tBodies[0]) return;
    var headers = table.tHead.querySelectorAll("th");
    if (headers.length < 2) return;
    try {
      var saved = JSON.parse(localStorage.getItem(storageKey(table)));
      if (saved && saved.col < headers.length && (saved.dir === "asc" || saved.dir === "desc")) table._sort = saved;
    } catch (e) {}
    Array.prototype.forEach.call(headers, function (th, i) {
      if (!th.textContent.trim() || th.hasAttribute("data-nosort")) return;
      th.classList.add("sortable");
      th.tabIndex = 0;
      th.addEventListener("click", function () { choose(table, i); });
      th.addEventListener("keydown", function (event) {
        if (event.key === "Enter" || event.key === " ") { event.preventDefault(); choose(table, i); }
      });
    });
    mark(table);
    apply(table);
    new MutationObserver(function () { apply(table); }).observe(table.tBodies[0], { childList: true });
  }

  document.addEventListener("DOMContentLoaded", function () {
    var style = document.createElement("style");
    style.textContent = STYLE;
    document.head.appendChild(style);
    Array.prototype.forEach.call(document.querySelectorAll("table"), setup);
  });
})();

// Bytes as B, KiB, MiB, GiB or TiB, a whole number of bytes and one decimal above.
function fmtBytes(n) {
  var units = ["B", "KiB", "MiB", "GiB", "TiB"];
  var i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return (i ? n.toFixed(1) : String(Math.round(n))) + " " + units[i];
}

// Line charts.
//
//   renderChart(element, [{t: unix seconds, v: number or null}, ...], formatValue, options)
//
// draws a chart that fills `element`: values on the left with a grid line at each, times along the
// bottom with a grid line at each, the line over a soft area, and a readout of the time and value
// under the mouse. The line breaks at a null value and where readings are missing for a while.
// It is drawn again when the element changes width.
//
// options: zero (start the value axis at 0), binary (put the value ticks on multiples of 1024, for
// bytes), height (pixels, default 170).
function renderChart(el, points, fmt, options) {
  el._chart = { points: points, fmt: fmt, options: options || {} };
  if (!el._chartObserver && window.ResizeObserver) {
    el._chartObserver = new ResizeObserver(function () {
      if (el.clientWidth !== el._chartWidth) drawChart(el);
    });
    el._chartObserver.observe(el);
  }
  drawChart(el);
}

// A step for ticks that is 1, 2, 2.5 or 5 times a power of ten (of 1024 if `binary`), giving about
// `count` ticks over `range`.
function chartStep(range, count, binary) {
  var raw = range / count, unit = 1;
  if (binary && raw >= 1024) {
    unit = Math.pow(1024, Math.floor(Math.log(raw) / Math.log(1024)));
    raw /= unit;
  }
  var magnitude = Math.pow(10, Math.floor(Math.log10(raw)));
  var n = raw / magnitude;
  return (n <= 1 ? 1 : n <= 2 ? 2 : n <= 2.5 ? 2.5 : n <= 5 ? 5 : 10) * magnitude * unit;
}

function drawChart(el) {
  var chart = el._chart;
  if (!chart) return;
  var points = chart.points, fmt = chart.fmt, options = chart.options;
  var known = points.filter(function (p) { return p.v != null; });
  el._chartWidth = el.clientWidth;
  if (known.length < 2) { el.innerHTML = '<div class="muted">collecting readings…</div>'; return; }

  var W = Math.max(300, el.clientWidth || 600), H = options.height || 170;
  var t0 = points[0].t, t1 = Math.max(points[points.length - 1].t, t0 + 1), span = t1 - t0;
  var vmin = Infinity, vmax = -Infinity;
  known.forEach(function (p) { vmin = Math.min(vmin, p.v); vmax = Math.max(vmax, p.v); });

  // The value axis: from 0 or from just under the lowest reading, up to just over the highest, on
  // round numbers.
  var margin = Math.max((vmax - vmin) * 0.15, Math.abs(vmax) * 0.02, 1e-9);
  var lo = options.zero ? 0 : vmin - margin, hi = vmax + margin;
  if (!(hi > lo)) hi = lo + 1;
  var step = chartStep(hi - lo, 4, options.binary);
  lo = Math.floor(lo / step + 1e-9) * step;
  hi = Math.ceil(hi / step - 1e-9) * step;
  if (options.zero) lo = 0;
  var yTicks = [];
  for (var k = 0; lo + k * step <= hi + step * 1e-9; k++) yTicks.push(lo + k * step);

  // The time axis: a tick every round number of minutes or hours, in local time.
  var stepsT = [60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800];
  var stepT = 172800;
  for (var s = 0; s < stepsT.length; s++) { if (span / stepsT[s] <= 6) { stepT = stepsT[s]; break; } }
  var offset = new Date().getTimezoneOffset() * 60;
  var xTicks = [];
  for (var t = Math.ceil((t0 - offset) / stepT) * stepT + offset; t <= t1; t += stepT) xTicks.push(t);
  var withDate = span > 36 * 3600;
  function clock(t) {
    var d = new Date(t * 1000);
    var time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    return withDate ? d.toLocaleDateString([], { day: "numeric", month: "short" }) + " " + time : time;
  }

  var labels = yTicks.map(fmt);
  var longest = labels.reduce(function (m, l) { return Math.max(m, l.length); }, 0);
  var ml = Math.round(longest * 6.4 + 14), mr = 12, mt = 10, mb = 24;
  var pw = W - ml - mr, ph = H - mt - mb;
  function x(t) { return ml + (t - t0) / span * pw; }
  function y(v) { return mt + ph - (v - lo) / (hi - lo) * ph; }

  // The line, in pieces: a piece ends at a null reading or a long gap.
  var gaps = points.slice(1).map(function (p, i) { return p.t - points[i].t; }).sort(function (a, b) { return a - b; });
  var gap = (gaps[Math.floor(gaps.length / 2)] || 10) * 6;
  var pieces = [], piece = null, previous = null;
  points.forEach(function (p) {
    if (p.v == null || (previous != null && p.t - previous > gap)) piece = null;
    previous = p.t;
    if (p.v == null) return;
    if (!piece) { piece = []; pieces.push(piece); }
    piece.push([x(p.t), y(p.v)]);
  });
  var bottom = mt + ph;
  function path(pc) { return pc.map(function (q) { return q[0].toFixed(1) + "," + q[1].toFixed(1); }).join("L"); }
  var line = pieces.map(function (pc) { return "M" + path(pc); }).join(" ");
  var fill = pieces.filter(function (pc) { return pc.length > 1; }).map(function (pc) {
    return "M" + pc[0][0].toFixed(1) + "," + bottom + "L" + path(pc) + "L" + pc[pc.length - 1][0].toFixed(1) + "," + bottom + "Z";
  }).join(" ");

  var grid = "", text = "";
  yTicks.forEach(function (v, i) {
    var yy = y(v).toFixed(1);
    grid += '<line x1="' + ml + '" x2="' + (W - mr) + '" y1="' + yy + '" y2="' + yy + '" stroke="rgba(255,255,255,' + (i === 0 ? ".18" : ".07") + ')"/>';
    text += '<text x="' + (ml - 7) + '" y="' + yy + '" dy="0.32em" text-anchor="end">' + labels[i] + "</text>";
  });
  xTicks.forEach(function (tt) {
    var xx = x(tt);
    var anchor = xx - ml < 22 ? "start" : W - mr - xx < 22 ? "end" : "middle";
    grid += '<line x1="' + xx.toFixed(1) + '" x2="' + xx.toFixed(1) + '" y1="' + mt + '" y2="' + bottom + '" stroke="rgba(255,255,255,.07)"/>';
    text += '<text x="' + xx.toFixed(1) + '" y="' + (H - 6) + '" text-anchor="' + anchor + '">' + clock(tt) + "</text>";
  });

  var last = known[known.length - 1].v;
  el.innerHTML =
    '<svg viewBox="0 0 ' + W + " " + H + '" width="' + W + '" height="' + H + '" style="display:block;max-width:100%">' +
    '<defs><linearGradient id="chartfill" x1="0" y1="0" x2="0" y2="1">' +
    '<stop offset="0" stop-color="#ffb84d" stop-opacity=".28"/><stop offset="1" stop-color="#ffb84d" stop-opacity="0"/></linearGradient></defs>' +
    grid +
    '<g style="fill:var(--muted,#8a93a5);font-size:11px">' + text + "</g>" +
    '<path d="' + fill + '" fill="url(#chartfill)" stroke="none"/>' +
    '<path d="' + line + '" fill="none" stroke="#ffb84d" stroke-width="1.6" stroke-linejoin="round"/>' +
    '<g class="chart-hover" style="display:none;pointer-events:none">' +
    '<line y1="' + mt + '" y2="' + bottom + '" stroke="rgba(255,255,255,.35)" stroke-dasharray="3 3"/>' +
    '<circle r="3.5" fill="#ffb84d" stroke="#0b0e14" stroke-width="1.5"/>' +
    '<rect rx="4" height="20" fill="#171d29" stroke="#2e3850"/>' +
    '<text dy="0.32em" style="fill:#d7dde8;font-size:11px"></text></g>' +
    '<rect class="chart-area" x="' + ml + '" y="' + mt + '" width="' + pw + '" height="' + ph + '" fill="transparent"/></svg>' +
    '<div class="muted" style="font-size:12px;margin:2px 0 12px">now ' + fmt(last) + " · highest " + fmt(vmax) +
    " · lowest " + fmt(vmin) + " · " + clock(t0) + " to " + clock(t1) + "</div>";

  var svg = el.querySelector("svg"), hit = svg.querySelector(".chart-area"), hover = svg.querySelector(".chart-hover");
  var hline = hover.querySelector("line"), dot = hover.querySelector("circle"), box = hover.querySelector("rect"), label = hover.querySelector("text");
  hit.addEventListener("mousemove", function (event) {
    var rect = svg.getBoundingClientRect();
    var t = t0 + ((event.clientX - rect.left) * (W / rect.width) - ml) / pw * span;
    var best = null;
    known.forEach(function (p) { if (best === null || Math.abs(p.t - t) < Math.abs(best.t - t)) best = p; });
    var bx = x(best.t), by = y(best.v);
    var content = clock(best.t) + " · " + fmt(best.v);
    var width = content.length * 6.3 + 14;
    var left = bx + 10 + width > W - mr ? bx - 10 - width : bx + 10;
    var top = Math.max(mt, by - 28);
    hline.setAttribute("x1", bx); hline.setAttribute("x2", bx);
    dot.setAttribute("cx", bx); dot.setAttribute("cy", by);
    box.setAttribute("x", left); box.setAttribute("y", top); box.setAttribute("width", width);
    label.setAttribute("x", left + 7); label.setAttribute("y", top + 10);
    label.textContent = content;
    hover.style.display = "";
  });
  hit.addEventListener("mouseleave", function () { hover.style.display = "none"; });
}
