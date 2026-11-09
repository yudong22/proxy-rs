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
import {
  isBrowserHostedConsole,
  renderList,
  renderSources,
  rowActionFromEvent,
  rowFromEvent,
  sessionDeepLink,
} from './list.js';
import { renderDetail, renderDetailMessage, renderTimelineBody } from './timeline.js';

const LIST_PAGE = 50;
const TIMELINE_PAGE = 200;

/**
 * The backend's session-id shape, checked here as well as in Rust.
 *
 * Defence in depth, not the boundary: the server validates every id it is given
 * (see `is_valid_session_id` in the Rust side). Doing it here too means a
 * malformed hash never reaches the backend at all — a deep link is untrusted
 * input, and "don't ask" is cheaper than "ask and be refused".
 */
const SESSION_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** Everything this view remembers between paints. */
const state = {
  list: null,
  offset: 0,
  limit: LIST_PAGE,
  sources: [],
  selected: null,
  detail: null,
  loadedOnce: false,
  isLoadingOlder: false,
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
          <div class="sess-detail-container" id="session-pane" hidden>
            <!-- 顶部极简导航头 -->
            <header class="sess-detail-topbar" id="session-detail-head"></header>

            <!-- 消息时间线滚动区：历史消息自上而下，滚到顶部可加载更多 -->
            <div class="sess-timeline-scroll" id="session-timeline-scroll">
              <div class="sess-load-more-bar" id="session-load-more-bar" hidden>
                <button type="button" class="btn btn-small sess-btn-load-more" id="btn-sessions-older">
                  ↑ 点击加载更早记录
                </button>
                <span class="sess-load-more-count" id="session-timeline-count"></span>
              </div>
              <div class="sess-top-hint" id="session-top-hint" hidden>
                <span>已到达会话开头</span>
              </div>
              <div class="sess-timeline" id="session-timeline"></div>
            </div>

            <!-- 底部参考输入框与状态行 -->
            <footer class="sess-footer" id="session-footer">
              <div class="sess-input-card">
                <textarea
                  class="sess-input-field"
                  id="sess-input-field"
                  rows="1"
                  placeholder="发消息或创建任务 , / 调用指令 , @ 文件或对话"
                ></textarea>
                <div class="sess-input-toolbar">
                  <div class="sess-input-toolbar-left">
                    <button type="button" class="sess-action-btn sess-action-plus" id="btn-sess-plus" title="操作">+</button>
                    <button type="button" class="sess-pill-btn" id="btn-sess-detail-trigger" title="展开会话详情与指标">
                      <span class="sess-pill-shield">🛡️</span>
                      <span class="sess-pill-label">会话详情</span>
                      <span class="sess-pill-chevron">▾</span>
                    </button>
                  </div>
                  <div class="sess-input-toolbar-right">
                    <button type="button" class="sess-model-pill" id="btn-sess-model-pill" title="使用的主要模型">
                      <span id="sess-active-model-name">模型未知</span>
                      <span class="sess-pill-chevron">▾</span>
                    </button>
                    <button type="button" class="sess-send-btn" id="btn-sess-send" title="发送 / 定位消息">
                      <svg viewBox="0 0 24 24" width="15" height="15" stroke="currentColor" stroke-width="2.5" fill="none" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="12" y1="19" x2="12" y2="5"></line>
                        <polyline points="5 12 12 5 19 12"></polyline>
                      </svg>
                    </button>
                  </div>
                </div>
              </div>

              <!-- 输入框下方小字状态栏 -->
              <div class="sess-status-bar" id="sess-status-bar" title="点击查看完整会话指标"></div>
            </footer>
          </div>

          <!-- 底部弹出的完整详情浮层 -->
          <div class="sess-pop-modal" id="sess-pop-modal" hidden>
            <div class="sess-pop-backdrop" id="sess-pop-backdrop"></div>
            <div class="sess-pop-dialog">
              <div class="sess-pop-header">
                <div class="sess-pop-title-wrap">
                  <span class="sess-pop-shield">🛡️</span>
                  <h3 class="sess-pop-title">会话详情与完整指标</h3>
                </div>
                <button type="button" class="btn-close sess-pop-close" id="btn-sess-pop-close" aria-label="关闭">&times;</button>
              </div>
              <div class="sess-pop-body" id="sess-pop-body"></div>
            </div>
          </div>

          <div class="sess-pane-empty" id="session-pane-empty">
            <div class="sess-empty">从左侧选择一个会话查看对话内容</div>
          </div>
        </section>
      </div>
    </div>
  `;
}

/** Open or close the session detail modal. */
function openDetailModal() {
  if (!state.detail) return;
  const modal = $('#sess-pop-modal');
  if (modal) {
    modal.removeAttribute('hidden');
    requestAnimationFrame(() => modal.classList.add('is-open'));
  }
}

function closeDetailModal() {
  const modal = $('#sess-pop-modal');
  if (modal) {
    modal.classList.remove('is-open');
    setTimeout(() => {
      if (!modal.classList.contains('is-open')) {
        modal.setAttribute('hidden', '');
      }
    }, 200);
  }
}

/**
 * Open one session in the machine's real browser (desktop only).
 *
 * The URL is the backend's business, not ours: the console port lives in
 * `ctx.console_port` in the Rust process, so any URL built here would be a guess
 * that silently breaks the moment the port changes. The command takes only the
 * id and returns the address it actually opened.
 *
 * The button is disabled for the duration so a double-click cannot spawn two
 * browser tabs; the flag is per-invocation, not per-row state, because the row
 * markup is re-rendered from the list on every selection change anyway.
 */
async function openInBrowser(id, trigger) {
  const disabledBefore = trigger?.disabled;
  if (trigger) trigger.disabled = true;
  try {
    await invoke('open_session_in_browser', { id });
    toast('已在浏览器打开');
  } catch (err) {
    // The backend's message is the whole point for the server console: it says
    // why this cannot work there and what to do instead. Rephrasing it here
    // would lose that.
    toast(String(err), 'error');
  } finally {
    if (trigger) trigger.disabled = disabledBefore ?? false;
  }
}

/**
 * Copy a session's deep link (server console only).
 *
 * `navigator.clipboard` needs a secure context; a console reached over plain
 * HTTP on a LAN address is not one, so there is a fallback rather than a claim of
 * success. The fallback is the deprecated `execCommand('copy')` path via a
 * throwaway textarea — it still works in exactly the non-secure context where
 * the modern API is unavailable. If even that is refused, the toast carries the
 * link so the operator can copy it by hand; nothing is ever reported as copied
 * when it was not.
 *
 * Returns whether the clipboard actually received the link, which makes the
 * three outcomes distinguishable in the toast.
 */
async function copyDeepLink(id) {
  const link = sessionDeepLink(id);
  if (await writeClipboard(link)) {
    toast('已复制会话链接');
    return true;
  }
  toast(`复制失败，请手动复制：${link}`, 'error');
  return false;
}

/** Put text on the clipboard, trying the modern API then the legacy command. */
async function writeClipboard(text) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* fall through to the legacy path */
  }
  const area = document.createElement('textarea');
  area.value = text;
  // Off-screen rather than `display: none`: a hidden element cannot be selected,
  // so `execCommand('copy')` would copy nothing.
  area.setAttribute('readonly', '');
  area.style.position = 'fixed';
  area.style.top = '-1000px';
  area.style.opacity = '0';
  document.body.appendChild(area);
  try {
    area.select();
    return document.execCommand('copy');
  } catch {
    return false;
  } finally {
    area.remove();
  }
}

/**
 * Route a `.sess-row-open` click to the action that flavour of console offers.
 *
 * One place reads the flavour, so the markup and the handler cannot disagree
 * about which action a click means. Both call sites (the sidebar rows and the
 * detail topbar) go through here for the same reason: the topbar carries the
 * same two flavours, and a second handler would be a second place to keep in
 * sync.
 */
function handleRowAction(e) {
  const btn = e.target.closest('.sess-row-open');
  if (!btn) return false;
  const hit = rowActionFromEvent(e);
  if (!hit) return true; // a shortcut with neither dataset is inert, never a row click
  if (hit.action === 'copy') copyDeepLink(hit.id);
  else openInBrowser(hit.id, btn);
  return true;
}

/** Helper: handle search or send action from input box. */
function handleInputSubmit() {
  const input = $('#sess-input-field');
  if (!input) return;
  const query = input.value.trim();
  if (!query) {
    toast('当前为本地归档会话，输入关键词可在对话中快速搜索定位');
    return;
  }

  // Find matching message turns in the transcript
  const turns = [...document.querySelectorAll('#session-timeline .sess-turn')];
  const matched = turns.filter((el) => el.textContent.toLowerCase().includes(query.toLowerCase()));

  if (matched.length > 0) {
    // Scroll the first matched entry into view
    matched[0].scrollIntoView({ behavior: 'smooth', block: 'center' });
    matched[0].classList.add('sess-highlight-pulse');
    setTimeout(() => matched[0].classList.remove('sess-highlight-pulse'), 2500);
    toast(`已找到 ${matched.length} 处相关记录，正在定位首处匹配`);
  } else {
    toast(`当前已加载的内容中未找到 "${query}"`);
  }
}

/** Wire. Called once by main.js. */
export function initSessions() {
  $('#sessions-rows')?.addEventListener('click', (e) => {
    // The per-row shortcut is handled first and returns: the row itself is a
    // sibling button, so without this a click on ↗ would fall through to the
    // row lookup below and also select the session (`closest('.sess-row')` is
    // null for it — but relying on that coincidence is how the next markup
    // change reintroduces a double action).
    if (handleRowAction(e)) return;
    const id = rowFromEvent(e);
    if (id) selectSession(id);
  });

  // The detail header is re-rendered on every selection, so its shortcut cannot
  // be bound at init time; delegation on the stable head element survives the
  // repaint. Bound here (not at document level) so it cannot collide with the
  // document-level `#btn-sess-topbar-stats` listener below, and routed through
  // the same handler as the rows so both flavours behave identically.
  $('#session-detail-head')?.addEventListener('click', handleRowAction);
  $('#btn-sessions-refresh')?.addEventListener('click', () => loadSessions({ force: true }));
  $('#btn-sessions-prev')?.addEventListener('click', () => {
    state.offset = Math.max(0, state.offset - LIST_PAGE);
    loadSessions({ force: true });
  });
  $('#btn-sessions-next')?.addEventListener('click', () => {
    state.offset += LIST_PAGE;
    loadSessions({ force: true });
  });

  // Top load-more button in timeline scroll
  $('#btn-sessions-older')?.addEventListener('click', () => loadOlder());

  // Triggers to open detail modal
  $('#btn-sess-detail-trigger')?.addEventListener('click', openDetailModal);
  $('#btn-sess-model-pill')?.addEventListener('click', openDetailModal);
  $('#sess-status-bar')?.addEventListener('click', openDetailModal);
  document.addEventListener('click', (e) => {
    if (e.target.closest('#btn-sess-topbar-stats')) {
      openDetailModal();
    }
  });

  // Close modal listeners
  $('#btn-sess-pop-close')?.addEventListener('click', closeDetailModal);
  $('#sess-pop-backdrop')?.addEventListener('click', closeDetailModal);
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeDetailModal();
  });

  // Input field keydown & send button
  $('#btn-sess-send')?.addEventListener('click', handleInputSubmit);
  $('#sess-input-field')?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleInputSubmit();
    }
  });
  $('#btn-sess-plus')?.addEventListener('click', () => {
    toast('支持在输入框输入关键词按回车搜索定位历史记录');
  });
}

/**
 * Read the list. Called on tab entry and by the toolbar buttons.
 */
export async function loadSessions({ force = false } = {}) {
  if (state.loadedOnce && !force) return;
  const seq = ++listSeq;
  try {
    const res = await invoke('claude_sessions_list', {
      args: { limit: state.limit, offset: state.offset },
    });
    if (seq !== listSeq) return;
    if (!res) {
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

/**
 * The session id named by `#sessions/<id>`, or null.
 *
 * Parsed here rather than in `main.js` because the hash is the only thing this
 * view needs from the URL: `main.js` already resolved the *tab*, and duplicating
 * the id rule in two files is how the router and the view drift apart.
 *
 * A malformed id is treated exactly like no deep link — an unknown UUID shape is
 * never sent to the backend (see `SESSION_ID_RE`).
 */
export function deepLinkSessionId() {
  const hash = window.location.hash.replace(/^#/, '');
  const [tab, id] = hash.split('/');
  if (tab.toLowerCase() !== 'sessions' || !id) return null;
  return SESSION_ID_RE.test(id) ? id : null;
}

/**
 * Apply the session named by the hash, if any.
 *
 * Called after the list has painted (from `applyTab`/`hashchange` in `main.js`),
 * because selecting before the rows exist would mark nothing and scroll nothing.
 * Selecting is not enough on its own: the row may be off-screen in the sidebar,
 * and a deep link that opens the right transcript while leaving the list
 * scrolled elsewhere looks like it did nothing.
 *
 * The id is looked up in the rows the list actually rendered, so a link to a
 * session outside the current page still selects it (the detail command reads by
 * id, not by row), and the scroll is a no-op rather than an error. That is
 * deliberate: paging the sidebar to chase a deep link would be a second,
 * invisible navigation the user did not ask for.
 */
export function consumeDeepLink() {
  const id = deepLinkSessionId();
  if (!id) return;
  if (id !== state.selected) selectSession(id);
  // Scrolled even when the session was already selected: the hash can change
  // (back/forward) without the selection changing, and the row may have been
  // pushed out of view by paging.
  const row = $(`#sessions-rows .sess-row[data-id="${id}"]`);
  row?.scrollIntoView({ block: 'nearest' });
}

/** Prepend the next-older page of the current session's timeline. */
async function loadOlder() {
  const id = state.selected;
  const current = state.detail;
  if (!id || !current || !current.has_more || state.isLoadingOlder) return;

  const btn = $('#btn-sessions-older');
  const scrollHost = $('#session-timeline-scroll');

  state.isLoadingOlder = true;
  if (btn) {
    btn.disabled = true;
    btn.textContent = '正在加载更早记录…';
  }

  const nextOffset = current.timeline_offset + current.timeline.length;
  const seq = detailSeq;
  try {
    const res = await invoke('claude_sessions_detail', {
      id,
      args: { offset: nextOffset, limit: TIMELINE_PAGE },
    });
    if (seq !== detailSeq || !res) return;

    // Snapshot scroll position before prepend so view doesn't jump
    const prevScrollHeight = scrollHost ? scrollHost.scrollHeight : 0;
    const prevScrollTop = scrollHost ? scrollHost.scrollTop : 0;

    // Older entries go on top; extend state detail
    state.detail = {
      ...current,
      timeline: res.timeline,
      has_more: res.has_more,
      timeline_offset: nextOffset,
    };
    renderTimelineBody(res, { replace: false });

    // Restore viewport position
    if (scrollHost) {
      const newScrollHeight = scrollHost.scrollHeight;
      scrollHost.scrollTop = prevScrollTop + (newScrollHeight - prevScrollHeight);
    }
  } catch (err) {
    toast('加载更早的记录失败: ' + err, 'error');
  } finally {
    state.isLoadingOlder = false;
    if (btn) {
      btn.disabled = false;
      btn.textContent = '↑ 点击加载更早记录';
    }
  }
}
