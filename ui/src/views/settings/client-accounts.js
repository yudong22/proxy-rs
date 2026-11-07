/**
 * Client access accounts (入站凭据) — public surface.
 *
 * ## Why this is not 身份池
 *
 * 身份池 (`credential-pool/`) holds **outbound** credentials: the identities the
 * gateway presents *to* the upstream. This module holds the opposite thing —
 * **inbound** keys that callers must present *to* the gateway. Both exist, both
 * are lists of secrets, and confusing them would either leak a caller's key to
 * the upstream or let an unauthorised caller spend the pool's quota. So the two
 * live in separate files, separate stores and separate sections, and the panel
 * says which direction it is in its first line.
 *
 * ## The plaintext key is shown exactly once
 *
 * `client_account_create` returns the raw key on creation and never again; the
 * backend only stores a hash. The dialog that shows it is therefore the only
 * place a user can ever obtain it, which is why it states that in the body
 * rather than in a tooltip.
 *
 * ## Where the implementation lives
 *
 *   shared.js        the request-header constant, `formatTimestamp`, `copyText`
 *   create-dialog.js the 添加访问账户 form
 *   reveal-dialog.js the one-time plaintext key disclosure
 *   list.js          `renderClientAccounts` and the row actions
 *   index.js         `initClientAccounts`
 *
 * The exports below are exactly the set this module had before the split.
 */

export { renderClientAccounts } from './client-accounts/list.js';
export { initClientAccounts } from './client-accounts/index.js';
