/**
 * 导出账号池: the dialog that writes every login state and key to one file.
 *
 * Split out of `pool-transfer.js`. The risk this dialog carries is stated in the
 * dialog rather than left to the README, so all of it — the warning, the source
 * disclosure, the passphrase and the empty-export verdict — belongs together.
 */

import { $, html, raw } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { openModal, closeModal } from '../../../components/modal.js';
import { toast } from '../../../components/toast.js';
import { showWbStatus } from '../credential-pool.js';
import { setModalBody } from './shared.js';

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

    // HTTP-backed export (server console, and the desktop webview now that it
    // runs the same-origin console): the backend cannot write a user-chosen
    // file, so it hands back a download URL. Navigate to it — the browser owns
    // the destination, which is also why `res.path` below reads "浏览器下载".
    // (The desktop's old native save panel answered without `download_url`.)
    if (res.download_url) {
      const a = document.createElement('a');
      a.href = res.download_url;
      a.download = '';
      document.body.appendChild(a);
      a.click();
      a.remove();
    }

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

    if (res.download_url) {
      // Browser download: there is no file to reveal.
      toast('账号池已导出（浏览器下载）', 'ok', { timeout: 5000 });
      showWbStatus('账号池已导出：浏览器下载');
    } else {
      toast('账号池已导出', 'ok', {
        label: '在文件夹中显示',
        onClick: async (dismiss) => {
          await invoke('reveal_path', { path: res.path });
          dismiss();
        },
        timeout: 8000,
      });
      showWbStatus(`账号池已导出：${res.path}`);
    }
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
