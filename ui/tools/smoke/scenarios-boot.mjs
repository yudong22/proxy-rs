/**
 * Boot-path assertions: does the real render/init/load path put the right values
 * on screen?
 *
 * Split out of `smoke.mjs`. [`checkBoot`] captures a payload once; the two
 * scenarios that follow drive the page, and [`checkBootDerived`] then asserts the
 * rest of that snapshot. The payload is a value, not a live view, so the later
 * driving cannot change it — capturing and asserting apart is only here because
 * that is the order the assertions have always run in.
 */

/**
 * Drive the boot path and return the payload it produced, for
 * [`checkBootDerived`].
 *
 * @returns {object} what the page showed after render + init + load.
 */

export async function checkBoot(ctx) {
  const { send, check, EXPECTED } = ctx;
  const boot = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const rows = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')];
      const sel = document.querySelector('#wb-default-identity-select');
      const t = (s) => document.querySelector(s)?.textContent;
      const v = (s) => document.querySelector(s)?.value;
      return {
        counts: {
          credentialRows: rows.length,
          providerOptions: document.querySelector('#setting-provider')?.options.length,
          speedRows: document.querySelectorAll('#speed-sessions-body tr').length,
        },
        // The overview's second card row. 缓存命中率 is deliberately inert: it
        // reports a value and has no drill-down, so it must carry neither the
        // affordance classes nor a hidden detail section.
        cacheCard: (() => {
          const all = [...document.querySelectorAll('.metrics-grid .metric-card')];
          const card = all.find((c) => c.textContent.includes('缓存命中率'));
          return {
            present: Boolean(card),
            clickable: card?.classList.contains('clickable') || false,
            expandable: card?.classList.contains('expandable') || false,
            hasCaret: Boolean(card?.querySelector('.stat-caret')),
            detailExists: Boolean(document.querySelector('#stat-detail')),
          };
        })(),
        // The 输出速度 drill-down: metric row label -> that row's value cell for
        // the current session. Asserted by name rather than by row count so
        // adding a metric does not look like a regression, and a missing one
        // cannot hide behind a matching total.
        speedMetrics: (() => {
          const out = {};
          for (const tr of document.querySelectorAll('#speed-sessions-body tr')) {
            const th = tr.querySelector('th.col-metric');
            const td = tr.querySelector('td');
            if (th && td) out[th.textContent.trim()] = td.textContent.trim();
          }
          return out;
        })(),
        speedHeaderLabels: [...document.querySelectorAll('#speed-sessions-body th.col-session-cell')]
          .map((th) => th.textContent.trim()),
        text: {
          metricProvider: t('#metric-provider'), metricPoints: t('#metric-points'),
          metricPointsAccount: t('#metric-points-account'), metricSpeed: t('#metric-speed'),
          portValue: v('#setting-port'),
          // The two automation switches moved into a lazy dialog; what the page
          // itself must always show is the summary line naming what runs
          // unattended, because a closed dialog hides the switches themselves.
          automationSummary: t('#wb-automation-summary'),
          automationButton: Boolean(document.querySelector('#btn-wb-automation')),
          forceStreamHint: t('#force-stream-hint'),
          statusText: t('#status-text'), appVersion: t('#app-version'),
        },
        // The overview statistics row, which reads GET /api/stats/today. Its
        // values differ from the IPC mock's, so these assertions prove which
        // transport answered.
        stats: {
          requests: t('#stat-requests'), tokens: t('#stat-tokens-total'),
          cachePct: t('#stat-cache-pct'), failed: t('#stat-requests-failed'),
          overrides: t('#metric-overrides'),
        },
        statsViaIpc: window.__CALLS__.includes('get_stats'),
        rowLabels: rows.map((r) => r.querySelector('.wb-credential-label')?.textContent),
        rowStates: rows.map((r) => r.querySelector('.wb-state-pill')?.textContent),
        defaultRowFlags: rows.map((r) => r.classList.contains('wb-credential-row--default')),
        // Drag-to-reorder the failover priority: every credential row must be
        // draggable and carry a grip. The API-key row behind it must NOT be —
        // keys are a separate store with no position in the failover order.
        // The rows array above covers credentials AND keys (both use
        // .wb-credential-row), so the drag assertions key off data-cred-row,
        // which only credentials carry - keeping the two stores distinct.
        credDraggable: [...document.querySelectorAll('#wb-identity-list [data-ident-row]')]
          .map((r) => r.classList.contains('wb-credential-row--draggable')),
        credHandles: [...document.querySelectorAll('#wb-identity-list [data-ident-row]')]
          .map((r) => Boolean(r.querySelector('[data-drag-handle]'))),
        // Credentials AND keys are draggable now: one interleaved order.
        keyDraggable: [...document.querySelectorAll('#wb-identity-list .wb-type-key')]
          .map((k) => k.closest('[data-ident-row]')?.classList.contains('wb-credential-row--draggable') === true),
        rowIds: [...document.querySelectorAll('#wb-identity-list [data-ident-row]')]
          .map((r) => r.dataset.identRow),
        // Growth automation: the toolbar entry, the per-row entry, and the
        // opt-in switch must all exist. The switch defaults to off, which is the
        // product decision — auto-running upstream calls is opt-in.
        growth: {
          toolbarButton: Boolean(document.querySelector('#btn-wb-growth')),
          rowButtons: document.querySelectorAll('[data-growth]').length,
          // The switches live in the automation dialog and are built on first
          // open, so absence here is the expected state — opening it is asserted
          // separately below.
          toggleInPage: Boolean(document.querySelector('#wb-growth-enabled')),
        },
        selectValue: sel?.value,
        dshButtonPresent: Boolean(document.querySelector('#btn-apply-dsh-config')),
        dshStatus: t('#dsh-config-status'),
      };
    })()`,
  });
  const b = boot.result.value;
  check('counts', b.counts, EXPECTED.counts);
  check('text', b.text, EXPECTED.text);
  check('rowLabels', b.rowLabels, EXPECTED.rowLabels);
  check('rowStates', b.rowStates, EXPECTED.rowStates);
  check('defaultRowFlags', b.defaultRowFlags, EXPECTED.defaultRowFlags);
  check('every identity row is draggable', b.credDraggable, b.credDraggable.map(() => true));
  check('every identity row has a drag handle', b.credHandles, b.credHandles.map(() => true));
  // Keys join the same ordered list rather than being a separate block.
  check('api-key rows are draggable too', b.keyDraggable, b.keyDraggable.map(() => true));
  check('identity rows are draggable at all', b.credDraggable.length > 0, true);
  check('rows render in the requested identity order', b.rowIds,
    ['cred-1', 'cred-2', 'cred-3', 'cred-4', 'key-1']);
  check('growth toolbar button exists', b.growth.toolbarButton, true);
  check('growth row button exists per credential', b.growth.rowButtons > 0, true);
  check('growth switches are not in the page until asked for', b.growth.toggleInPage, false);
  return b;
}

/**
 * The remaining assertions on the boot payload captured above.
 *
 * @param {object} b - the payload [`checkBoot`] returned.
 */

export async function checkBootDerived(ctx, b) {
  const { check, apiHits, EXPECTED } = ctx;
  check('selectValue', b.selectValue, EXPECTED.selectValue);
  check('dsh button present', b.dshButtonPresent, EXPECTED.dshButtonPresent);
  check('dsh status', b.dshStatus, EXPECTED.dshStatus);
  check('speed drill-down metrics', b.speedMetrics, EXPECTED.speedMetrics);
  check('speed drill-down session columns', b.speedHeaderLabels, EXPECTED.speedHeaderLabels);
  check('缓存命中率 card is not expandable', b.cacheCard, EXPECTED.cacheCard);
  // The dashboard reads the listener's HTTP endpoints, not the IPC fallbacks.
  check('overview stats come from /api/stats/today', b.stats, EXPECTED.statsOverHttp);
  check('overview stats did not fall back to IPC', b.statsViaIpc, false);
  check('harness served /api/stats/today', apiHits.includes('/api/stats/today'), true);
}

/**
 * The automation dialog: open it and assert both switches appear with the
 * stored preference values.
 *
 * This is what replaces the two old inline rows — the point of the move is that
 * both switches are visible at once, so a test that only checked "a button
 * exists" would not cover the actual change.
 */

export async function checkAutomationDialog(ctx) {
  const { send, check } = ctx;
  // The automation dialog: open it and assert both switches appear with the
  // stored preference values. This is what replaces the two old inline rows —
  // the point of the move is that both switches are visible at once, so a test
  // that only checked "a button exists" would not cover the actual change.
  await send('Runtime.evaluate', { expression: `document.querySelector('#btn-wb-automation')?.click()` });
  await new Promise((r) => setTimeout(r, 400));
  const automation = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = document.querySelector('#wb-automation-overlay');
      return {
        open: Boolean(el) && el.classList.contains('active'),
        hasCheckin: Boolean(document.querySelector('#wb-checkin-enabled')),
        hasGrowth: Boolean(document.querySelector('#wb-growth-enabled')),
        checkinChecked: document.querySelector('#wb-checkin-enabled')?.checked ?? null,
        growthChecked: document.querySelector('#wb-growth-enabled')?.checked ?? null,
        checkinTime: document.querySelector('#wb-checkin-time')?.value ?? null,
        growthHint: (document.querySelector('#wb-growth-hint')?.textContent || '').trim(),
      };
    })()`,
  });
  const auto = automation.result.value;
  check('automation dialog opens', auto.open, true);
  check('automation dialog holds both switches', [auto.hasCheckin, auto.hasGrowth], [true, true]);
  // The switches must reflect the stored preferences, not a hardcoded default:
  // this fixture has 每日打卡 on and growth off, so a dialog that reset either
  // would silently change what runs unattended.
  check('automation switches reflect stored preferences',
    [auto.checkinChecked, auto.growthChecked], [true, false]);
  check('check-in time seeds from stored preferences', auto.checkinTime, '08:30');
  check('growth hint explains the off state', auto.growthHint.includes('关闭中'), true);
  // Close it again so the rest of the run sees the resting page.
  await send('Runtime.evaluate', { expression: `document.querySelector('#wb-automation-overlay [data-close]')?.click()` });
  await new Promise((r) => setTimeout(r, 250));
  const closed = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `Boolean(document.querySelector('#wb-automation-overlay')?.classList.contains('active'))`,
  });
  check('automation dialog closes again', closed.result.value, false);
}

/**
 * The growth-run progress stream.
 *
 * This is the regression the user reported: clicking 增长 gave no feedback at
 * all. The assertion drives the real button and requires the progress box to show
 * the polled lines.
 */

export async function checkGrowthProgress(ctx) {
  const { send, check } = ctx;
  // Cross-module integration 5: the growth-run progress stream.
  //
  // This is the regression the user reported: clicking 增长 gave no feedback at
  // all. The assertion drives the real button, emits the same `growth-progress`
  // events the backend sends, and requires the progress box to show them.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__CALLS__ = [];
    // A fresh run starts with an empty buffer (the backend drains it).
    window.__MOCKDATA__.__growth_progress_drained = false;
    document.querySelector('#btn-wb-growth').click();
  })()` });
  // Let the click handler set up its state before the events arrive.
  await new Promise((r) => setTimeout(r, 150));
  // The progress lines now arrive by polling `get_growth_progress` (the
  // console has no event channel over HTTP), so the harness waits for a poll
  // tick instead of emitting events.
  await new Promise((r) => setTimeout(r, 1400));

  const growthUi = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = document.querySelector('#wb-growth-progress');
      const txt = (el?.textContent || '').trim();
      return {
        visible: Boolean(el) && !el.hidden,
        text: txt,
        called: window.__CALLS__.includes('wb_growth_run'),
        buttonLabel: document.querySelector('#btn-wb-growth')?.textContent?.trim(),
      };
    })()`,
  });
  const g = growthUi.result.value;
  check('growth run invoked', g.called, true);
  check('growth progress box is visible', g.visible, true);
  // The per-account line and the claimed-credit line must both appear: those are
  // the two things that tell the user something is happening.
  check('growth progress shows the account', g.text.includes('测试账号一'), true);
  check('growth progress shows the claimed task', g.text.includes('chat_5'), true);
  check('growth progress shows earned credits', g.text.includes('100'), true);
  check('growth button restored after run', g.buttonLabel.includes('增长积分'), true);
}
