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
}

/**
 * Tab routing is hash-driven: a switch writes the hash, and the hash alone (as
 * a deep link would) repaints the pane.
 */

export async function checkHashRouting(ctx) {
  const { send, check } = ctx;
  // Tab routing is hash-driven: a switch writes the hash, and the hash alone
  // (as a deep link would) repaints the pane.
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
      return { afterClick, afterHash, unknown };
    })()`,
  });
  const hi = hashInfo.result.value;
  check('switching a tab writes the hash', hi.afterClick, '#logs');
  check('the hash alone switches the pane', hi.afterHash, 'settings');
  check('an unknown hash falls back to overview', hi.unknown, 'overview');
}

/** Return to the overview so the later scenarios find their elements. */
export async function returnToOverview(ctx) {
  const { send } = ctx;
  // Back to the overview so the remaining integration checks find their elements.
  await send('Runtime.evaluate', { expression: `window.switchTab('overview');` });
  await new Promise((r) => setTimeout(r, 300));
}
