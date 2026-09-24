/**
 * Tab bar.
 *
 * The tabs are declared here rather than in index.html so adding a view is one
 * entry in this list plus a module — the pane id is derived from the key.
 */

import { $, html } from '../core/dom.js';

/**
 * Each tab: `key` doubles as the pane id (`tab-<key>`) and the `data-tab`
 * value the router matches on.
 */
export const TABS = [
  { key: 'overview', label: '运行概览' },
  { key: 'logs', label: '实时日志' },
  { key: 'settings', label: '服务设置' },
];

/** Render the tab buttons into #tabs. */
export function renderTabs() {
  const nav = $('#tabs');
  if (!nav) return;
  nav.innerHTML = TABS.map((t, i) => html`
    <button class="tab${i === 0 ? ' active' : ''}" data-tab="${t.key}">${t.label}</button>
  `).join('');
}

/** Mark one tab active and the rest inactive. */
export function setActiveTab(key) {
  for (const btn of document.querySelectorAll('.tab')) {
    btn.classList.toggle('active', btn.dataset.tab === key);
  }
}
