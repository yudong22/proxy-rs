/**
 * 导入账号池: preview-then-apply for a pool bundle produced by the export dialog.
 *
 * Split out of `pool-transfer.js`. Preview and apply are deliberately separate
 * steps against separate backend calls — `pool_import_preview` writes nothing —
 * and keeping both beside the preview renderer is what makes that visible.
 */

import { $, html, raw } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { openModal, closeModal } from '../../../components/modal.js';
import { confirmAction } from '../../../components/confirm.js';
import { toast } from '../../../components/toast.js';
import { showWbStatus, renderIdentityPool } from '../credential-pool.js';
import { refreshStatus } from '../../overview.js';
import { readFileText, setModalBody, renderPreview } from './shared.js';

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
