/**
 * 会话页的两级列表与「记住上次打开的会话」。
 *
 * ## Why these are separate from `scenarios-views.checkSessions`
 *
 * `checkSessions` asserts the *shape* of one page: one header, one nested row, the
 * transcript. These two drive state that outlives a single paint — folding a
 * group, asking one group for the rest of its rows, and leaving/returning to the
 * tab — and each of them needs its own mock or its own reload, which would make
 * `checkSessions` unreadable.
 *
 * ## What they are allowed to reach for
 *
 * Both are written against what a user or a browser does: click the header, click
 * "显示其余", click a row, switch tabs, reload. No module is imported, so the same
 * assertions hold against the Vite build (`UI_DIR=ui-dist`), where the module
 * graph has been bundled and a test's own `import` would 404.
 */

/** The one session in `fixtures.mjs`, visible in this file's failure messages. */
const FIXTURE_SESSION_ID = 'b9c3800a-311e-4c89-9a38-c533d8969b5d';

/** The group the fixture describes: one project, one session. */
const FIXTURE_GROUP_KEY = 'claude:-Users-test-project';

/**
 * A grouped list with three projects, one of which is bigger than `group_limit`.
 *
 * Deliberately a *second* fixture rather than a bigger one in `fixtures.mjs`: the
 * shared mock is what `checkSessions` measures a one-row list against, and a
 * multi-group mock there would turn that scenario's "exactly one row" into a
 * brittle count of rows nobody cares about. This one lives here because only this
 * scenario needs more than one group.
 */
function groupedOverride() {
  const row = (id, title, lastSeen) => ({
    source: 'claude',
    id,
    encoded_project: '-Users-test-alpha',
    cwd: '/Users/test/alpha',
    title,
    first_seen_ms: lastSeen - 60_000,
    last_seen_ms: lastSeen,
    file_mtime_ms: lastSeen,
    size_bytes: 1024,
    git_branch: 'main',
    version: '2.1.277',
    turns: 1,
    assistant_turns: 1,
    tool_calls: 0,
    subagents: 0,
    last_model: 'deepseek-v4.1-flash',
    counts_partial: false,
    gateway: null,
    gateway_error: null,
  });
  const now = Date.now();
  const alphaRows = [
    row('11111111-1111-4111-8111-111111111111', 'alpha 最新', now - 60_000),
    row('22222222-2222-4222-8222-222222222222', 'alpha 次新', now - 120_000),
  ];
  return {
    available: true,
    reason: null,
    root: '/Users/test/.claude/projects',
    sources: [
      { kind: 'claude_local', slug: 'claude', label: 'Claude Code', available: true, note: null },
      { kind: 'dsh_local', slug: 'dsh', label: 'DSH', available: false, note: 'DSH 会话是 zstd 压缩的本地存档，暂不支持浏览' },
    ],
    stats_available: true,
    gateway_error: null,
    total: 6,
    offset: 0,
    limit: 50,
    group_by: 'project',
    groups: [
      {
        key: FIXTURE_GROUP_KEY, label: 'alpha', cwd: '/Users/test/alpha', sources: ['claude'],
        // Larger than the two rows returned, so `more` is true and the
        // "显示其余" control must appear.
        count: 4,
        last_seen_ms: now - 60_000,
        size_bytes: 4096,
        more: true,
        sessions: alphaRows,
      },
      {
        key: 'claude:-Users-test-beta', label: 'beta', cwd: '/Users/test/beta', sources: ['claude'],
        count: 2, last_seen_ms: now - 3_600_000, size_bytes: 2048, more: false,
        sessions: [
          row('33333333-3333-4333-8333-333333333333', 'beta 之一', now - 3_600_000),
          row('44444444-4444-4444-8444-444444444444', 'beta 之二', now - 7_200_000),
        ],
      },
      {
        // A group whose directory yielded no session: 3 of this machine's 13
        // project directories are like this, and rendering them would push the
        // real sessions off screen.
        key: 'claude:-Users-test-empty', label: 'empty', cwd: '/Users/test/empty', sources: ['claude'],
        count: 0, last_seen_ms: 0, size_bytes: 0, more: false, sessions: [],
      },
    ],
    sessions: [],
  };
}

/**
 * Folding and "show the rest" are per-group, in place, and must not repaint the
 * rest of the list.
 */
export async function checkSessionsGrouping(ctx) {
  const { send, check, origin } = ctx;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  const grouped = groupedOverride();
  // The "show the rest" click asks for `group=<key>&group_limit=<count>`, which the
  // static payload above cannot answer (it is not a page of anything). A
  // function-valued override is what makes that request observable at all: it
  // returns the same group with its full row list, exactly as the backend would.
  const { identifier } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `window.__MOCK_OVERRIDE__ = { sessions_list: (args) => {
      const base = ${JSON.stringify(grouped)};
      const group = (args && args.args && args.args.group) || '';
      if (!group) return base;
      const only = base.groups.find((g) => g.key === group);
      if (!only) return base;
      return Object.assign({}, base, {
        total: only.count,
        groups: [Object.assign({}, only, { more: false, sessions: only.sessions.concat([
          Object.assign({}, only.sessions[0], { id: 'aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa', title: 'alpha 其余一' }),
          Object.assign({}, only.sessions[0], { id: 'bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb', title: 'alpha 其余二' }),
        ]) })],
      });
    } };`,
  });
  // Navigate to the bare document first: a hash-only navigation on the current
  // URL is a same-document one, which would never install the override.
  await send('Page.navigate', { url: `${origin}/index.html` });
  await sleep(300);
  await send('Runtime.evaluate', { expression: `window.switchTab('sessions')` });
  await sleep(1200);

  const initial = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(() => {
      const heads = [...document.querySelectorAll('#sessions-rows .sess-group-head')];
      return {
        heads: heads.length,
        labels: heads.map((h) => h.querySelector('.sess-group-label')?.textContent || ''),
        counts: heads.map((h) => h.querySelector('.sess-group-count')?.textContent || ''),
        rows: document.querySelectorAll('#sessions-rows .sess-row').length,
        emptyGroupRendered: heads.some((h) => h.dataset.groupKey === 'claude:-Users-test-empty'),
        moreButtons: [...document.querySelectorAll('#sessions-rows .sess-group-more')]
          .map((b) => b.textContent.replace(/\\s+/g, ' ').trim()),
        // Every group must report its whole size, not the number of rows it was
        // able to hand back -- that is what makes the header honest.
        metaCount: document.querySelectorAll('#sessions-rows .sess-group-meta').length,
        pagerHidden: document.querySelector('.sess-pager')?.hasAttribute('hidden') ?? null,
        firstAsk: window.__INVOKE_ARGS__?.sessions_list?.args || null,
      };
    })()`,
  });
  const v = initial.result.value;

  // What the page asks for is the contract: grouping on, ten per group, no offset.
  check('sessions list asks for project grouping', v.firstAsk?.group_by, 'project');
  check('sessions list asks for ten sessions per group', v.firstAsk?.group_limit, 10);
  check('grouped list renders one header per non-empty group', v.heads, 2);
  check('group headers are ordered by recent activity', v.labels, ['alpha', 'beta']);
  check('group headers show the exact group size', v.counts, ['4', '2']);
  check('an empty group is not rendered', v.emptyGroupRendered, false);
  check('grouped rows are the rows actually returned', v.rows, 4);
  check('every group carries a relative-time/size line', v.metaCount, 2);
  check('a truncated group offers "show the rest"', v.moreButtons, ['显示其余 2 个（共 4 个）']);
  check('the global pager stays hidden in grouped mode', v.pagerHidden, true);

  // Fold the first group. The rows must stay in the document (hidden, not
  // discarded): a deep link can point at one, and re-rendering the whole list
  // would lose the reader's scroll position.
  const folded = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => { try {
      const head = document.querySelectorAll('#sessions-rows .sess-group-head')[0];
      head.click();
      await new Promise((r) => setTimeout(r, 100));
      const body = document.querySelector('#sessions-rows .sess-group-body[data-group-key="claude:-Users-test-project"]');
      const caret = head.querySelector('.sess-group-caret')?.textContent;
      const expanded = head.getAttribute('aria-expanded');
      const rowsInDocument = body?.querySelectorAll('.sess-row').length ?? 0;
      const secondBody = document.querySelector('#sessions-rows .sess-group-body[data-group-key="claude:-Users-test-beta"]');
      // Read the folded state *before* clicking again: the second click is what
      // proves it can be undone, and it would otherwise be measured as "not folded".
      const bodyHidden = body?.hasAttribute('hidden') ?? null;
      const secondBodyHidden = secondBody?.hasAttribute('hidden') ?? null;
      // Fold, then unfold: one click each way must land back where it started.
      head.click();
      await new Promise((r) => setTimeout(r, 100));
      return {
        bodyHidden,
        caret,
        expanded,
        rowsInDocument,
        secondBodyHidden,
        reopened: !(body?.hasAttribute('hidden') ?? true),
        reopenedCaret: head.querySelector('.sess-group-caret')?.textContent,
      };
    } catch (e) { return { __err: String(e && e.stack || e) }; } })()`,
  });
  const f = folded.result.value;
  check('clicking a group header folds it', f.bodyHidden, true);
  check('the folded group says so on its caret', f.caret, '▸');
  check('the folded group reports aria-expanded=false', f.expanded, 'false');
  check('folding keeps the rows in the document', f.rowsInDocument, 2);
  check('folding one group leaves the other open', f.secondBodyHidden, false);
  check('clicking again unfolds the group', f.reopened, true);
  check('the unfolded group says so on its caret', f.reopenedCaret, '▾');

  // "显示其余": re-reads only that group and splices its rows in.
  const expandedGroup = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const before = window.__CALLS__.length;
      const more = document.querySelector('#sessions-rows .sess-group-more');
      more.click();
      await new Promise((r) => setTimeout(r, 300));
      const groupCalls = window.__CALLS__.slice(before);
      const body = document.querySelector('#sessions-rows .sess-group-body[data-group-key="claude:-Users-test-project"]');
      const betaBody = document.querySelector('#sessions-rows .sess-group-body[data-group-key="claude:-Users-test-beta"]');
      return {
        called: groupCalls.filter((c) => c === 'sessions_list').length,
        moreGone: !document.querySelector('#sessions-rows .sess-group-more'),
        rowsInGroup: body?.querySelectorAll('.sess-row').length ?? 0,
        // The other group must not be rebuilt: its own rows are untouched.
        betaRows: betaBody?.querySelectorAll('.sess-row').length ?? 0,
      };
    })()`,
  });
  const g = expandedGroup.result.value;
  check('"show the rest" re-reads just that group', g.called, 1);
  check('"show the rest" disappears once the group is complete', g.moreGone, true);
  check('"show the rest" fills the group to its stated size', g.rowsInGroup, 4);
  check('"show the rest" leaves the other group alone', g.betaRows, 2);

  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier });

  // The degradation path: a response without `groups` (an older backend, a cached
  // bundle, a script's flat list) must still paint. Whitelisting this in the
  // contract is only worth anything if something proves it.
  const { identifier: flatId } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `window.__MOCK_OVERRIDE__ = { sessions_list: {
      available: true, reason: null, root: '/Users/test/.claude/projects',
      sources: [{ kind: 'claude_local', slug: 'claude', label: 'Claude Code', available: true, note: null }],
      stats_available: true, gateway_error: null,
      total: 2, offset: 0, limit: 50,
      group_by: null, groups: null,
      sessions: [
        { source: 'claude', id: '55555555-5555-4555-8555-555555555555',
          encoded_project: '-Users-test-flat', cwd: '/Users/test/flat', title: '扁平一',
          first_seen_ms: 1794000000000, last_seen_ms: 1794000100000, file_mtime_ms: 1794000100000,
          size_bytes: 1024, git_branch: 'main', version: '2.1.277', turns: 1, assistant_turns: 1,
          tool_calls: 0, subagents: 0, last_model: 'deepseek-v4.1-flash', counts_partial: false,
          gateway: null, gateway_error: null },
        { source: 'claude', id: '66666666-6666-4666-8666-666666666666',
          encoded_project: '-Users-test-flat', cwd: '/Users/test/flat', title: '扁平二',
          first_seen_ms: 1794000000000, last_seen_ms: 1794000000000, file_mtime_ms: 1794000000000,
          size_bytes: 1024, git_branch: 'main', version: '2.1.277', turns: 1, assistant_turns: 1,
          tool_calls: 0, subagents: 0, last_model: 'deepseek-v4.1-flash', counts_partial: false,
          gateway: null, gateway_error: null },
      ] } };`,
  });
  await send('Page.navigate', { url: `${origin}/index.html` });
  await sleep(300);
  await send('Runtime.evaluate', { expression: `window.switchTab('sessions')` });
  await sleep(1200);
  const flat = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(() => ({
      heads: document.querySelectorAll('#sessions-rows .sess-group-head').length,
      rows: document.querySelectorAll('#sessions-rows .sess-row').length,
      pagerHidden: document.querySelector('.sess-pager')?.hasAttribute('hidden') ?? null,
      pagingInfo: (document.querySelector('#sessions-paging-info') || {}).textContent || '',
    }))()`,
  });
  const fl = flat.result.value;
  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier: flatId });
  check('a response with no groups still renders its rows', fl.rows, 2);
  check('a flat response renders no group headers', fl.heads, 0);
  check('a flat response shows the pager again', fl.pagerHidden, false);
  check('a flat response keeps the offset-based paging text', fl.pagingInfo, '显示第 1–2 个 · 共 2 个');
}

/**
 * "Which session am I looking at" must survive both a reload and a tab switch,
 * and the URL must win over the remembered one.
 *
 * The mock serves one session, so "still on it" is exactly "that row is
 * `.active`". Every assertion below drives the real path — a click, a reload, a
 * tab switch — rather than calling the view's functions.
 */
export async function checkSessionsPersistence(ctx) {
  const { send, check, origin } = ctx;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  const activeId = `document.querySelector('#sessions-rows .sess-row.active')?.dataset.id || ''`;
  const hashOf = `location.hash`;

  // Start from a clean address and a clean memory: this scenario is about what
  // the page *writes*, so a leftover record from an earlier one would make it pass
  // for the wrong reason.
  await send('Page.navigate', { url: `${origin}/index.html` });
  await sleep(1200);
  await send('Runtime.evaluate', { expression: `(() => {
    try { localStorage.clear(); } catch {}
    window.switchTab('sessions');
  })()` });
  await sleep(1200);

  // ── 1. A click writes the address and does not create a history entry ──
  const clicked = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const before = history.length;
      const hashBefore = location.hash;
      document.querySelector('#sessions-rows .sess-row')?.click();
      await new Promise((r) => setTimeout(r, 400));
      let stored = null;
      try { stored = JSON.parse(localStorage.getItem('proxy-rs.lastSession.v1')); } catch {}
      return {
        hashBefore,
        hashAfter: location.hash,
        historyDelta: history.length - before,
        active: ${activeId},
        stored,
      };
    })()`,
  });
  const c = clicked.result.value;
  check('a click starts from a bare #sessions address', c.hashBefore, '#sessions');
  check('a click writes the three-segment address',
    c.hashAfter, `#sessions/claude/${FIXTURE_SESSION_ID}`);
  check('a click adds no history entry (replaceState)', c.historyDelta, 0);
  check('a click selects that session', c.active, FIXTURE_SESSION_ID);
  check('a click remembers the session', c.stored && c.stored.source === 'claude' && c.stored.id === FIXTURE_SESSION_ID, true);

  // ── 2. Reload: the address alone is enough ──
  await send('Page.reload');
  await sleep(1500);
  await send('Runtime.evaluate', { expression: `window.switchTab('sessions')` });
  await sleep(800);
  const reloaded = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `({ hash: ${hashOf}, active: ${activeId} })`,
  });
  check('a reload keeps the address', reloaded.result.value.hash, `#sessions/claude/${FIXTURE_SESSION_ID}`);
  check('a reload selects the same session', reloaded.result.value.active, FIXTURE_SESSION_ID);

  // ── 3. Tab switch and back: the hash is gone, storage restores ──
  const switched = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      window.switchTab('logs');
      await new Promise((r) => setTimeout(r, 200));
      const hashOnLogs = location.hash;
      window.switchTab('sessions');
      await new Promise((r) => setTimeout(r, 600));
      return {
        hashOnLogs,
        hashBack: location.hash,
        active: ${activeId},
        stored: (() => { try { return JSON.parse(localStorage.getItem('proxy-rs.lastSession.v1')); } catch { return null; } })(),
      };
    })()`,
  });
  const s = switched.result.value;
  check('switching to logs still writes the bare tab', s.hashOnLogs, '#logs');
  // Deliberately NOT "#sessions/claude/<id>": writing the id back into the URL on
  // tab entry would make the address say something the user did not navigate to,
  // and it is the one assertion elsewhere (`scenarios-pages.mjs:89`) that would go
  // red long after the change that caused it.
  check('coming back to sessions keeps the bare tab address', s.hashBack, '#sessions');
  check('coming back to sessions restores the session from storage', s.active, FIXTURE_SESSION_ID);

  // ── 4. The URL wins over storage ──
  // Point the address at the same single session, but with a memory that claims
  // something else: the address must be what is shown.
  const precedence = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      try {
        localStorage.setItem('proxy-rs.lastSession.v1', JSON.stringify({
          source: 'claude', id: '99999999-9999-4999-8999-999999999999', at: Date.now(),
        }));
      } catch {}
      location.hash = 'sessions/claude/${FIXTURE_SESSION_ID}';
      await new Promise((r) => setTimeout(r, 600));
      return {
        active: ${activeId},
        stored: (() => { try { return JSON.parse(localStorage.getItem('proxy-rs.lastSession.v1')); } catch { return null; } })(),
      };
    })()`,
  });
  const p = precedence.result.value;
  check('a deep link wins over the remembered session', p.active, FIXTURE_SESSION_ID);
  // Opening the address also refreshes the memory, so the next tab switch returns
  // to what the user is actually reading rather than to the stale value.
  check('opening a deep link overwrites the remembered session', p.stored?.id, FIXTURE_SESSION_ID);

  // ── 5. A remembered session that is gone: silent, and the key is cleared ──
  //
  // A fresh document, because the guard is "do not re-select what is already
  // selected": on this one the successful session is still open, so restore would
  // (correctly) do nothing. `sessions_detail` rejects for any other id — that is
  // what a deleted transcript does, and rejecting is deliberately different from
  // answering null, which takes the "no backend at all" branch.
  const { identifier: goneId } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      try {
        localStorage.setItem('proxy-rs.lastSession.v1', JSON.stringify({
          source: 'claude', id: 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee', at: Date.now(),
        }));
      } catch {}
      const installGoneStub = () => {
        const tauri = window.__TAURI__;
        if (!tauri || !tauri.core || tauri.core.__goneStub) return;
        const real = tauri.core.invoke;
        tauri.core.invoke = (cmd, args) => {
          if (cmd === 'sessions_detail' && args && args.id === 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee') {
            window.__CALLS__.push(cmd);
            return Promise.reject('会话文件不存在');
          }
          return real(cmd, args);
        };
        tauri.core.invoke.__goneStub = true;
      };
      // The bridge is installed by the harness script, which may run before or
      // after this one; a microtask is enough either way, and a timer covers the
      // rest.
      queueMicrotask(installGoneStub);
      setTimeout(installGoneStub, 0);
    `,
  });
  // The query string is what makes this a *document* navigation: navigating from
  // `index.html#sessions/claude/<id>` to `index.html#sessions` is a same-document
  // one, which would not run the injected script above at all (the harness serves
  // by path, so the query is ignored).
  await send('Page.navigate', { url: `${origin}/index.html?restore-gone=1#sessions` });
  await sleep(1800);
  const gone = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      window.switchTab('sessions');
      await new Promise((r) => setTimeout(r, 1200));
      let stored = 'unreadable';
      try { stored = localStorage.getItem('proxy-rs.lastSession.v1'); } catch {}
      return {
        toasts: [...document.querySelectorAll('.toast')].map((t) => t.textContent.trim()),
        errorToasts: [...document.querySelectorAll('.toast-error')].map((t) => t.textContent.trim()),
        paneMessage: (document.querySelector('#session-pane-empty .sess-empty') || {}).textContent || '',
        hash: location.hash,
        stored,
        asked: window.__CALLS__.includes('sessions_detail'),
      };
    })()`,
  });
  const g2 = gone.result.value;
  check('a remembered session is read on tab entry', g2.asked, true);
  check('a dead remembered session raises no toast at all', g2.toasts, []);
  check('a dead remembered session says so in the pane', g2.paneMessage.includes('打开会话失败'), true);
  check('a dead remembered session does not rewrite the address', g2.hash, '#sessions');
  check('a dead remembered session is forgotten', g2.stored, null);
  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier: goneId });

  // Leave the console on the plain address and with a clean memory: the scenarios
  // after this one assume a default document.
  await send('Runtime.evaluate', { expression: `(() => {
    try { localStorage.clear(); } catch {}
    location.hash = 'overview';
  })()` });
  await sleep(300);
}
