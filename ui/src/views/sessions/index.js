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
 * `invoke('sessions_*')` — the same command names the server console dispatches
 * over `/admin/api/rpc/*`, so this view is identical in Tauri and in the
 * browser. They were `claude_sessions_*` before 3.5.6; the old names still answer
 * as aliases, but this view uses the new ones because the page is no longer
 * Claude-Code-only. The HTTP-first rule from `core/api.js` does not apply: these
 * are not the two observation endpoints, and the local files are not reachable
 * over `/api/*` in server mode by design.
 *
 * ## Where "which session" lives
 *
 * Two places with two different jobs, and the split is load-bearing: the URL is
 * the address (shareable, bookmarkable, survives a reload), `core/prefs.js` is the
 * memory (survives a tab switch, where the hash is necessarily overwritten by
 * `#logs`). The URL wins whenever it names a session; storage only fills in when
 * it does not. See `restoreLastSession`.
 */

import { $, html } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { getLastSession, setLastSession, clearLastSession } from '../../core/prefs.js';
import { toast } from '../../components/toast.js';
import {
  expandGroupOfRow,
  expandGroupRows,
  groupFromEvent,
  groupMoreFromEvent,
  isBrowserHostedConsole,
  renderList,
  renderSources,
  rowActionFromEvent,
  rowFromEvent,
  sessionDeepLink,
  setGroupCollapsed,
} from './list.js';
import { renderDetail, renderDetailMessage, renderTimelineBody } from './timeline.js';

const LIST_PAGE = 50;
const TIMELINE_PAGE = 200;

/**
 * How many sessions each project group returns.
 *
 * Ten is the number that keeps the grouped list as cheap as the flat one on real
 * data (this machine's group sizes are `[10, 8, 4, 2, 2, 1, 1, 1, 1, 1]`, so
 * groups of ten open exactly the same files `limit=50` opens today). It is a
 * measurement, not a guess about screen height; the rest of a bigger group is one
 * click away on its own "显示其余" row.
 */
const GROUP_LIMIT = 10;

/**
 * The backend's session-id shape (Claude Code), checked here as well as in Rust.
 *
 * Defence in depth, not the boundary: the server validates every id it is given
 * (see `is_valid` in the Rust side). Doing it here too means a malformed hash
 * never reaches the backend at all — a deep link is untrusted input, and "don't
 * ask" is cheaper than "ask and be refused".
 *
 * It is also *per source*: an id is only meaningful inside the source that issued
 * it, which is why the address carries the source as its own segment instead of
 * letting this regex decide which source a bare id belongs to.
 */
const SESSION_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * The source segment's shape, and the sources this build knows how to *address*.
 *
 * An enum, not a character set: `sessions/<anything>/<id>` must not become a
 * free-form string in the address bar, and a source this build cannot read has no
 * id rule to apply. A source that becomes readable later joins this table and
 * `ID_RULES` together — the two lists are separate on purpose, because the second
 * one is the answer to "what may this source's id look like".
 */
const SOURCE_SLUGS = ['claude'];
const ID_RULES = { claude: SESSION_ID_RE };

/** The source assumed by a two-segment address (`#sessions/<uuid>`). */
const DEFAULT_SOURCE = 'claude';

/**
 * Everything this view remembers between paints.
 *
 * `source` is the source filter in force (null = every readable source, which on
 * this build is the same row set as `'claude'`); `collapsed` is the set of group
 * keys the user folded shut, in memory only — folding is an optimisation, and
 * persisting it would mean opening the page to a list with nothing visible.
 */
const state = {
  list: null,
  offset: 0,
  limit: LIST_PAGE,
  sources: [],
  source: null,
  collapsed: new Set(),
  selected: null,
  /** Why the list is empty, as the last list read explained it. Kept so a later
   *  re-render (a selection, a paging step) does not trade "没有找到 Claude Code 会话"
   *  for the generic wording — the difference between "none" and "could not read the
   *  directory" is the whole message, and a repaint must not erase it. */
  emptyMessage: '',
  /** The source of `selected`. Kept beside it because paging the timeline (and
   *  the modal's subagent reads) need the same `{source, id}` pair the selection
   *  was opened with — re-deriving it from the hash would break the moment the
   *  user switches to another tab, which clears the session out of the hash. */
  selectedSource: DEFAULT_SOURCE,
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
async function openInBrowser(source, id, trigger) {
  const disabledBefore = trigger?.disabled;
  if (trigger) trigger.disabled = true;
  try {
    // The source is a separate argument, not baked into the id: the backend
    // resolves it through its registry and validates the id against *that*
    // source's rule before it builds any path. Omitting it would mean "the default
    // source", which is right only as long as there is one.
    await invoke('open_session_in_browser', { source: source || DEFAULT_SOURCE, id });
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
async function copyDeepLink(source, id) {
  const link = sessionDeepLink(source, id);
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
  if (hit.action === 'copy') copyDeepLink(hit.source, hit.id);
  else openInBrowser(hit.source, hit.id, btn);
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
    // Order matters and is part of the contract: the group controls are handled
    // first (a `data-group-key` is not a session id and a header is not a row),
    // then the per-row shortcut, then the row itself. The shortcut is a *sibling*
    // of the row (HTML forbids nesting one button in another), so without the
    // early return a click on ↗ would fall through to the row lookup and also
    // select the session.
    const group = groupFromEvent(e);
    if (group) {
      if (group.collapsedNow) state.collapsed.add(group.key);
      else state.collapsed.delete(group.key);
      setGroupCollapsed(group.key, group.collapsedNow);
      return;
    }    const more = groupMoreFromEvent(e);
    if (more) {
      expandGroup(more);
      return;
    }
    if (handleRowAction(e)) return;
    const id = rowFromEvent(e);
    // The source travels with the click: an id is unique within its source, not
    // across sources, and a row can carry a source that is not the filter in force
    // (the filter is optional).
    if (id) selectSession(id, rowSourceFromEvent(e));
  });

  // Source segments. Bound as delegation on the stable toolbar host because
  // `renderSources` rewrites its children on every list load — a listener bound to
  // the buttons themselves would be discarded by the next refresh.
  //
  // A disabled segment never reaches this: the browser dispatches no click from a
  // `disabled` button, and the check below is the second half of the same rule for
  // the day a segment is `aria-disabled` instead.
  $('#sessions-sources')?.addEventListener('click', (e) => {
    const btn = e.target.closest('.btn-toggle');
    if (!btn || btn.disabled) return;
    const slug = btn.dataset.slug || '';
    if (!slug) return;
    // Clicking the lit segment again clears the filter. That matters on the first
    // day there is a second readable source: "only Claude" and "everything" have
    // to be distinguishable, or the merged view becomes unreachable.
    const next = state.source === slug ? null : slug;
    if (next === state.source) return;
    state.source = next;
    state.offset = 0;
    loadSessions({ force: true });
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
 * The row's source, read from the row's own shortcut.
 *
 * The row's `data-id` deliberately stays a bare id (it is the selection key and
 * what the sidebar's "is this row selected" test compares), so the source is
 * carried by the sibling `.sess-row-open` — the same element that already knows
 * which action it offers. Falling back to the filter in force is exact rather than
 * a guess: the backend defaults a missing source to the only readable one, which
 * is what a bare `#sessions/<uuid>` address means too.
 */
function rowSourceFromEvent(event) {
  const row = event.target.closest('.sess-row');
  const shortcut = row?.parentElement?.querySelector('.sess-row-open');
  return shortcut?.dataset.openSource
    || shortcut?.dataset.copySource
    || state.source
    || DEFAULT_SOURCE;
}

/**
 * "显示其余 N 个" for one group: re-read just that group and splice its rows in.
 *
 * Only the group is re-read (`group=<key>`, `group_limit=<count>`), not the whole
 * page, so nothing else on screen is re-rendered and the reader keeps their scroll
 * position. `count` is the number the header already claims, so one click produces
 * exactly the group the header promised.
 */
async function expandGroup({ key, count }) {
  if (!key) return;
  try {
    const res = await invoke('sessions_list', {
      args: {
        group_by: 'project',
        group: key,
        group_limit: Math.min(Math.max(count || GROUP_LIMIT, 1), 200),
        limit: state.limit,
        offset: 0,
        ...(state.source ? { source: state.source } : {}),
      },
    });
    if (!res) return;
    const group = Array.isArray(res.groups) ? res.groups.find((g) => g.key === key) : null;
    if (!group) return;
    expandGroupRows(group);
    // Keep the cached page honest: a later re-render (selection, refresh) rebuilds
    // from `state.list`, so the group it paints has to be the expanded one.
    if (Array.isArray(state.list?.groups)) {
      state.list.groups = state.list.groups.map((g) => (g.key === key ? group : g));
    }
  } catch (err) {
    toast('加载该项目的其余会话失败: ' + err, 'error');
  }
}

/**
 * Read the list. Called on tab entry and by the toolbar buttons.
 *
 * Always grouped by project: the two-level list *is* the 3.5.6 shape of this page.
 * The flat form stays on the wire for REST scripts, and `renderList` still knows
 * how to paint it for a cached bundle or a mock that omits `groups`.
 */
export async function loadSessions({ force = false } = {}) {
  if (state.loadedOnce && !force) return;
  const seq = ++listSeq;
  const args = {
    group_by: 'project',
    group_limit: GROUP_LIMIT,
    limit: state.limit,
    offset: state.offset,
  };
  if (state.source) args.source = state.source;
  try {
    const res = await invoke('sessions_list', { args });
    if (seq !== listSeq) return;
    if (!res) {
      renderList(null, { empty: '当前环境没有会话接口' });
      return;
    }
    state.loadedOnce = true;
    state.list = res;
    state.sources = res.sources || [];
    state.emptyMessage = res.available
      ? '没有找到 Claude Code 会话'
      : res.reason || '没有可用的会话来源';
    renderSources(state.sources, { active: state.source });
    const root = $('#sessions-root');
    if (root) {
      root.textContent = res.available ? `根目录：${res.root || ''}` : '';
      root.title = res.available ? res.root || '' : res.reason || '';
    }
    renderList(res, {
      selected: state.selected,
      collapsed: state.collapsed,
      empty: state.emptyMessage,
    });
  } catch (err) {
    if (seq !== listSeq) return;
    toast('加载会话列表失败: ' + err, 'error');
    renderList(null, { empty: '加载失败：' + err });
  }
}

/**
 * Open one session (the newest page of its timeline).
 *
 * The order of the steps after the request is the interesting part, and it is
 * deliberate: the highlight and the detail read come first (a click must feel
 * instant), and the address and the remembered session are written **only after
 * the read succeeded**. Writing them up front would let a session that cannot be
 * read (its file was deleted, its source went away) be branded into the address
 * bar *and* into storage — so every reload would fail again on the same dead id and
 * a transient failure would become "this page is broken".
 *
 * `silent` is for the restore path: when storage points at a session that is gone,
 * the user never asked for it, so the failure must not raise an error toast. It
 * clears the key and leaves the pane's own explanation instead.
 */
export async function selectSession(
  id,
  source = DEFAULT_SOURCE,
  { silent = false, writeAddress = true } = {},
) {
  state.selected = id;
  const slug = source || DEFAULT_SOURCE;
  state.selectedSource = slug;
  renderList(state.list, {
    selected: id,
    collapsed: state.collapsed,
    empty: state.emptyMessage,
  });
  const seq = ++detailSeq;
  try {
    const res = await invoke('sessions_detail', {
      source: slug,
      id,
      args: { offset: 0, limit: TIMELINE_PAGE },
    });
    if (seq !== detailSeq) return;
    if (!res) {
      renderDetailMessage('当前环境没有会话接口');
      return;
    }
    state.detail = res;
    renderDetail(res, { source: slug });
    // Success: only now may the address and the memory point here. `replaceState`
    // rather than `location.hash =` for two reasons: ten clicks must not add ten
    // history entries, and it fires no `hashchange` — so writing the address cannot
    // loop back into `consumeDeepLink`. That is exactly why there is no re-entry
    // flag here; adding one would only hide a later attempt to use `hash =`.
    //
    // `writeAddress` is false for the two paths the user did not navigate: arriving
    // on a deep link (the address they gave is not ours to rewrite) and restoring a
    // remembered session (the address has no session by design, and writing one
    // back would make "switch tab and return" look like a navigation). The memory
    // is still updated in both — that is what makes the *next* tab switch return
    // here instead of to a stale session.
    if (writeAddress) {
      const href = `#sessions/${slug}/${id}`;
      if (window.location.hash !== href) window.history.replaceState(null, '', href);
    }
    setLastSession({ source: slug, id });
  } catch (err) {
    if (seq !== detailSeq) return;
    renderDetailMessage('打开会话失败：' + err);
    if (silent) clearLastSession();
    else toast('打开会话失败: ' + err, 'error');
  }
}

/**
 * The session named by the address, as `{ source, id }`, or null.
 *
 * Parsed here rather than in `main.js` because the hash is the only thing this
 * view needs from the URL: `main.js` already resolved the *tab*, and duplicating
 * the id rule in two files is how the router and the view drift apart.
 *
 * Two shapes, and the reason the old one is not deprecated:
 *
 * - `#sessions/<uuid>` — everything 3.5.5 ever handed out (copied links,
 *   `open_session_in_browser` addresses, bookmarks). It **always** means `claude`.
 *   Guessing the source from the id's shape would be worse than a rule: Codex ids
 *   are UUIDs too, so one id could resolve to two different sessions.
 * - `#sessions/<source>/<id>` — the current shape. The source must be in the
 *   enum and the id must pass *that source's* rule, in that order: an unknown
 *   source has no id rule to apply, so validating the id first would be validating
 *   against nothing.
 *
 * Anything else — a malformed id, an unknown source, a fourth segment — is
 * exactly like no deep link at all: never sent to the backend.
 */
export function deepLinkSessionId() {
  const hash = window.location.hash.replace(/^#/, '');
  const [tab, second, third] = hash.split('/');
  if (!tab || tab.toLowerCase() !== 'sessions' || !second) return null;
  if (third === undefined) {
    // Legacy two-segment form. `second` is the id.
    return SESSION_ID_RE.test(second) ? { source: DEFAULT_SOURCE, id: second } : null;
  }
  // Three-segment form. A fourth segment means someone appended to a link we
  // never generate, which is a malformed link rather than one to be charitable
  // about.
  if (hash.split('/').length !== 3) return null;
  const source = second;
  if (!SOURCE_SLUGS.includes(source)) return null;
  const rule = ID_RULES[source];
  return rule && rule.test(third) ? { source, id: third } : null;
}

/** Scroll the row for `id` into view, unfolding its group first if it was folded. */
function revealRow(id) {
  expandGroupOfRow(id);
  const row = $(`#sessions-rows .sess-row[data-id="${id}"]`);
  row?.scrollIntoView({ block: 'nearest' });
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
  const link = deepLinkSessionId();
  if (!link) return;
  // `writeAddress: false`: the address already says this; rewriting it would turn a
  // legacy two-segment link into the three-segment one under the user's cursor.
  if (link.id !== state.selected) {
    selectSession(link.id, link.source, { writeAddress: false });
  } else {
    // Already open — but this *is* the session the user is now looking at (they may
    // have arrived by a pasted link), so the memory must follow the address. Without
    // this, a tab switch would return to whatever the previous session was.
    setLastSession({ source: link.source, id: link.id });
  }
  // Scrolled even when the session was already selected: the hash can change
  // (back/forward) without the selection changing, and the row may have been
  // pushed out of view by paging — or be inside a group the reader folded shut.
  revealRow(link.id);
}

/**
 * Fall back to the session this browser last opened, when the address names none.
 *
 * Runs **after** `consumeDeepLink`, never instead of it: the URL is the address
 * and wins whenever it says anything. The guard below states that in one line
 * rather than relying on call order, so the two truths cannot silently swap.
 *
 * This exists for the tab-switch case, which the URL cannot cover: switching to
 * 日志 rewrites the hash to `#logs` (`switchTab`), so the id is gone by the time
 * the user comes back. Coming back re-runs `applyTab('sessions')`, which is where
 * this is called from.
 *
 * A record pointing at a session that is gone is the expected outcome of "the file
 * was deleted", not an error state: the read fails, the key is cleared, and the
 * empty pane explains itself without a toast the user did not cause.
 */
export function restoreLastSession() {
  if (deepLinkSessionId() || state.selected) return;
  const last = getLastSession();
  if (!last) return;
  selectSession(last.id, last.source, { silent: true, writeAddress: false });
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
  const source = state.selectedSource;
  try {
    const res = await invoke('sessions_detail', {
      source,
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
