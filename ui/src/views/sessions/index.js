/**
 * 会话 view: local Claude Code conversations on the left, transcript on the right.
 *
 * ## Why no polling
 *
 * Sessions are files that change only while a conversation is running. A timer
 * would re-read 50 transcripts every few seconds to learn nothing — and the
 * one session that *is* changing is rarely the one being read. A toolbar
 * 刷新 button is the explicit refresh.
 *
 * ## Stale responses
 *
 * Clicking quickly through the list fires several detail reads; a slow first
 * one must not paint over the second. Every request takes a sequence number and
 * only the newest is allowed to render.
 *
 * ## Where data comes from
 *
 * `invoke('claude_sessions_*')` — the same command names the server console
 * dispatches over `/admin/api/rpc/*`, so this view is identical in Tauri and in
 * the browser. The HTTP-first rule from `core/api.js` does not apply: these are
 * not the two observation endpoints, and the local files are not reachable over
 * `/api/*` in server mode by design.
 */

import { $, html } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { toast } from '../../components/toast.js';
import { renderList, renderSources, rowFromEvent } from './list.js';
import { renderDetail, renderDetailMessage, renderTimelineBody } from './timeline.js';

const LIST_PAGE = 50;
const TIMELINE_PAGE = 200;

/** Everything this view remembers between paints. */
const state = {
  list: null,
  offset: 0,
  limit: LIST_PAGE,
  sources: [],
  selected: null,
  detail: null,
  loadedOnce: false,
};
let listSeq = 0;
let detailSeq = 0;

/** Build the pane. Called once by main.js, before `initSessions`. */
export function renderSessions() {
  const root = $('#tab-sessions');
  if (!root) return;
  root.innerHTML = html`
    <div class="logs-panel sess-panel">
      <div class="logs-toolbar">
        <div class="view-mode-group sess-sources" id="sessions-sources"></div>
        <span class="log-count" id="sessions-root"></span>
        <div class="logs-actions">
          <span class="log-count" id="sessions-paging-info"></span>
          <button class="btn btn-small" id="btn-sessions-refresh" title="重新读取会话列表">刷新</button>
        </div>
      </div>

      <div class="sess-split">
        <aside class="sess-sidebar">
          <div class="sess-rows" id="sessions-rows">
            <div class="sess-empty">正在读取会话…</div>
          </div>
          <div class="sess-pager">
            <button class="btn btn-small" id="btn-sessions-prev" disabled>上一页</button>
            <button class="btn btn-small" id="btn-sessions-next" disabled>下一页</button>
          </div>
        </aside>

        <section class="sess-detail">
          <div class="sess-detail-scroll" id="session-pane" hidden>
            <header class="sess-detail-head" id="session-detail-head"></header>
            <div class="sess-timeline-bar">
              <button class="btn btn-small" id="btn-sessions-older" hidden>加载更早</button>
              <span class="log-count" id="session-timeline-count"></span>
            </div>
            <div class="sess-timeline" id="session-timeline"></div>
          </div>
          <div class="sess-pane-empty" id="session-pane-empty">
            <div class="sess-empty">从左侧选择一个会话查看对话内容</div>
          </div>
        </section>
      </div>
    </div>
  `;
}

/** Wire. Called once by main.js. */
export function initSessions() {
  $('#sessions-rows')?.addEventListener('click', (e) => {
    const id = rowFromEvent(e);
    if (id) selectSession(id);
  });
  $('#btn-sessions-refresh')?.addEventListener('click', () => loadSessions({ force: true }));
  $('#btn-sessions-prev')?.addEventListener('click', () => {
    state.offset = Math.max(0, state.offset - LIST_PAGE);
    loadSessions({ force: true });
  });
  $('#btn-sessions-next')?.addEventListener('click', () => {
    state.offset += LIST_PAGE;
    loadSessions({ force: true });
  });
  $('#btn-sessions-older')?.addEventListener('click', () => loadOlder());
}

/**
 * Read the list. Called on tab entry and by the toolbar buttons.
 *
 * `force` skips the once-loaded shortcut; entering the tab re-reads only if
 * nothing has been loaded yet in this page's lifetime, so switching back and
 * forth does not re-scan the home directory.
 */
export async function loadSessions({ force = false } = {}) {
  if (state.loadedOnce && !force) return;
  const seq = ++listSeq;
  try {
    const res = await invoke('claude_sessions_list', {
      args: { limit: state.limit, offset: state.offset },
    });
    if (seq !== listSeq) return; // a newer request won
    if (!res) {
      // No bridge (a plain-browser preview): say so rather than spin.
      renderList(null, { empty: '当前环境没有会话接口' });
      return;
    }
    state.loadedOnce = true;
    state.list = res;
    state.sources = res.sources || [];
    renderSources(state.sources);
    const root = $('#sessions-root');
    if (root) {
      root.textContent = res.available ? `根目录：${res.root || ''}` : '';
      root.title = res.available ? res.root || '' : res.reason || '';
    }
    renderList(res, {
      selected: state.selected,
      empty: res.available
        ? '没有找到 Claude Code 会话'
        : res.reason || '没有可用的会话来源',
    });
  } catch (err) {
    if (seq !== listSeq) return;
    toast('加载会话列表失败: ' + err, 'error');
    renderList(null, { empty: '加载失败：' + err });
  }
}

/** Open one session (the newest page of its timeline). */
export async function selectSession(id) {
  state.selected = id;
  renderList(state.list, { selected: id });
  const seq = ++detailSeq;
  try {
    const res = await invoke('claude_sessions_detail', {
      id,
      args: { offset: 0, limit: TIMELINE_PAGE },
    });
    if (seq !== detailSeq) return;
    if (!res) {
      renderDetailMessage('当前环境没有会话接口');
      return;
    }
    state.detail = res;
    renderDetail(res);
  } catch (err) {
    if (seq !== detailSeq) return;
    renderDetailMessage('打开会话失败：' + err);
    toast('打开会话失败: ' + err, 'error');
  }
}

/** Prepend the next-older page of the current session's timeline. */
async function loadOlder() {
  const id = state.selected;
  const current = state.detail;
  if (!id || !current || !current.has_more) return;
  const nextOffset = current.timeline_offset + current.timeline.length;
  const seq = detailSeq; // do not clobber a different session being opened
  try {
    const res = await invoke('claude_sessions_detail', {
      id,
      args: { offset: nextOffset, limit: TIMELINE_PAGE },
    });
    if (seq !== detailSeq || !res) return;
    // Older entries go on top; the header figures stay exact from the first
    // call, so only the timeline body is extended.
    state.detail = { ...current, timeline: res.timeline, has_more: res.has_more, timeline_offset: nextOffset };
    renderTimelineBody(res, { replace: false });
  } catch (err) {
    toast('加载更早的记录失败: ' + err, 'error');
  }
}
