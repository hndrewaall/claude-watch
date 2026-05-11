// Drag-and-drop dependency UI for the queue minisite.
//
// Mechanics:
//   - Pending rows have ``draggable="true"``; running + pending rows
//     have class ``drop-zone``.
//   - On dragstart: stash the dragged row's id+summary, add ``dragging``.
//   - On dragover (valid target): preventDefault to allow drop, add
//     ``drop-target`` highlight on the row under the cursor.
//   - On dragleave / dragend: clear highlights.
//   - On drop: POST /api/queue/depend with {dragged_id, target_id}.
//       200 -> reload. 501 (model not yet wired) -> info toast. 4xx ->
//       error toast.
//
// We do NOT interfere with click handling on the action buttons — the
// stop/abandon buttons live inside the same article but accept clicks
// independently because dragstart fires only when the user actually
// begins a drag gesture (>3px movement on most browsers). We also
// suppress dragstart when the gesture begins on an action button so a
// botched click on "abandon" never flips into a drag.
//
// Stub-aware: until the model side lands the endpoint returns 501, and
// we surface the explanation as an info toast rather than a hard error.

(function () {
  'use strict';

  const ENDPOINT = '/api/queue/depend';
  const TOAST_TIMEOUT_MS = 4500;

  let dragSrc = null; // { id, summary, el }
  let lastTarget = null; // last element with .drop-target

  function clearHighlights() {
    document.querySelectorAll('.item.drop-target').forEach((el) => {
      el.classList.remove('drop-target');
    });
    lastTarget = null;
  }

  function findDropZone(el) {
    if (!el || !(el instanceof Element)) return null;
    return el.closest('.item.drop-zone');
  }

  function showToast(message, kind) {
    const existing = document.querySelector('.depend-toast');
    if (existing) existing.remove();
    const toast = document.createElement('div');
    toast.className = 'depend-toast';
    if (kind === 'info') toast.classList.add('depend-toast-info');
    toast.setAttribute('role', 'status');
    toast.textContent = message;
    document.body.appendChild(toast);
    setTimeout(() => {
      toast.remove();
    }, TOAST_TIMEOUT_MS);
  }

  function onDragStart(ev) {
    const article = ev.target.closest('.item.draggable');
    if (!article) return;
    // If the gesture started on an action button (stop/abandon) or its
    // descendants, cancel — preserves the button's click behavior.
    const actionBtn = ev.target.closest('.action-btn');
    if (actionBtn) {
      ev.preventDefault();
      return;
    }
    const id = article.getAttribute('data-queue-id') || '';
    const summary = article.getAttribute('data-queue-summary') || '';
    if (!id) return;
    dragSrc = { id, summary, el: article };
    article.classList.add('dragging');
    // setData is required for the drag to actually take effect in
    // Firefox; the payload doubles as a fallback identifier.
    if (ev.dataTransfer) {
      try {
        ev.dataTransfer.setData('text/plain', id);
      } catch (_) {
        /* ignore — some browsers throw if called too early */
      }
      ev.dataTransfer.effectAllowed = 'link';
    }
  }

  function onDragOver(ev) {
    if (!dragSrc) return;
    const dz = findDropZone(ev.target);
    if (!dz) return;
    // Don't allow dropping onto self — would be a no-op (and the
    // backend rejects with 400 anyway).
    if (dz === dragSrc.el) return;
    ev.preventDefault();
    if (ev.dataTransfer) ev.dataTransfer.dropEffect = 'link';
    if (lastTarget && lastTarget !== dz) {
      lastTarget.classList.remove('drop-target');
    }
    dz.classList.add('drop-target');
    lastTarget = dz;
  }

  function onDragLeave(ev) {
    if (!dragSrc) return;
    const dz = findDropZone(ev.target);
    if (!dz) return;
    // Only clear when leaving the drop zone entirely — relatedTarget
    // is the element entering — if it's still inside the same dz, keep
    // the highlight.
    const enteringInside =
      ev.relatedTarget && dz.contains(ev.relatedTarget);
    if (!enteringInside) {
      dz.classList.remove('drop-target');
      if (lastTarget === dz) lastTarget = null;
    }
  }

  function onDragEnd() {
    if (dragSrc && dragSrc.el) {
      dragSrc.el.classList.remove('dragging');
    }
    clearHighlights();
    dragSrc = null;
  }

  async function onDrop(ev) {
    if (!dragSrc) return;
    const dz = findDropZone(ev.target);
    if (!dz) return;
    if (dz === dragSrc.el) return;
    ev.preventDefault();

    const draggedId = dragSrc.id;
    const targetId = dz.getAttribute('data-queue-id') || '';
    const targetSummary = dz.getAttribute('data-queue-summary') || '';

    // Reset visual state immediately — request is async.
    dragSrc.el.classList.remove('dragging');
    clearHighlights();
    const srcSummary = dragSrc.summary;
    dragSrc = null;

    if (!targetId) return;

    try {
      const r = await fetch(ENDPOINT, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        cache: 'no-store',
        body: JSON.stringify({
          dragged_id: draggedId,
          target_id: targetId,
        }),
      });
      let body = null;
      try {
        body = await r.json();
      } catch (_) {
        body = null;
      }
      if (r.ok && body && body.ok) {
        // Real path (post-model): reload to surface the new dep edge.
        location.reload();
        return;
      }
      // Stub-aware: 501 means validation passed but the model isn't
      // wired yet — surface as an info toast, not a scary error.
      if (r.status === 501) {
        const note =
          (body && (body.note || body.error)) ||
          'dependency model not yet implemented';
        showToast(`Would link ${draggedId} → ${targetId}: ${note}`, 'info');
        return;
      }
      const detail =
        (body && (body.error || body.stderr || body.stdout)) ||
        `HTTP ${r.status}`;
      showToast(`depend failed: ${detail}`, 'error');
    } catch (e) {
      showToast(
        `network error: ${e && e.message ? e.message : e}`,
        'error',
      );
    }
  }

  // ----- dep-badge click handlers (link + remove) -------------------------
  //
  // Dep badges render as: → q-XXXX  ×
  // The anchor scrolls the target row into view + flashes it briefly. The
  // remove button DELETEs the edge via /api/queue/<source>/depend.
  // Document-level listeners survive morphdom re-renders.

  function flashRow(targetId) {
    const row = document.getElementById('queue-' + targetId);
    if (!row) return;
    row.classList.remove('dep-flash');
    // force reflow so the animation restarts when re-applied
    void row.offsetWidth;
    row.classList.add('dep-flash');
  }

  function onDepLinkClick(ev) {
    const a = ev.target.closest('.dep-link');
    if (!a) return;
    const targetId = a.getAttribute('data-dep-target');
    if (!targetId) return;
    // Default <a href="#queue-..."> scroll is fine; we just add the flash.
    setTimeout(() => flashRow(targetId), 50);
  }

  async function onDepRemoveClick(ev) {
    const btn = ev.target.closest('.dep-remove-btn');
    if (!btn) return;
    ev.preventDefault();
    ev.stopPropagation();
    const sourceId = btn.getAttribute('data-dep-source');
    const targetId = btn.getAttribute('data-dep-target');
    if (!sourceId || !targetId) return;
    btn.disabled = true;
    try {
      const r = await fetch(
        '/api/queue/' + encodeURIComponent(sourceId) + '/depend',
        {
          method: 'DELETE',
          headers: { 'Content-Type': 'application/json' },
          cache: 'no-store',
          body: JSON.stringify({ target_id: targetId }),
        },
      );
      let body = null;
      try {
        body = await r.json();
      } catch (_) {
        body = null;
      }
      if (r.ok && body && body.ok) {
        location.reload();
        return;
      }
      const detail = (body && body.error) || `HTTP ${r.status}`;
      showToast(`remove dep failed: ${detail}`, 'error');
    } catch (e) {
      showToast(
        `network error: ${e && e.message ? e.message : e}`,
        'error',
      );
    } finally {
      btn.disabled = false;
    }
  }

  // Use document-level listeners so refresh.js' page reload doesn't
  // require us to re-bind on rerender.
  document.addEventListener('dragstart', onDragStart);
  document.addEventListener('dragover', onDragOver);
  document.addEventListener('dragleave', onDragLeave);
  document.addEventListener('dragend', onDragEnd);
  document.addEventListener('drop', onDrop);
  document.addEventListener('click', onDepLinkClick);
  document.addEventListener('click', onDepRemoveClick);
})();
