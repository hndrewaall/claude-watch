// Info dropdown toggle in the topbar.
//
// Shows session info (auth user, cache age, /api/queue link) on demand.
// Replaces the old fixed bottombar so the visual real-estate stays focused
// on queue state. Toggled by the ⓘ button in the topbar.
//
// Accessibility:
//   - aria-expanded reflects state on the toggle
//   - role=menu on the panel; closes on Esc and on outside click
//   - focus returns to the toggle on close
(function () {
  'use strict';

  const toggle = document.getElementById('info-toggle');
  const dropdown = document.getElementById('info-dropdown');
  if (!toggle || !dropdown) return;

  function open() {
    dropdown.hidden = false;
    toggle.setAttribute('aria-expanded', 'true');
    document.addEventListener('click', onDocClick, true);
    document.addEventListener('keydown', onKeyDown, true);
  }

  function close() {
    dropdown.hidden = true;
    toggle.setAttribute('aria-expanded', 'false');
    document.removeEventListener('click', onDocClick, true);
    document.removeEventListener('keydown', onKeyDown, true);
  }

  function onDocClick(ev) {
    if (dropdown.contains(ev.target) || toggle.contains(ev.target)) return;
    close();
  }

  function onKeyDown(ev) {
    if (ev.key === 'Escape') {
      close();
      toggle.focus();
    }
  }

  toggle.addEventListener('click', (ev) => {
    ev.stopPropagation();
    if (dropdown.hidden) open();
    else close();
  });
})();
