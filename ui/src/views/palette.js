/**
 * Command palette (⌘K).
 *
 * A flat, filterable list of named actions. Add an entry to `actions` and it
 * appears in the palette — no other wiring.
 *
 * The overlay is built here, on first use, through components/overlay.js rather
 * than sitting in index.html: the markup and the handlers that target its ids
 * then cannot drift apart.
 */

import { $, html } from '../core/dom.js';
import { invoke } from '../core/ipc.js';
import { overlay } from '../components/overlay.js';

const ID = 'palette-overlay';
const INPUT_ID = 'palette-input';
const LIST_ID = 'palette-list';

/** Index of the highlighted row; index into the *filtered* list. */
let selected = 0;

/**
 * The palette's contents.
 *
 * `run` is called with no arguments after the palette closes, so an action can
 * assume it is safe to move focus or switch tabs.
 */
const actions = [
  { label: '打开概览 (Overview)', kbd: '1', run: () => switchTab('overview') },
  { label: '查看日志 (Logs)', kbd: '2', run: () => switchTab('logs') },
  { label: '服务设置 (Settings)', kbd: '3', run: () => switchTab('settings') },
  { label: '启动/暂停代理服务', kbd: 'S', run: () => $('#btn-toggle-service')?.click() },
  {
    label: '测试上游连通性',
    kbd: 'T',
    run: async () => {
      switchTab('settings');
      const { testUpstream } = await import('./settings.js');
      testUpstream($('#settings-test-result'));
    },
  },
  { label: '清空日志记录', kbd: 'C', run: () => $('#btn-clear-logs')?.click() },
  { label: '打开系统日志目录', kbd: 'O', run: () => invoke('open_logs_dir') },
];

/** `run` for a tab switch, injected to avoid importing main.js (a cycle). */
let switchTab = () => {};

/**
 * Build the overlay on first use.
 *
 * `.palette-overlay` starts closed and is opened by the `.open` class, so the
 * element can be created and left in the DOM from the first call onward.
 */
function ensure() {
  return overlay(ID, () => `
    <div class="palette-overlay" id="${ID}">
      <div class="palette">
        <input type="text" class="palette-input" id="${INPUT_ID}"
               placeholder="输入指令或搜索 (↑↓ 切换，Enter 执行，Esc 关闭)...">
        <div class="palette-list" id="${LIST_ID}"></div>
      </div>
    </div>
  `);
}

/** Rows matching the current query. */
function matches() {
  const q = $(`#${INPUT_ID}`)?.value.trim().toLowerCase() || '';
  return actions.filter(a => !q || a.label.toLowerCase().includes(q));
}

function render() {
  const list = $(`#${LIST_ID}`);
  if (!list) return;
  const filtered = matches();

  if (filtered.length === 0) {
    list.innerHTML = '<div class="palette-item" style="color: var(--muted);">未找到匹配项</div>';
    return;
  }

  if (selected >= filtered.length) selected = 0;

  list.innerHTML = filtered.map((item, idx) => html`
    <div class="palette-item ${idx === selected ? 'selected' : ''}" data-index="${idx}">
      <span>${item.label}</span>
      <kbd>${item.kbd}</kbd>
    </div>`).join('');

  list.querySelectorAll('.palette-item').forEach((el, idx) => {
    el.addEventListener('click', () => {
      const action = filtered[idx];
      close();
      action?.run();
    });
  });
}

export function open() {
  const el = ensure();
  if (!el) return;
  el.classList.add('open');
  const input = $(`#${INPUT_ID}`);
  if (input) input.value = '';
  selected = 0;
  render();
  input?.focus();
}

export function close() {
  $(`#${ID}`)?.classList.remove('open');
}

export function toggle() {
  if ($(`#${ID}`)?.classList.contains('open')) close();
  else open();
}

/** Wire the palette and its global shortcut. Called once, from main.js. */
export function initPalette(switchTabFn) {
  switchTab = switchTabFn;

  $('#btn-open-palette')?.addEventListener('click', open);

  const el = ensure();
  el?.addEventListener('click', (e) => {
    if (e.target === el) close();
  });

  const input = $(`#${INPUT_ID}`);
  input?.addEventListener('input', () => {
    selected = 0;
    render();
  });

  input?.addEventListener('keydown', (e) => {
    const filtered = matches();
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (filtered.length) {
        selected = (selected + 1) % filtered.length;
        render();
      }
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (filtered.length) {
        selected = (selected - 1 + filtered.length) % filtered.length;
        render();
      }
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const action = filtered[selected];
      if (action) {
        close();
        action.run();
      }
    } else if (e.key === 'Escape') {
      close();
    }
  });

  window.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
      e.preventDefault();
      toggle();
    }
  });
}
