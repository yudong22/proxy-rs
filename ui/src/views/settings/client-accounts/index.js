/**
 * Wiring for the access-account panel.
 *
 * Split out of `client-accounts.js` so the public file is a pure re-export and
 * this is the only place that decides what gets bound at init.
 */

import { $ } from '../../../core/dom.js';
import { renderClientAccounts } from './list.js';
import { initCreateDialog, openCreateDialog } from './create-dialog.js';

/** Wire the panel's controls. Called once, from the settings view init. */
export function initClientAccounts() {
  initCreateDialog();
  $('#btn-client-account-add')?.addEventListener('click', openCreateDialog);
  // Paint immediately so the section is never an empty box on first view.
  void renderClientAccounts();
}
