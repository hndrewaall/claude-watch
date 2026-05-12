// Browser-local time rendering helpers.
//
// Backend emits timestamps as ISO8601 UTC (e.g. "2026-05-01T21:38:42Z").
// We render them in the viewer's local timezone using
// Intl.DateTimeFormat / toLocaleString defaults — no explicit zone is
// passed so each viewer's browser picks its own. UTC stays the wire
// format; conversion is purely a frontend concern.
//
// Exposed on window as `LocalTime` (no module loader; minisite scripts
// are plain <script> tags). Functions are pure and side-effect-free.
//
// Times render in 24-hour format (hour12: false) regardless of locale
// default — matches the cross-app design system convention.
//
// Usage:
//   LocalTime.timeOnly("2026-05-01T21:38:42Z")  -> "17:38:42" (en-US, ET)
//   LocalTime.dateTime("2026-05-01T21:38:42Z")  -> "5/1/2026, 17:38:42"
//   LocalTime.tzShort()                          -> "EDT" / "PDT" / etc.
//   LocalTime.hydrate()                          -> walk the DOM, fill
//     any element with [data-local-time-iso] using the format named by
//     [data-local-time-fmt] (default "time"). Idempotent — sets
//     `data-local-time-rendered="1"` and skips already-rendered nodes.

(function () {
  'use strict';

  function _parse(iso) {
    if (!iso) return null;
    const d = new Date(iso);
    if (isNaN(d.getTime())) return null;
    return d;
  }

  function timeOnly(iso) {
    const d = _parse(iso);
    if (!d) return '';
    // Default locale; force 24-hour format (cross-app design system).
    return d.toLocaleTimeString(undefined, { hour12: false });
  }

  function dateOnly(iso) {
    const d = _parse(iso);
    if (!d) return '';
    return d.toLocaleDateString();
  }

  function dateTime(iso) {
    const d = _parse(iso);
    if (!d) return '';
    return d.toLocaleString(undefined, { hour12: false });
  }

  // Short timezone abbreviation for the current locale (e.g. "EDT").
  // Falls back to a fixed-offset string ("UTC-04:00") if the runtime
  // can't produce a name.
  function tzShort(date) {
    const d = date || new Date();
    try {
      const parts = new Intl.DateTimeFormat(undefined, {
        timeZoneName: 'short',
      }).formatToParts(d);
      const tz = parts.find((p) => p.type === 'timeZoneName');
      if (tz && tz.value) return tz.value;
    } catch (_) {
      // fall through
    }
    // Fallback: ±HH:MM offset.
    const off = -d.getTimezoneOffset();
    const sign = off >= 0 ? '+' : '-';
    const abs = Math.abs(off);
    const hh = String(Math.floor(abs / 60)).padStart(2, '0');
    const mm = String(abs % 60).padStart(2, '0');
    return 'UTC' + sign + hh + ':' + mm;
  }

  const FORMATTERS = {
    time: timeOnly,
    date: dateOnly,
    datetime: dateTime,
  };

  function format(iso, name) {
    const fn = FORMATTERS[name] || timeOnly;
    return fn(iso);
  }

  // Walk the DOM and render any element with [data-local-time-iso].
  // Format is taken from [data-local-time-fmt] ("time" | "date" |
  // "datetime"; default "time"). Optionally a [data-local-time-suffix]
  // attribute appends extra text (e.g. " " + tzShort()).
  //
  // Modes:
  //   default                       — replace textContent with formatted string.
  //   [data-local-time-title-only]  — leave textContent alone, just set the
  //                                   title attribute (tooltip) to the
  //                                   long datetime form. Used for row
  //                                   "created 5m ago" lines where the
  //                                   visible text is a relative age and
  //                                   the tooltip used to be raw UTC ISO.
  //   [data-local-time-tooltip]     — also set title to long form (in
  //                                   addition to replacing textContent).
  // Idempotent: sets data-local-time-rendered to the iso value so re-runs
  // (e.g. on refresh.js tick) skip already-rendered nodes unless the iso
  // changed.
  function hydrate(root) {
    const scope = root || document;
    const nodes = scope.querySelectorAll('[data-local-time-iso]');
    nodes.forEach((el) => {
      const iso = el.getAttribute('data-local-time-iso');
      if (!iso) return;
      const prev = el.getAttribute('data-local-time-rendered');
      if (prev === iso) return; // already done for this iso
      const fmt = el.getAttribute('data-local-time-fmt') || 'time';
      const suffix = el.getAttribute('data-local-time-suffix') || '';
      const titleOnly = el.hasAttribute('data-local-time-title-only');
      if (titleOnly) {
        el.title = dateTime(iso);
      } else {
        const out = format(iso, fmt);
        if (out) {
          el.textContent = out + (suffix || '');
          if (el.hasAttribute('data-local-time-tooltip')) {
            el.title = dateTime(iso);
          }
        }
      }
      el.setAttribute('data-local-time-rendered', iso);
    });
  }

  window.LocalTime = {
    timeOnly,
    dateOnly,
    dateTime,
    tzShort,
    format,
    hydrate,
  };

  // Auto-hydrate on DOMContentLoaded so server-rendered ISO strings
  // become local-formatted before the user notices. refresh.js can call
  // LocalTime.hydrate() again after each tick.
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => hydrate());
  } else {
    hydrate();
  }
})();
