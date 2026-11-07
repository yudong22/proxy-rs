/**
 * Identity pool wiring: the one place that says what the section consists of.
 *
 * Split out of `credential-pool.js`. Each step below is owned by the module named
 * beside it; this file only decides the order, and that order matters — the
 * repaint hooks must be installed before the first render, the automation
 * summary is painted before its dialog can be opened, and the growth poll
 * starts before the run button that feeds it.
 */

import { setOAuthPoolRefresh } from '../oauth-qr.js';
import { initRelogin, setReloginPoolRefresh } from '../relogin.js';
import { renderIdentityPool } from './pool-table.js';
import { setPoolRepaint } from './drag.js';
import {
  loadSchedule,
  initAutomationDialog,
  refreshAutomationSummary,
} from './automation.js';
import { initGrowthProgress, initGrowthRun } from './growth.js';
import { initQuickActions, initPoolControls } from './controls.js';

/** Wire the credential-pool controls and paint the initial list. */
export function initCredentialPool() {
  // The QR flow repaints the list through this hook (see oauth-qr.js), and the
  // re-bind dialog does the same through its own (see relogin.js). Both are
  // injected rather than imported back, so neither module has to depend on this
  // one — importing this file from them would close a cycle.
  setOAuthPoolRefresh(renderIdentityPool);
  setReloginPoolRefresh(renderIdentityPool);
  setPoolRepaint(renderIdentityPool);
  initRelogin();
  initAutomationDialog();
  // Paint the summary immediately: the page must state whether anything runs
  // unattended before the user opens the dialog.
  void refreshAutomationSummary();
  initQuickActions();
  initGrowthProgress();
  initGrowthRun();
  initPoolControls();
  renderIdentityPool();
  loadSchedule();
}
