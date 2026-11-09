/**
 * The other two view panes: the request-log table, the 会话 transcript, and tab
 * routing.
 *
 * Split out of `smoke.mjs`. Each of these proves the same class of thing as the
 * boot assertions — that a pane is driven by the transport it is supposed to use
 * — so they read together.
 */

/**
 * The request-log table is HTTP-backed: switching to the logs tab must fetch
 * `/api/request-logs` (with the pager's limit/offset) and render the row the
 * harness serves — not silently use the empty IPC mock.
 */

export async function checkRequestLogs(ctx) {
  const { send, check, apiHits } = ctx;
  // The request-log table is the other HTTP-backed panel. Switching to the logs
  // tab must fetch `/api/request-logs` (with the pager's limit/offset) and render
  // the row the harness serves — not silently use the empty IPC mock.
  await send('Runtime.evaluate', { expression: `window.switchTab('logs');` });
  await new Promise((r) => setTimeout(r, 800));

  const logsPan = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const rows = [...document.querySelectorAll('#request-logs-tbody tr')];
      return {
        rowCount: rows.length,
        firstModel: rows[0]?.querySelector('.cell-model')?.textContent || '',
        count: document.querySelector('#table-log-count')?.textContent,
        logsViaIpc: window.__CALLS__.includes('get_request_logs'),
      };
    })()`,
  });
  const lp = logsPan.result.value;
  check('request log row came from /api/request-logs', lp.firstModel, 'deepseek-chat');
  check('request log page size came from /api/request-logs', lp.rowCount, 1);
  check('request log total came from /api/request-logs', lp.count, '1 条记录');
  check('harness served /api/request-logs', apiHits.includes('/api/request-logs'), true);
  check('request log table did not fall back to IPC', lp.logsViaIpc, false);
}

/**
 * The one session in the mock: `.sess-row-open` must name exactly this id.
 *
 * Kept beside the assertions rather than imported from `fixtures.mjs` because
 * the harness passes its own copies through `ctx`, and an inline literal makes
 * the expected value visible in the failure message.
 */
const FIXTURE_SESSION_ID = 'b9c3800a-311e-4c89-9a38-c533d8969b5d';

/**
 * 会话 view: the list renders, a click opens the transcript, and the
 * transcript's markup is shown as text — not executed.
 */

export async function checkSessions(ctx) {
  const { send, check } = ctx;
  // 会话 view: the list renders, a click opens the transcript, and the
  // transcript's markup is shown as text — not executed.
  await send('Runtime.evaluate', { expression: `window.switchTab('sessions');` });
  await new Promise((r) => setTimeout(r, 500));
  const sess = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const rows = [...document.querySelectorAll('#sessions-rows .sess-row')];
      const dsh = document.querySelector('#sessions-sources [data-kind="dsh_local"]');

      // The per-row shortcut is a *sibling* of the row (a button cannot nest a
      // button), so this also pins the wrapper: one action per row, and it sits
      // in the same wrapper as the row it acts on.
      const openBtns = [...document.querySelectorAll('#sessions-rows .sess-row-wrap .sess-row-open')];
      const openId = openBtns[0]?.dataset.openId || '';

      // Click the shortcut *before* the row and record what it did. The order
      // matters: the detail read is only expected to be caused by the row click
      // below, so if the shortcut also triggers it, the calls recorded right
      // after the shortcut catch the leak.
      const callsBeforeOpen = window.__CALLS__.length;
      openBtns[0]?.click();
      await new Promise((r) => setTimeout(r, 150));
      const callsAfterOpen = window.__CALLS__.slice(callsBeforeOpen);
      const openArgs = window.__INVOKE_ARGS__['open_session_in_browser'] || null;

      rows[0]?.click();
      await new Promise((r) => setTimeout(r, 300));
      return {
        rowCount: rows.length,
        title: rows[0]?.querySelector('.sess-row-title')?.textContent,
        gatewayBadge: !!rows[0]?.querySelector('.sess-badge'),
        dshDisabled: !!dsh && dsh.disabled,
        detailCalled: window.__CALLS__.includes('claude_sessions_detail'),
        timelineEntries: document.querySelectorAll('#session-timeline .sess-turn').length,
        toolName: document.querySelector('#session-timeline .sess-tool-name')?.textContent || '',
        innerScrollers: [...document.querySelectorAll('#session-timeline .sess-pre')].filter((p) => p.scrollHeight > p.clientHeight + 2).length,
        bubbleText: document.querySelector('#session-timeline .sess-prompt')?.textContent || '',
        xssRan: window.__XSS__ === 1,
        gatewayShown: !!document.querySelector('.sess-gateway .sess-gateway-title'),
        // ── "open in browser" wiring ──
        openButtons: openBtns.length,
        openId,
        openCalled: callsAfterOpen.includes('open_session_in_browser'),
        openArgId: openArgs?.id ?? null,
        // Event isolation: the shortcut must not also select the row, which
        // would fire a detail read the user never asked for.
        openSelectedRow: callsAfterOpen.includes('claude_sessions_detail'),
        // Consistency: the detail topbar carries the same affordance.
        headerOpenId: document.querySelector('#session-detail-head .sess-row-open')?.dataset.openId || '',
      };
    })()`,
  });
  const sv = sess.result.value;
  check('sessions list renders one row', sv.rowCount, 1);
  check('sessions row title', sv.title, '会话页测试');
  check('sessions row shows the gateway badge', sv.gatewayBadge, true);
  check('DSH source is disabled', sv.dshDisabled, true);
  check('clicking a row loads the transcript', sv.detailCalled, true);
  check('timeline entries rendered', sv.timelineEntries, 2);
  check('tool call renders in Claude Code style', sv.toolName, 'Bash');
  check('tool output never scrolls (collapse instead)', sv.innerScrollers, 0);
  check('transcript text renders as text', sv.bubbleText.includes('<script>window.__XSS__=1</script>'), true);
  check('transcript markup is not executed', sv.xssRan, false);
  check('gateway totals shown in the detail', sv.gatewayShown, true);
  // The row's own action, and the two properties that make it safe: it asks by
  // id only (the backend owns the URL), and it does not double as a row click.
  check('every session row carries an open-in-browser button', sv.openButtons, 1);
  check('the open button targets that row id', sv.openId, FIXTURE_SESSION_ID);
  check('clicking open calls open_session_in_browser', sv.openCalled, true);
  check('open is asked by session id, not by url', sv.openArgId, FIXTURE_SESSION_ID);
  check('clicking open does not also select the row', sv.openSelectedRow, false);
  check('the detail header offers the same action', sv.headerOpenId, FIXTURE_SESSION_ID);

  // The same view, in the *server* console. There "open in browser" is
  // meaningless (the page is already in one), so the row must offer a copy-link
  // action instead — and it must copy a link that actually leads back to this
  // console's deep link. Asserted here rather than in its own scenario because
  // it is the same markup being rendered through the other branch of the same
  // function, and a rename in one branch should fail beside the other.
  await send('Runtime.evaluate', {
    // Stub the clipboard: headless Chrome on localhost is a secure context, so
    // the real API exists but writing may hang on a permission check. The stub
    // records what the UI *asked* to copy, which is the assertion.
    expression: `(() => {
      window.__COPIED__ = null;
      try {
        Object.defineProperty(navigator, 'clipboard', {
          configurable: true,
          value: { writeText: async (t) => { window.__COPIED__ = t; } },
        });
      } catch { window.__COPIED__ = 'clipboard-define-failed'; }
      window.__PROXY_RS_SERVER__ = true;
      document.querySelector('#btn-sessions-refresh').click();
    })()`,
  });
  await new Promise((r) => setTimeout(r, 400));
  const serverFlavor = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const btn = document.querySelector('#sessions-rows .sess-row-open');
      const out = {
        copyId: btn?.dataset.copyId || '',
        openId: btn?.dataset.openId || '',
        label: btn?.getAttribute('aria-label') || '',
      };
      // Count only the calls this click makes: __CALLS__ is cumulative for the
      // whole run, and the desktop branch above already opened a browser.
      const before = window.__CALLS__.length;
      btn?.click();
      await new Promise((r) => setTimeout(r, 150));
      out.copied = window.__COPIED__ || '';
      out.calledOpen = window.__CALLS__.slice(before).includes('open_session_in_browser');
      // The detail header is repainted by selection, not by the list refresh, so
      // re-open the row to render the header under the server flavour.
      document.querySelector('#sessions-rows .sess-row')?.click();
      await new Promise((r) => setTimeout(r, 300));
      out.headerCopyId = document.querySelector('#session-detail-head .sess-row-open')?.dataset.copyId || '';
      return out;
    })()`,
  });
  const sf = serverFlavor.result.value;
  check('server console: the row action copies instead of opening', sf.copyId, FIXTURE_SESSION_ID);
  check('server console: the row action is not an open button', sf.openId, '');
  check('server console: the action says it copies', sf.label.includes('复制'), true);
  check(
    'server console: the copied link is this console’s deep link',
    sf.copied.endsWith(`/admin/#sessions/${FIXTURE_SESSION_ID}`) &&
      sf.copied.startsWith('http://127.0.0.1:'),
    true,
  );
  // The copy path must not reach the desktop command at all: on a server it can
  // only answer with an error, and a spurious error toast is a bug of its own.
  check('server console: copying never asks to open a browser', sf.calledOpen, false);
  check('server console: the detail header copies too', sf.headerCopyId, FIXTURE_SESSION_ID);

  // Restore the desktop flavour so the scenarios after this one see the console
  // they expect (and so the clipboard stub is not left in place).
  await send('Runtime.evaluate', {
    expression: `(() => {
      window.__PROXY_RS_SERVER__ = undefined;
      delete window.__COPIED__;
      document.querySelector('#btn-sessions-refresh').click();
    })()`,
  });
  await new Promise((r) => setTimeout(r, 300));
}

/**
 * Tab routing is hash-driven: a switch writes the hash, and the hash alone (as
 * a deep link would) repaints the pane.
 */

export async function checkHashRouting(ctx) {
  const { send, check } = ctx;
  // Tab routing is hash-driven: a switch writes the hash, and the hash alone
  // (as a deep link would) repaints the pane.
  //
  // The last three cases are the `#sessions/<uuid>` shape the console hands out
  // as a shareable address: the tab resolves from the first segment, the id must
  // survive the paint (rewriting it to a bare `#sessions` would be the router
  // silently dropping the link), and a malformed id must resolve the tab while
  // selecting nothing at all.
  const hashInfo = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      window.switchTab('logs');
      await new Promise((r) => setTimeout(r, 100));
      const afterClick = location.hash;
      window.location.hash = 'settings';
      await new Promise((r) => setTimeout(r, 150));
      const afterHash = document.querySelector('.tab.active')?.dataset.tab;
      location.hash = 'no-such-tab';
      await new Promise((r) => setTimeout(r, 150));
      const unknown = document.querySelector('.tab.active')?.dataset.tab;

      location.hash = 'sessions/${FIXTURE_SESSION_ID}';
      await new Promise((r) => setTimeout(r, 500));
      const deepTab = document.querySelector('.tab.active')?.dataset.tab;
      const deepHashKept = location.hash.includes('${FIXTURE_SESSION_ID}');
      const deepSelected = document.querySelector('#sessions-rows .sess-row.active')?.dataset.id || '';

      location.hash = 'sessions/not-a-uuid';
      await new Promise((r) => setTimeout(r, 300));
      const badIdTab = document.querySelector('.tab.active')?.dataset.tab;
      return { afterClick, afterHash, unknown, deepTab, deepHashKept, deepSelected, badIdTab };
    })()`,
  });
  const hi = hashInfo.result.value;
  check('switching a tab writes the hash', hi.afterClick, '#logs');
  check('the hash alone switches the pane', hi.afterHash, 'settings');
  check('an unknown hash falls back to overview', hi.unknown, 'overview');
  check('a #sessions/<uuid> link opens the sessions tab', hi.deepTab, 'sessions');
  check('a deep link keeps its session id in the hash', hi.deepHashKept, true);
  check('a deep link selects that session', hi.deepSelected, FIXTURE_SESSION_ID);
  check('a malformed session id still resolves the tab', hi.badIdTab, 'sessions');
}

/** Return to the overview so the later scenarios find their elements. */
export async function returnToOverview(ctx) {
  const { send } = ctx;
  // Back to the overview so the remaining integration checks find their elements.
  await send('Runtime.evaluate', { expression: `window.switchTab('overview');` });
  await new Promise((r) => setTimeout(r, 300));
}
