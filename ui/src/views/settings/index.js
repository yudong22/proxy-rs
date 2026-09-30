/**
 * Settings view: public surface.
 *
 * `renderSettings` builds the form once at boot and `initSettings` wires it.
 * `loadSettings` and `testUpstream` are reached from outside (the tab router
 * and the command palette), so they are re-exported here rather than off a
 * deep path.
 */

import { $ } from '../../core/dom.js';
import { renderSettings } from './form.js';
import { loadSettings, loadCodexConfig } from './load.js';
import {
  saveSettings,
  fetchModels,
  applyClaudeConfig,
  applyCodexConfig,
  testUpstream,
} from './save.js';
import { refreshSectionBadges } from './badges.js';
import { initCredentialPool } from './credential-pool.js';
import { initPoolTransfer } from './pool-transfer.js';
import { applyDshConfig, loadDshConfig } from './dsh.js';

export { renderSettings, loadSettings, testUpstream, refreshSectionBadges };

/** Wire every control in the settings form. Called once, from main.js. */
export function initSettings() {
  $('#btn-save-settings')?.addEventListener('click', saveSettings);
  $('#btn-fetch-models')?.addEventListener('click', fetchModels);
  $('#btn-apply-claude-config')?.addEventListener('click', applyClaudeConfig);
  $('#btn-apply-codex-config')?.addEventListener('click', applyCodexConfig);
  $('#btn-apply-dsh-config')?.addEventListener('click', applyDshConfig);
  $('#btn-test-upstream-settings')?.addEventListener('click', () => {
    testUpstream($('#settings-test-result'));
  });

  // Keep the "已修改" badges in step with the form as it is edited. `input`
  // covers typing and pasting; `change` covers selects and checkboxes.
  const form = $('#settings-form');
  form?.addEventListener('input', refreshSectionBadges);
  form?.addEventListener('change', refreshSectionBadges);

  initCredentialPool();
  // Import/export lives in its own module: it is a distinct workflow (a modal
  // either way), and folding it into credential-pool.js would grow that file
  // past the point where its list rendering is readable.
  initPoolTransfer();
  loadCodexConfig();
  loadDshConfig();
}
