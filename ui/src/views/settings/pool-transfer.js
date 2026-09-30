/**
 * Account-pool import / export (跨机迁移).
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
 */

import { $, html, htmlEscape, raw } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { openModal, closeModal } from '../../components/modal.js';
import { confirmAction } from '../../components/confirm.js';
import { toast } from '../../components/toast.js';
import { showWbStatus, renderIdentityPool } from './credential-pool.js';
import { refreshStatus } from '../overview.js';

/** The generic detail modal's body id (see components/modal.js). */
const MODAL_BODY_ID = 'modal-req-body';

/** Read a `File` as UTF-8 text. */
function readFileText(file) {
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
function setModalBody(markup) {
  const body = $(`#${MODAL_BODY_ID}`);
  if (body) body.innerHTML = markup;
}

/** Render a preview object as the summary block the import dialog shows. */
function renderPreview(preview) {
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

/** Open the export dialog. */
export function openExportDialog() {
  openModal('📤 导出账号池', ''); // shell only; the body is replaced below
  setModalBody(html`
      <div class="wb-transfer-modal">
        <p class="wb-transfer-lead">
          导出<b>全部账号登录态与上游密钥</b>（不含端口、模型映射等本机设置），
          在另一台机器上「导入账号池」即可恢复。
        </p>
        <div class="wb-transfer-warning">
          ⚠️ 导出文件等同于密码本：任何拿到它的人都能使用你的账号。请勿通过聊天工具、邮件或网盘传输。
        </div>

        <!-- Which instance this is, and how much it will carry. A dev run
             (task dev) uses a separate data directory with its own, usually
             empty, pool; without showing both, "why did my export come out
             empty" has no visible answer until after the file exists. -->
        <div class="wb-transfer-source">
          <span class="wb-transfer-kv">导出数据目录</span>
          <span class="mono wb-transfer-path" id="wb-export-source">读取中...</span>
          <span class="badge-pill wb-state-pill wb-state-pending" id="wb-export-dev-pill" hidden>开发实例</span>
        </div>
        <div class="wb-transfer-source">
          <span class="wb-transfer-kv">本次将导出</span>
          <span id="wb-export-counts" class="wb-transfer-counts">统计中...</span>
        </div>

        <div class="form-group">
          <label for="wb-export-filename">文件名建议（可选）</label>
          <input type="text" id="wb-export-filename" class="log-input"
            placeholder="proxy-rs-pool-日期.json" autocomplete="off">
          <p class="hint">
            点「导出」后会弹出系统保存面板，文件名与保存位置都以面板里的选择为准；这里只是预填的名字。
          </p>
        </div>

        <label class="wb-checkbox-inline">
          <input type="checkbox" id="wb-export-encrypt">
          <span>使用口令加密（跨网络/网盘传输时建议开启）</span>
        </label>

        <div id="wb-export-passphrase-row" class="wb-transfer-passphrase" hidden>
          <div class="form-group">
            <label for="wb-export-passphrase">口令</label>
            <input type="password" id="wb-export-passphrase" class="log-input"
              placeholder="建议 12 位以上，忘记后无法恢复" autocomplete="new-password">
          </div>
          <div class="form-group">
            <label for="wb-export-passphrase2">再输一次</label>
            <input type="password" id="wb-export-passphrase2" class="log-input"
              placeholder="确认口令" autocomplete="new-password">
          </div>
        </div>

        <div class="hint" id="wb-export-result"></div>
      </div>
  `);

  // The shell's footer is shared by both dialogs, so wire it per open.
  const footer = $('#request-modal-overlay .modal-footer');
  if (footer) {
    footer.innerHTML = `
      <button type="button" class="btn btn-small" data-wb-transfer-cancel>取消</button>
      <button type="button" class="btn btn-small btn-primary" id="btn-wb-export-run">导出</button>
    `;
    footer.querySelector('[data-wb-transfer-cancel]')?.addEventListener('click', closeModal);
    $('#btn-wb-export-run')?.addEventListener('click', runExport);
  }

  $('#wb-export-encrypt')?.addEventListener('change', (e) => {
    const row = $('#wb-export-passphrase-row');
    if (row) row.hidden = !e.target.checked;
  });

  fillExportSource();
}

/**
 * Show which data directory this export will read from, and how much is in it.
 *
 * Best-effort: a failure here must not block the dialog, so it reports the
 * unknown state rather than throwing.
 *
 * The counts come from the same two list commands the 身份池 section already
 * renders, so this cannot disagree with what the pool shows. Showing them before
 * the click is the point: "导出得太少" is a question the dialog should answer
 * before it writes a file, not after.
 */
async function fillExportSource() {
  const label = $('#wb-export-source');
  const pill = $('#wb-export-dev-pill');
  const counts = $('#wb-export-counts');

  try {
    const status = await invoke('get_status');
    if (label) label.textContent = status?.data_dir || '(未知)';
    // A dev build reads a different directory; saying so up front is what turns
    // "my export is empty" into an obvious diagnosis.
    if (pill) pill.hidden = !status?.is_dev;
  } catch {
    if (label) label.textContent = '(无法读取)';
  }

  if (!counts) return;
  try {
    const [credsRes, keysRes] = await Promise.all([
      invoke('wb_credentials_list'),
      invoke('api_keys_list'),
    ]);
    const c = (credsRes?.credentials || []).length;
    const k = (keysRes?.keys || []).length;
    counts.textContent = `账号 ${c} 个、密钥 ${k} 个`;
    // Colour it as a problem only in the case the export would be useless.
    counts.classList.toggle('wb-tone-err', c === 0 && k === 0);
    if (c === 0 && k === 0) {
      counts.textContent += '（本机身份池为空，导出的文件将不含任何账号）';
    }
  } catch {
    counts.textContent = '(无法统计)';
  }
}

/** Run the export with whatever the dialog currently holds. */
async function runExport() {
  const btn = $('#btn-wb-export-run');
  const result = $('#wb-export-result');
  const encrypt = Boolean($('#wb-export-encrypt')?.checked);
  const passphrase = $('#wb-export-passphrase')?.value || '';
  const confirm = $('#wb-export-passphrase2')?.value || '';

  if (encrypt) {
    if (passphrase.length < 6) {
      if (result) {
        result.textContent = '口令太短：请使用至少 6 位（建议 12 位以上）';
        result.classList.add('error');
      }
      return;
    }
    if (passphrase !== confirm) {
      if (result) {
        result.textContent = '两次输入的口令不一致';
        result.classList.add('error');
      }
      return;
    }
  }

  if (btn) {
    btn.disabled = true;
    // The panel is modal to the app: say what is on screen, not "writing", so
    // the label matches what the user is looking at.
    btn.textContent = '等待选择位置...';
  }
  try {
    const res = await invoke('pool_export', {
      body: {
        passphrase: encrypt ? passphrase : null,
        filename: $('#wb-export-filename')?.value || null,
      },
    });

    // Dismissing the save panel is a normal outcome, not a failure: the backend
    // wrote nothing and the user already knows why.
    if (res?.cancelled) {
      if (result) {
        result.classList.remove('error');
        result.textContent = '已取消导出（未写入任何文件）';
      }
      return;
    }
    if (!res?.ok) throw new Error('导出未返回结果');

    // An empty export must never read as success. This is the case that produced
    // a 356-byte "backup" containing nothing: the file is written and valid, but
    // it restores no account, and the user's next step is to trust it on another
    // machine. Report the file path *and* the reason, in the warning tone.
    if (res.empty) {
      const why = (res.warnings || []).join('；') || '本机身份池为空';
      if (result) {
        result.classList.add('error');
        result.innerHTML = html`⚠️ 导出的文件<b>不包含任何账号或密钥</b>（${why}）<br>
          <span class="mono wb-transfer-path">${res.path}</span><br>
          请先确认你导出的是<b>正在使用的那份数据</b>：如果这是开发实例（<span class="mono">~/.proxy-rs-dev</span>），
          它本来就没有账号池，请在正式实例中导出。`;
      }
      toast('导出内容为空，未包含任何账号', 'error');
      showWbStatus(`导出内容为空：${res.path}（${why}）`, true);
      return;
    }

    if (result) {
      result.classList.remove('error');
      result.innerHTML = html`已导出 ${res.counts?.credentials ?? 0} 个账号、${res.counts?.api_keys ?? 0} 个密钥${raw(res.encrypted ? '（口令加密）' : '（明文）')}<br>
        <span class="mono wb-transfer-path">${res.path}</span>`;
    }
    // The passphrase is never echoed back, so clear it as soon as it is used.
    if ($('#wb-export-passphrase')) $('#wb-export-passphrase').value = '';
    if ($('#wb-export-passphrase2')) $('#wb-export-passphrase2').value = '';

    toast('账号池已导出', 'ok', {
      label: '在文件夹中显示',
      onClick: async (dismiss) => {
        await invoke('reveal_path', { path: res.path });
        dismiss();
      },
      timeout: 8000,
    });
    showWbStatus(`账号池已导出：${res.path}`);
  } catch (e) {
    if (result) {
      result.textContent = `导出失败: ${e}`;
      result.classList.add('error');
    }
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = '导出';
    }
  }
}

/** Open the import dialog. */
export function openImportDialog() {
  openModal('📥 导入账号池', ''); // shell only; the body is replaced below
  setModalBody(html`
      <div class="wb-transfer-modal">
        <p class="wb-transfer-lead">
          选择或粘贴「导出账号池」生成的文件。导入<b>不会</b>修改端口、模型映射等其他设置，
          且写入本机账号池之前会自动备份原文件。
        </p>

        <div class="actions-row">
          <input type="file" id="wb-import-file" accept=".json,application/json" hidden>
          <button type="button" class="btn btn-small" id="btn-wb-import-pick">选择文件…</button>
          <span class="hint" id="wb-import-filename">未选择文件</span>
        </div>

        <div class="form-group">
          <label for="wb-import-text">或直接粘贴导出内容</label>
          <textarea id="wb-import-text" class="log-input wb-transfer-textarea"
            placeholder="粘贴导出文件的完整 JSON 内容" spellcheck="false"></textarea>
        </div>

        <div class="form-row">
          <div class="form-group half">
            <label for="wb-import-passphrase">口令（加密文件才需要）</label>
            <input type="password" id="wb-import-passphrase" class="log-input"
              placeholder="明文文件留空" autocomplete="off">
          </div>
          <div class="form-group half">
            <label for="wb-import-mode">导入方式</label>
            <select id="wb-import-mode" class="log-input">
              <option value="merge">合并（保留本机其他账号，仅按 id 覆盖）</option>
              <option value="replace">替换（清空本机账号池后导入）</option>
            </select>
          </div>
        </div>

        <div class="hint" id="wb-import-result"></div>
      </div>
  `);

  const footer = $('#request-modal-overlay .modal-footer');
  if (footer) {
    footer.innerHTML = `
      <button type="button" class="btn btn-small" data-wb-transfer-cancel>取消</button>
      <button type="button" class="btn btn-small" id="btn-wb-import-preview">预览</button>
      <button type="button" class="btn btn-small btn-primary" id="btn-wb-import-run" disabled>确认导入</button>
    `;
    footer.querySelector('[data-wb-transfer-cancel]')?.addEventListener('click', closeModal);
    $('#btn-wb-import-preview')?.addEventListener('click', runPreview);
    $('#btn-wb-import-run')?.addEventListener('click', runImport);
  }

  $('#btn-wb-import-pick')?.addEventListener('click', () => $('#wb-import-file')?.click());
  $('#wb-import-file')?.addEventListener('change', async (e) => {
    const file = e.target.files?.[0];
    if (!file) return;
    const label = $('#wb-import-filename');
    try {
      const text = await readFileText(file);
      if ($('#wb-import-text')) $('#wb-import-text').value = text;
      if (label) label.textContent = `${file.name}（${text.length} 字节）`;
    } catch (err) {
      if (label) label.textContent = `读取失败: ${err}`;
    }
  });
}

/** Ask the backend what the pasted/selected bundle would do. */
async function runPreview() {
  const btn = $('#btn-wb-import-preview');
  const result = $('#wb-import-result');
  const text = $('#wb-import-text')?.value || '';
  if (!text.trim()) {
    if (result) {
      result.textContent = '请先选择文件或粘贴导出内容';
      result.classList.add('error');
    }
    return;
  }

  if (btn) {
    btn.disabled = true;
    btn.textContent = '预览中...';
  }
  // A new preview invalidates the previous confirmation: the text may differ.
  const runBtn = $('#btn-wb-import-run');
  if (runBtn) runBtn.disabled = true;

  try {
    const preview = await invoke('pool_import_preview', {
      body: {
        text,
        passphrase: $('#wb-import-passphrase')?.value || null,
        mode: $('#wb-import-mode')?.value || 'merge',
      },
    });
    if (!preview) throw new Error('预览未返回结果');
    if (result) {
      result.classList.remove('error');
      result.innerHTML = renderPreview(preview);
    }
    if (runBtn) runBtn.disabled = false;
  } catch (e) {
    if (result) {
      result.textContent = `预览失败: ${e}`;
      result.classList.add('error');
    }
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = '预览';
    }
  }
}

/** Apply the bundle, after the destructive-path confirmation. */
async function runImport() {
  const text = $('#wb-import-text')?.value || '';
  const mode = $('#wb-import-mode')?.value || 'merge';
  if (!text.trim()) return;

  // Replace mode deletes accounts this machine may still be using, so it asks
  // by name; merge is additive and does not need a second gate beyond 预览.
  if (mode === 'replace') {
    const ok = await confirmAction(
      '替换式导入会先清空本机现有账号池（原文件会自动备份），确定继续？',
      '确认替换导入',
    );
    if (!ok) return;
  }

  const btn = $('#btn-wb-import-run');
  const result = $('#wb-import-result');
  if (btn) {
    btn.disabled = true;
    btn.textContent = '导入中...';
  }
  try {
    const preview = await invoke('pool_import', {
      body: {
        text,
        passphrase: $('#wb-import-passphrase')?.value || null,
        mode,
      },
    });
    if (!preview) throw new Error('导入未返回结果');

    // The passphrase has been used; do not leave it in the DOM.
    if ($('#wb-import-passphrase')) $('#wb-import-passphrase').value = '';
    closeModal();
    await renderIdentityPool();
    await refreshStatus();
    const bits = [
      `账号 新增 ${preview.credentials_added} / 更新 ${preview.credentials_updated}`,
      `密钥 新增 ${preview.keys_added} / 更新 ${preview.keys_updated}`,
    ];
    toast(`账号池导入完成：${bits.join('，')}`, 'ok');
    showWbStatus(`账号池导入完成（${bits.join('，')}），如需立即使用可点「刷新积分」或重新打卡`);
  } catch (e) {
    if (result) {
      result.textContent = `导入失败: ${e}`;
      result.classList.add('error');
    }
    if (btn) {
      btn.disabled = false;
      btn.textContent = '确认导入';
    }
  }
}

/** Wire the toolbar buttons. Called once, from settings/index.js. */
export function initPoolTransfer() {
  $('#btn-wb-export')?.addEventListener('click', openExportDialog);
  $('#btn-wb-import')?.addEventListener('click', openImportDialog);
}
