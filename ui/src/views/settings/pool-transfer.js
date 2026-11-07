/**
 * Account-pool import / export (跨机迁移) — public surface.
 *
 * Two modals, both built on the shared overlay shell:
 *
 *   导出 — writes every login state and key to one file the user can carry to
 *          another machine. The file is a password book, so the risk is stated
 *          in the dialog rather than left to the README, and the optional
 *          passphrase is right there for anyone who wants it.
 *
 *   导入 — reads that file back. Preview and apply are separate steps against
 *          separate backend calls: `pool_import_preview` writes nothing, so the
 *          user sees exactly what will happen (and which default identity will
 *          be in force) before anything on disk changes.
 *
 * The backend never picks or reads a file itself — the webview reads the text
 * and sends it — so this module is the only place that touches the filesystem
 * on the export side, and it does so through the platform's own file input.
 *
 * The implementation lives in `pool-transfer/`; this file is the module
 * `settings/index.js` imports, so the split changed no import path:
 *
 *   shared.js         the modal-body helper, the File reader and the preview block
 *   export-dialog.js  导出账号池 (openExportDialog, fillExportSource, runExport)
 *   import-dialog.js  导入账号池 (openImportDialog, runPreview, runImport)
 *   index.js          toolbar wiring (initPoolTransfer)
 *
 * The exports below are exactly the set this module had before the split.
 */

export { openExportDialog } from './pool-transfer/export-dialog.js';
export { openImportDialog } from './pool-transfer/import-dialog.js';
export { initPoolTransfer } from './pool-transfer/index.js';
