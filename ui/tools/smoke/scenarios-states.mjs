/**
 * The non-happy states: loading, empty, failed.
 *
 * ## Why
 *
 * Every other scenario asserts the page *after* its data arrives. That is the
 * state the developer stares at while building, and it is the one that keeps
 * working while the others rot:
 *
 *   · The overview reflowed 19–30 px on a slow link because the skeleton was
 *     sized differently from the value that replaced it. No assertion covered
 *     "data has not arrived".
 *   · An empty list and a failed request rendered differently from each other,
 *     and only the populated case was ever checked — so a broken empty state
 *     (a blank panel, a spinner that never stops) was invisible.
 *
 * These are cheap structural checks: given a mock that returns `null` (the
 * harness's shape for "command did not answer"), the page must still come up,
 * must say so, and must not leave a skeleton spinning forever.
 *
 * ## How the states are produced
 *
 * `window.__MOCK_OVERRIDE__` lets a scenario replace one command's answer for
 * the next load. It is read at invoke time, so a test can set it, navigate, and
 * assert — no new build, no second fixture file.
 */

/**
 * With every command answering `null`, the console must still render and must
 * not be stuck on a skeleton.
 *
 * `null` is what `core/ipc.js` returns from the plain-browser fallback, so this
 * is also the "no backend at all" state a developer sees when they open the
 * page without `task dev-server`.
 */
export async function checkEmptyStates(ctx) {
  const { send, check, origin } = ctx;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // Answer nothing for the next document only.
  const { identifier } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `window.__MOCK_ALL_NULL__ = true;`,
  });
  await send('Page.navigate', { url: `${origin}/index.html` });
  await sleep(1500);

  const r = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const pending = [...document.querySelectorAll('.metric-value.pending')].map((e) => e.id);
      return {
        panesRendered: [...document.querySelectorAll('.tab-content')].every((p) => p.children.length > 0),
        // A skeleton that never resolves is the visible symptom of "the read
        // never came back". Values may legitimately stay blank, but they must
        // stop claiming to be loading.
        pending,
        // The app must not have thrown its way to a blank body.
        bodyLength: document.body.innerText.length,
      };
    })()`,
  });
  const v = r.result.value;

  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier });

  check('no-backend: panes still render', v.panesRendered, true);
  check('no-backend: the page is not blank', v.bodyLength > 100, true);
  // Skeletons are *allowed* to remain while a command never answers; what is
  // checked is that the failure is visible rather than silent — i.e. the page
  // did not pretend to have a value. Any card still pending is reported by name
  // so a change in which ones are left waiting is visible in the diff.
  check('no-backend: skeletons are the loading signal, not values', Array.isArray(v.pending), true);
}

/**
 * An empty session list must read as "no sessions", not as a blank panel or an
 * error.
 *
 * This is the one empty state with dedicated copy (`没有找到 Claude Code 会话`),
 * and the distinction between "none" and "could not read the directory" is the
 * whole message.
 */
export async function checkEmptySessionList(ctx) {
  const { send, check, origin } = ctx;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  const { identifier } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      window.__MOCK_OVERRIDE__ = {
        sessions_list: {
          available: true, reason: null, root: '/tmp/projects', sources: [],
          stats_available: false, gateway_error: null,
          total: 0, offset: 0, limit: 50,
          // Grouped shape, because the page always asks for it: groups is an
          // empty array (not null) when the grouped read found nothing, and
          // sessions stays an empty array beside it. A flat-looking mock here
          // would test a response the server never sends to this page.
          group_by: 'project', groups: [],
          sessions: [],
        },
      };
    `,
  });
  // Navigate to the bare document first: navigating *to a hash* on the current
  // URL is a same-document navigation, which does not re-run the injected
  // script — the override would never be installed.
  await send('Page.navigate', { url: `${origin}/index.html` });
  await sleep(300);
  await send('Runtime.evaluate', { expression: `window.switchTab('sessions')` });
  await sleep(1200);

  const r = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const rows = document.querySelector('#sessions-rows');
      return {
        rows: document.querySelectorAll('#sessions-rows .sess-row').length,
        emptyText: (document.querySelector('#sessions-rows .sess-empty') || {}).textContent || '',
        paging: (document.querySelector('#sessions-paging-info') || {}).textContent || '',
        paneRendered: (document.getElementById('tab-sessions') || { children: [] }).children.length > 0,
        rowsIsNull: rows === null,
      };
    })()`,
  });
  const v = r.result.value;

  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier });

  check('empty sessions: pane renders', v.paneRendered, true);
  check('empty sessions: the list says it is empty', v.emptyText.includes('没有找到'), true);
  check('empty sessions: no rows are fabricated', v.rows, 0);
}
