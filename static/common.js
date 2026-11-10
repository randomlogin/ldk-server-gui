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
