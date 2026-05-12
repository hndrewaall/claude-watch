#!/usr/bin/env node
// Exercises the metadata-toggle persistence + viewport-default logic in
// live-log.js by booting jsdom with the real index.html template fragment
// and driving setMetaToggleInitialState() across various states.

'use strict';

const path = require('path');
const fs = require('fs');

const NODE_MODULES = process.env.QM_NODE_MODULES ||
  '/tmp/queue-minisite-test/node_modules';
const { JSDOM } = require(path.join(NODE_MODULES, 'jsdom'));

const STATIC_DIR = path.dirname(path.resolve(__filename));
const liveLogSrc = fs.readFileSync(path.join(STATIC_DIR, 'live-log.js'), 'utf8');

function bootDom(viewportWidth, storedValue) {
  const html = `<!doctype html><html><body>
    <div id="log-modal" hidden>
      <span id="log-modal-id"></span>
      <span id="log-modal-mode-label"></span>
      <span id="log-modal-summary"></span>
      <span id="log-modal-status"></span>
      <pre id="log-modal-stream"></pre>
      <button id="log-modal-close"></button>
      <button id="log-modal-autoscroll"></button>
      <button id="log-modal-jump-top"></button>
      <button id="log-modal-jump-bottom"></button>
      <details id="log-modal-prompt" hidden>
        <summary><span id="log-modal-prompt-label"></span></summary>
        <pre id="log-modal-prompt-body"></pre>
      </details>
      <div id="log-modal-meta-summary" hidden>
        <details id="log-meta-toggle">
          <summary><span class="log-meta-toggle-label">Metadata</span></summary>
          <div id="log-meta-rows">
            <div id="log-meta-row-status"></div>
            <div id="log-meta-row-runtime"></div>
            <div id="log-meta-row-times"></div>
            <div id="log-meta-row-scope"></div>
            <div id="log-meta-row-deps"></div>
            <div id="log-meta-row-dependents"></div>
            <div id="log-meta-row-by"></div>
            <div id="log-meta-row-group"></div>
            <div id="log-meta-row-usage"></div>
            <div id="log-meta-row-abandon"></div>
          </div>
        </details>
      </div>
      <span id="log-meta-status"></span>
      <span id="log-meta-runtime"></span>
      <span id="log-meta-times"></span>
      <span id="log-meta-scope"></span>
      <span id="log-meta-deps"></span>
      <span id="log-meta-dependents"></span>
      <span id="log-meta-by"></span>
      <span id="log-meta-group"></span>
      <span id="log-meta-usage"></span>
      <span id="log-meta-abandon"></span>
      <details id="log-modal-return" hidden>
        <summary><span id="log-modal-return-label"></span></summary>
        <pre id="log-modal-return-body"></pre>
      </details>
    </div>
  </body></html>`;
  const dom = new JSDOM(html, {
    runScripts: 'outside-only',
    pretendToBeVisual: true,
    url: 'http://localhost/',
  });
  const { window } = dom;
  // Force innerWidth to the test value
  Object.defineProperty(window, 'innerWidth', { value: viewportWidth, configurable: true });
  // Seed localStorage
  if (storedValue !== null) {
    window.localStorage.setItem('queue-minisite.metadataExpanded', storedValue);
  } else {
    window.localStorage.removeItem('queue-minisite.metadataExpanded');
  }
  window.eval(liveLogSrc);
  return { window, hooks: window.__liveLog };
}

let failures = 0;
function assert(label, cond, detail) {
  if (cond) console.log('  ok  ' + label);
  else {
    failures += 1;
    console.error('  FAIL ' + label + (detail ? '\n       ' + detail : ''));
  }
}

console.log('Default state — first visit, mobile (375px)');
{
  const { window, hooks } = bootDom(375, null);
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('collapsed on narrow viewport', toggle.open === false, 'open=' + toggle.open);
}

console.log('Default state — first visit, desktop (1024px)');
{
  const { window, hooks } = bootDom(1024, null);
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('expanded on wide viewport', toggle.open === true);
}

console.log('Default state — first visit, breakpoint exactly 768px');
{
  const { window, hooks } = bootDom(768, null);
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('expanded at 768px (>= breakpoint)', toggle.open === true);
}

console.log('Stored "1" → expanded regardless of viewport');
{
  const { window, hooks } = bootDom(320, '1');
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('stored=1 forces expanded on mobile', toggle.open === true);
}

console.log('Stored "0" → collapsed regardless of viewport');
{
  const { window, hooks } = bootDom(1920, '0');
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('stored=0 forces collapsed on desktop', toggle.open === false);
}

console.log('Toggle persists state on user open');
{
  const { window, hooks } = bootDom(1920, '0');
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('initial collapsed', toggle.open === false);
  toggle.open = true;
  toggle.dispatchEvent(new window.Event('toggle'));
  const stored = window.localStorage.getItem('queue-minisite.metadataExpanded');
  assert('localStorage updated to 1 after open', stored === '1', 'stored=' + stored);
}

console.log('Toggle persists state on user close');
{
  const { window, hooks } = bootDom(320, '1');
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('initial expanded', toggle.open === true);
  toggle.open = false;
  toggle.dispatchEvent(new window.Event('toggle'));
  const stored = window.localStorage.getItem('queue-minisite.metadataExpanded');
  assert('localStorage updated to 0 after close', stored === '0', 'stored=' + stored);
}

console.log('Garbage value in localStorage → fall back to viewport default');
{
  const { window, hooks } = bootDom(320, 'banana');
  hooks.setMetaToggleInitialState();
  const toggle = window.document.getElementById('log-meta-toggle');
  assert('garbage in storage → mobile collapsed default', toggle.open === false);
}

if (failures > 0) {
  console.error('\nFAILED: ' + failures + ' assertion(s)');
  process.exit(1);
}
console.log('\nAll meta-toggle assertions passed.');
