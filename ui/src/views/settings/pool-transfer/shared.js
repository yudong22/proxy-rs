/**
 * Plumbing shared by the two pool-transfer dialogs.
 *
 * Split out of `pool-transfer.js`: the export and import dialogs both render
 * into the one generic modal shell and both need to read a `File`, so the
 * helpers live here instead of being duplicated or one dialog importing the
 * other's file.
 */

import { $, html, htmlEscape, raw } from '../../../core/dom.js';

/** The generic detail modal's body id (see components/modal.js). */
const MODAL_BODY_ID = 'modal-req-body';

/** Read a `File` as UTF-8 text. */
export function readFileText(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result || ''));
    reader.onerror = () => reject(new Error('无法读取所选文件'));
    reader.readAsText(file);
  });
}

/**
 * Render markup into the open modal's body.
 *
 * The shell is generic (`components/modal.js` owns the one instance), so the
 * dialogs below replace its body wholesale on each open rather than splicing
 * into whatever the previous caller left there.
 */
export function setModalBody(markup) {
  const body = $(`#${MODAL_BODY_ID}`);
  if (body) body.innerHTML = markup;
}

/** Render a preview object as the summary block the import dialog shows. */
export function renderPreview(preview) {
  const tone = (n) => (n > 0 ? ' wb-tone-ok' : '');
  // The card markup is built by the escaping template and then nested, so each
  // call is wrapped in raw(): a bare string interpolation would be escaped and
  // the user would see markup instead of numbers.
  const cell = (label, value, cls = '') => html`
    <div class="wb-stat-card">
      <span class="wb-stat-val${raw(cls)}">${value}</span>
      <span class="wb-stat-label">${label}</span>
    </div>`;

  const warnings = preview.warnings?.length
    ? html`<ul class="wb-transfer-warnings">
        ${raw(preview.warnings.map((w) => `<li>${htmlEscape(w)}</li>`).join(''))}
      </ul>`
    : '';

  // "Default identity" is the one line a user must not have to guess at: it is
  // what every request will use after the import.
  const identity = preview.default_identity_applied
    ? preview.default_identity_applied
    : preview.default_identity_dropped
      ? '未设置（原默认已失效被清空）'
      : '未变更';

  return html`
    <div class="wb-transfer-preview">
      <div class="wb-transfer-cards">
        ${raw(cell('账号总数', preview.credentials_total))}
        ${raw(cell('账号新增', preview.credentials_added, tone(preview.credentials_added)))}
        ${raw(cell('账号更新', preview.credentials_updated, tone(preview.credentials_updated)))}
        ${raw(cell('账号不变', preview.credentials_unchanged))}
        ${raw(cell('密钥新增', preview.keys_added, tone(preview.keys_added)))}
        ${raw(cell('密钥更新', preview.keys_updated, tone(preview.keys_updated)))}
      </div>
      ${raw(preview.mode === 'replace'
        ? html`<p class="hint wb-transfer-replace-note">
            替换式导入：本机现有账号将被清空（已备份原文件），仅保留本次导入的内容。
          </p>`
        : '')}
      <div class="wb-transfer-identity">
        <span class="wb-transfer-kv">默认身份</span>
        <span class="mono">${identity}</span>
      </div>
      ${raw(warnings)}
    </div>
  `;
}
