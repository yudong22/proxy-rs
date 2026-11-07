/**
 * Identity pool (账号 / 密钥) — public surface.
 *
 * This file is the module every other view imports (`showWbStatus`,
 * `renderIdentityPool`, `loadSchedule`, `initCredentialPool`), so splitting the
 * implementation into `credential-pool/` did not change a single import path
 * elsewhere.
 *
 * Where the work lives:
 *
 *   pool-table.js   the list, the default-identity selector and the row actions
 *   drag.js         pointer-event drag-to-reorder of the failover priority
 *   growth.js       the polled growth-task progress and the run button
 *   automation.js   the 每日打卡 / 成长任务 automation dialog and its summary
 *   controls.js     toolbar entries, the key form, default identity, sticky reset
 *   status.js       the `#wb-status` line every one of the above writes to
 *   index.js        wiring order (`initCredentialPool`)
 *
 * The exports below are exactly the set this module had before the split.
 */

export { renderIdentityPool } from './credential-pool/pool-table.js';
export { showWbStatus } from './credential-pool/status.js';
export { loadSchedule } from './credential-pool/automation.js';
export { initCredentialPool } from './credential-pool/index.js';
