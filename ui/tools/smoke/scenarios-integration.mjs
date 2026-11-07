/**
 * The cross-module integration points that a file split is most likely to break.
 *
 * Split out of `smoke.mjs`. Each scenario drives a real toolbar button and
 * requires the collaborating module to respond: the palette's dynamic import, the
 * OAuth callback into the pool, the pool transfer dialogs, and login-state
 * recovery.
 */

/**
 * The palette's dynamic import of the settings view, and the OAuth success that
 * must repaint the pool through the callback the pool injected into the OAuth
 * module.
 *
 * @returns {object} the integration payload, whose `called` list the report counts.
 */

export async function checkPaletteAndOAuth(ctx) {
  const { send, check, failures } = ctx;
  // Cross-module integration 1: palette's dynamic import of the settings view.
  await send('Runtime.evaluate', { expression: 'window.__CALLS__.length = 0;' });
  await send('Runtime.evaluate', {
    expression: `import('./src/views/settings/index.js').then(async (m) => {
      window.__T_TYPE__ = typeof m.testUpstream;
      await m.testUpstream(document.querySelector('#settings-test-result'));
      window.__T_TEXT__ = document.querySelector('#settings-test-result')?.textContent;
      window.__T_CLASS__ = document.querySelector('#settings-test-result')?.className;
    });`,
  });
  await new Promise((r) => setTimeout(r, 800));

  // Cross-module integration 2: OAuth success must repaint the pool via the
  // callback the pool injected into the OAuth module.
  await send('Runtime.evaluate', { expression: `document.querySelector('#btn-wb-qrcode').click();` });
  await new Promise((r) => setTimeout(r, 3000));

  const integ = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({
      testUpstreamType: window.__T_TYPE__,
      testUpstreamText: window.__T_TEXT__,
      testUpstreamClass: window.__T_CLASS__,
      qrModalActive: document.querySelector('#request-modal-overlay')?.classList.contains('active'),
      qrSvgInjected: Boolean(document.querySelector('#wb-qr-target svg')),
      pollStatus: document.querySelector('#wb-qr-poll-status')?.textContent?.trim(),
      called: [...new Set(window.__CALLS__)],
    }))()`,
  });
  const i = integ.result.value;
  check('palette dynamic import exposes testUpstream', i.testUpstreamType, 'function');
  check('testUpstream result', i.testUpstreamText, '✓ 连通性测试通过 (上游流式响应正常)');
  check('testUpstream class', i.testUpstreamClass, 'test-result ok');
  check('qr svg injected', i.qrSvgInjected, true);
  check('qr poll status', i.pollStatus, '✅ 扫码成功！账号已自动加入账号池');
  for (const cmd of ['wb_oauth_start', 'wb_oauth_poll', 'wb_credentials_list', 'api_keys_list']) {
    if (!i.called.includes(cmd)) failures.push(`OAuth flow did not call ${cmd} (pool repaint likely broken)`);
  }
  return i;
}

/**
 * The account-pool transfer dialogs (2.0.0).
 *
 * Both are modal-only, so the toolbar buttons are the entry point, and the
 * import path must go preview -> (nothing written) -> apply -> pool repaint.
 *
 * The export leg starts by cancelling: the save panel is now a real OS sheet, so
 * "dismissed it" is an ordinary outcome the UI must report as such instead of as
 * a failure.
 */

export async function checkPoolTransfer(ctx) {
  const { send, check, failures, EXPECTED } = ctx;

  // Cross-module integration 3: the account-pool transfer dialogs (2.0.0).
  // Both are modal-only, so the toolbar buttons are the entry point, and the
  // import path must go preview -> (nothing written) -> apply -> pool repaint.
  //
  // The export leg starts by cancelling: the save panel is now a real OS sheet,
  // so "dismissed it" is an ordinary outcome the UI must report as such instead
  // of as a failure.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__CALLS__ = [];
    window.__MOCKDATA__.pool_export = { ok: false, cancelled: true };
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-export').click();
    document.querySelector('#btn-wb-export-run').click();
  })()` });
  await new Promise((r) => setTimeout(r, 300));

  const cancelStep = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({
      text: (document.querySelector('#wb-export-result')?.textContent || '')
        .replace(/\\s+/g, ' ').trim(),
      isError: document.querySelector('#wb-export-result')?.classList.contains('error'),
      retryable: !document.querySelector('#btn-wb-export-run')?.disabled,
    }))()`,
  });
  check('cancelled export is not an error', cancelStep.result.value, EXPECTED.exportCancelled);

  await send('Runtime.evaluate', { expression: `(() => {
    window.__MOCKDATA__.pool_export = {
      ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-20260101.json', encrypted: true,
      counts: { credentials: 2, api_keys: 1 }, empty: false, warnings: [],
    };
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-export').click();
    document.querySelector('#wb-export-encrypt').checked = true;
    document.querySelector('#wb-export-encrypt')
      .dispatchEvent(new Event('change', { bubbles: true }));
    document.querySelector('#wb-export-passphrase').value = 'hunter2hunter2';
    document.querySelector('#wb-export-passphrase2').value = 'hunter2hunter2';
    document.querySelector('#btn-wb-export-run').click();
  })()` });
  await new Promise((r) => setTimeout(r, 500));

  // Read the export result before the next dialog replaces the modal body.
  const exportStep = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({
      text: (document.querySelector('#wb-export-result')?.textContent || '')
        .replace(/\\s+/g, ' ').trim(),
      withPassphrase: Boolean(window.__INVOKE_ARGS__?.pool_export?.body?.passphrase),
      sourceShown: (document.querySelector('#wb-export-source')?.textContent || '').trim(),
      countsShown: (document.querySelector('#wb-export-counts')?.textContent || '').trim(),
    }))()`,
  });

  // An empty export must not look like a success: the backend reports
  // `empty: true` and the UI has to say so in the error tone, naming the reason.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__MOCKDATA__.pool_export = {
      ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-empty.json', encrypted: false,
      counts: { credentials: 0, api_keys: 0 }, empty: true,
      warnings: ['本机身份池为空，导出的文件不包含任何账号或密钥'],
    };
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-export').click();
    document.querySelector('#btn-wb-export-run').click();
  })()` });
  await new Promise((r) => setTimeout(r, 400));

  const emptyStep = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = document.querySelector('#wb-export-result');
      return {
        // The message spans inline <b> tags, so collapse whitespace around the
        // element boundaries before comparing.
        text: (el?.textContent || '').replace(/\\s+/g, ' ').trim(),
        isError: el?.classList.contains('error'),
      };
    })()`,
  });
  const empty = emptyStep.result.value;
  check('empty export is an error tone', empty.isError, EXPECTED.exportEmpty.isError);
  for (const needle of EXPECTED.exportEmpty.contains) {
    if (!empty.text.includes(needle)) {
      failures.push(`empty export message is missing ${JSON.stringify(needle)}\n      actual: ${empty.text}`);
    }
  }

  // Restore the healthy mock before the import leg runs.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__MOCKDATA__.pool_export = {
      ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-20260101.json', encrypted: true,
      counts: { credentials: 2, api_keys: 1 }, empty: false, warnings: [],
    };
  })()` });

  await send('Runtime.evaluate', { expression: `(() => {
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-import').click();
    document.querySelector('#wb-import-text').value = '{"format":"proxy-rs-pool"}';
    document.querySelector('#btn-wb-import-preview').click();
  })()` });
  await new Promise((r) => setTimeout(r, 400));

  // Only a successful preview may enable 确认导入; the apply call is what must
  // repaint the pool, so the assertion covers both halves.
  const transfer = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const t = (s) => (document.querySelector(s)?.textContent || '').replace(/\\s+/g, ' ').trim();
      const previewEnabled = !document.querySelector('#btn-wb-import-run')?.disabled;
      const preview = {
        credentialsAdded: t('.wb-transfer-cards .wb-stat-card:nth-child(2) .wb-stat-val'),
        credentialsUpdated: t('.wb-transfer-cards .wb-stat-card:nth-child(3) .wb-stat-val'),
        warningText: t('.wb-transfer-warnings li'),
        identity: t('.wb-transfer-identity .mono'),
      };
      const before = window.__CALLS__.length;
      document.querySelector('#btn-wb-import-run').click();
      await new Promise((r) => setTimeout(r, 600));
      return {
        previewEnabled,
        preview,
        importCalled: window.__CALLS__.includes('pool_import'),
        poolRepainted: window.__CALLS__.slice(before).includes('wb_credentials_list'),
        modalClosed: !document.querySelector('#request-modal-overlay')?.classList.contains('active'),
      };
    })()`,
  });
  const ex = exportStep.result.value;
  const tr = transfer.result.value;
  check('export result text', ex.text, EXPECTED.exportResult);
  check('export sends the passphrase', ex.withPassphrase, true);
  // The dialog names the data directory it will read — the fact that explains an
  // unexpectedly empty export.
  check('export dialog shows its data directory', ex.sourceShown, EXPECTED.exportSourceShown);
  check('export dialog shows what will be exported', ex.countsShown, EXPECTED.exportCountsShown);
  check('preview enables 确认导入', tr.previewEnabled, true);
  check('import preview counts', tr.preview, EXPECTED.importPreview);
  check('import called', tr.importCalled, true);
  check('import repaints the pool', tr.poolRepainted, true);
  check('import closes the dialog', tr.modalClosed, true);
}

/**
 * Login-state recovery (relogin).
 *
 * There is deliberately no "refresh all logins" button: renewal is automatic (the
 * scheduler scans, and a 401 renews reactively), so the only manual step left is
 * re-binding an account that cannot heal itself. What the badge must therefore
 * carry is the verdict — including the case a static check cannot see, namely a
 * credential that *has* a refresh token but was rejected.
 */

export async function checkLoginRecovery(ctx) {
  const { send, check, failures, EXPECTED } = ctx;
  // Cross-module integration 4: login-state recovery (relogin).
  //
  // There is deliberately no "refresh all logins" button: renewal is automatic
  // (the scheduler scans, and a 401 renews reactively), so the only manual step
  // left is re-binding an account that cannot heal itself. What the badge must
  // therefore carry is the verdict — including the case a static check cannot
  // see, namely a credential that *has* a refresh token but was rejected.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__CALLS__ = [];
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
  })()` });

  const recovery = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const t = (s) => (document.querySelector(s)?.textContent || '').replace(/\\s+/g, ' ').trim();

      // The per-row 刷新积分 button must refresh ONLY its own row. It used to
      // ignore its data-points id and call the pool-wide command, so this
      // asserts the id actually reaches the backend.
      const pointsRow = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')]
        .find((r) => r.textContent.includes('测试账号二'));
      pointsRow.querySelector('[data-points]').click();
      await new Promise((r) => setTimeout(r, 300));
      const rowPoints = {
        called: window.__CALLS__.includes('wb_refresh_points_single'),
        id: window.__INVOKE_ARGS__?.wb_refresh_points_single?.id,
        calledPoolWide: window.__CALLS__.includes('wb_refresh_points'),
        status: t('#wb-status'),
      };

      const row = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')]
        .find((r) => r.textContent.includes('需重登账号'));
      const badge = row?.querySelector('.wb-relogin-pill')?.textContent?.trim() || '';

      // A static "no refresh token" check would miss this row entirely; the
      // durable verdict from a rejected refresh is what marks it.
      const rejectedRow = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')]
        .find((r) => r.textContent.includes('令牌被拒账号'));
      const rejected = {
        marked: Boolean(rejectedRow?.querySelector('.wb-relogin-pill')),
        // The reason belongs on the row, not only in the live log.
        tooltip: rejectedRow?.querySelector('.wb-relogin-pill')?.getAttribute('title') || '',
      };

      const before = window.__CALLS__.length;
      // The button on *that* row, not the first one in the list.
      row.querySelector('[data-relogin]').click();
      const dialogOpen = document.querySelector('#request-modal-overlay')?.classList.contains('active');
      const targetShown = t('#modal-req-body .mono');
      document.querySelector('#wb-relogin-text').value = '{"accessToken":"fresh-token"}';
      document.querySelector('#btn-wb-relogin-run').click();
      await new Promise((r) => setTimeout(r, 400));
      return {
        reloginBadge: badge,
        dialogOpen,
        targetShown,
        rowPoints,
        rejected,
        reloginCalled: window.__CALLS__.includes('wb_relogin'),
        reloginBody: window.__INVOKE_ARGS__?.wb_relogin?.body?.id,
        poolRepainted: window.__CALLS__.slice(before).includes('wb_credentials_list'),
        dialogClosed: !document.querySelector('#request-modal-overlay')?.classList.contains('active'),
      };
    })()`,
  });
  const rc = recovery.result.value;
  check('needs-relogin account is marked', rc.reloginBadge, '需重新绑定');
  // The capability the removed button used to provide: an account whose refresh
  // was *rejected* still gets flagged, with the reason on the row.
  check('rejected-refresh account is marked', rc.rejected.marked, true);
  for (const needle of EXPECTED.rejectedTooltipContains) {
    if (!rc.rejected.tooltip.includes(needle)) {
      failures.push(`rejected-refresh tooltip is missing ${JSON.stringify(needle)}\n      actual: ${rc.rejected.tooltip}`);
    }
  }
  check('relogin dialog opens', rc.dialogOpen, true);
  check('relogin dialog names its target', rc.targetShown, 'cred-3');
  check('relogin called', rc.reloginCalled, true);
  check('relogin targets the row it was opened from', rc.reloginBody, 'cred-3');
  check('relogin repaints the pool', rc.poolRepainted, true);
  check('relogin closes the dialog', rc.dialogClosed, true);
  // The per-row points button must target its own row and must NOT fall back to
  // the pool-wide command.
  check('row 刷新积分 calls the single-identity command', rc.rowPoints.called, true);
  check('row 刷新积分 passes its own id', rc.rowPoints.id, 'cred-2');
  check('row 刷新积分 does not refresh the whole pool', rc.rowPoints.calledPoolWide, false);
}
