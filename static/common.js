// Shared by every page.

// A whole number with a (non-breaking) space between the thousands: 1 234 567.
function fmtInt(n) {
  if (n == null || Number.isNaN(Number(n))) return "–";
  return Math.round(Number(n)).toLocaleString("en-US").replace(/,/g, " ");
}
