/**
 * Wiring for the import/export toolbar entries.
 *
 * Split out of `pool-transfer.js` so the public file is a pure re-export and this
 * is the only place that knows both dialogs exist.
 */

import { $ } from '../../../core/dom.js';
import { openExportDialog } from './export-dialog.js';
import { openImportDialog } from './import-dialog.js';

/** Wire the toolbar buttons. Called once, from settings/index.js. */
export function initPoolTransfer() {
  $('#btn-wb-export')?.addEventListener('click', openExportDialog);
  $('#btn-wb-import')?.addEventListener('click', openImportDialog);
}
