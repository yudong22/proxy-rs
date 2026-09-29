/**
 * DSH (DeepSeek Harness) provider wiring.
 *
 * One button: write this proxy into `~/.dsh/settings.yaml` as the `proxy-rs`
 * provider, using the provider's live model list — the same shape as the Codex
 * group next door.
 *
 * The Rust side owns the write and confines it to the provider's `baseURL` and
 * `models`; this module only drives the button and reports the result.
 */

import { $ } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';

/**
 * Report the current wiring into the group's status line.
 *
 * Called on every settings load, so the line describes the file as it is now
 * rather than the last time this button ran.
 */
export async function loadDshConfig() {
  const status = $('#dsh-config-status');
  if (!status) return;
  try {
    const cfg = await invoke('get_dsh_config');
    if (!cfg?.supported) {
      status.textContent = cfg?.reason || '未找到 DSH 配置目录';
      return;
    }
    if (!cfg.settings_exists) {
      status.textContent = `尚未创建 ${cfg.settings_path}，请先启动一次 DSH`;
      return;
    }
    if (!cfg.provider_exists) {
      status.textContent = `配置中尚无 proxy-rs provider，点击写入即会创建（${cfg.settings_path}）`;
      return;
    }
    const credential = cfg.credential_present ? '凭据已就绪' : '凭据缺失，写入时会自动补齐';
    status.textContent = `当前 ${cfg.model_count} 个模型 · ${cfg.base_url || '未设置地址'} · ${credential}`;
  } catch (err) {
    status.textContent = '';
  }
}

/**
 * Write the provider and report what happened.
 *
 * The button is disabled while the request is in flight: the write reads the
 * model list from upstream first, so a second click would race the first.
 */
export async function applyDshConfig() {
  const status = $('#dsh-config-status');
  const button = $('#btn-apply-dsh-config');
  const original = button?.textContent;

  if (button) {
    button.disabled = true;
    button.textContent = '⏳ 正在写入...';
  }
  if (status) {
    status.className = 'hint';
    status.textContent = '正在拉取模型并写入...';
  }

  try {
    const res = await invoke('apply_dsh_config');
    if (status) {
      status.className = 'hint ok';
      status.textContent = `✓ 已写入 ${res.models} 个模型到 ${res.settings_path}（${res.base_url}），下次请求生效`;
    }
  } catch (err) {
    if (status) {
      status.className = 'hint err';
      status.textContent = '✗ ' + err;
    }
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = original;
    }
  }
}
