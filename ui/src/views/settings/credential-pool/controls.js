/**
 * The identity pool's non-row controls: the toolbar entries and the key form.
 *
 * Split out of `credential-pool.js` so the orchestration in `index.js` reads as a
 * list of "what is wired", with the handlers themselves next door.
 */

import { $ } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { refreshStatus } from '../../overview.js';
import { startQrCodeLogin } from '../oauth-qr.js';
import { triggerBatchCheckin } from '../checkin.js';
import { showWbStatus } from './status.js';
import { renderIdentityPool } from './pool-table.js';

/** Toolbar entries that open flows owned by other modules. */
export function initQuickActions() {
  $('#btn-wb-qrcode')?.addEventListener('click', startQrCodeLogin);
  $('#btn-wb-checkin-all')?.addEventListener('click', triggerBatchCheckin);
}

/** The 刷新全部积分 / key form / default-identity / sticky-reset controls. */
export function initPoolControls() {
  $('#btn-wb-refresh-points')?.addEventListener('click', async () => {
    const btn = $('#btn-wb-refresh-points');
    if (btn) {
      btn.disabled = true;
      btn.textContent = '⏳ 刷新中...';
    }
    try {
      await invoke('wb_refresh_points');
      await renderIdentityPool();
      showWbStatus('已刷新全部身份的剩余积分（账号 + 密钥）');
    } catch (e) {
      showWbStatus('刷新积分失败: ' + e, true);
    } finally {
      if (btn) {
        btn.disabled = false;
        btn.textContent = '💎 刷新全部积分';
      }
    }
  });

  // Key pool: visibility toggle mirrors the provider API-key field.
  $('#btn-wb-key-visibility')?.addEventListener('click', () => {
    const input = $('#wb-new-key');
    const btn = $('#btn-wb-key-visibility');
    const show = input?.type === 'password';
    if (input) input.type = show ? 'text' : 'password';
    if (btn) btn.textContent = show ? '隐藏' : '显示';
  });

  $('#btn-wb-key-add')?.addEventListener('click', async () => {
    const key = $('#wb-new-key')?.value.trim();
    if (!key) {
      showWbStatus('请先粘贴上游 API Key', true);
      return;
    }
    try {
      await invoke('api_keys_add', { key, label: $('#wb-new-key-label')?.value || '' });
      if ($('#wb-new-key')) $('#wb-new-key').value = '';
      if ($('#wb-new-key-label')) $('#wb-new-key-label').value = '';
      showWbStatus('密钥已加入身份池');
      await renderIdentityPool();
    } catch (e) {
      showWbStatus('添加失败: ' + e, true);
    }
  });

  // Unified default-identity selector: choose a credential, a key, or "no
  // account pool" (`__none__`). Picking hot-applies via the backend, so the
  // account/key switch takes effect without a restart.
  $('#wb-default-identity-select')?.addEventListener('change', async (e) => {
    const id = e.target.value || '';
    try {
      await invoke('wb_set_default_identity', { id });
      showWbStatus(
        id === '__none__'
          ? '已切换：不使用账号池，仅用密钥 / 单 Key（立即生效）'
          : id
            ? '已设置默认身份，立即生效（无需重启）'
            : '已清除默认身份，回退到「账号优先，否则密钥 / 单 Key」',
      );
      await renderIdentityPool();
      await refreshStatus();
    } catch (err) {
      showWbStatus('设置默认身份失败: ' + err, true);
    }
  });

  $('#btn-wb-sticky-reset')?.addEventListener('click', async () => {
    try {
      await invoke('wb_sticky_reset');
      showWbStatus('已重置会话粘滞');
      await renderIdentityPool();
    } catch (e) {
      showWbStatus('重置失败: ' + e, true);
    }
  });
}
