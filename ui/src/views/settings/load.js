/**
 * Loading persisted state into the form.
 *
 * Read-only with respect to settings: it fills the fields from the backend and
 * then re-baselines the badges.
 */

import { $, html, setText } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { captureBaseline, refreshSectionBadges } from './badges.js';

/** Populate the provider dropdown from the backend's built-in presets. */
export async function loadProviders() {
  try {
    const res = await invoke('get_providers');
    if (!res?.providers) return;
    const select = $('#setting-provider');
    if (!select) return;
    select.innerHTML = res.providers
      .map(p => html`<option value="${p.id}">${p.name}</option>`)
      .join('') + '<option value="custom">自定义服务商 (Custom)</option>';
  } catch (err) {
    console.error('loadProviders error:', err);
  }
}

/** Load the persisted settings into the form. */
export async function loadSettings() {
  await loadProviders();
  try {
    const s = await invoke('get_settings');
    if (!s) return;

    $('#setting-provider').value = s.provider_id || 'workbuddy-cn';
    $('#setting-custom-url').value = s.custom_url || '';
    // No api_key field: this form no longer carries one (keys live in the pool,
    // added from 模型提供商). The backend therefore keeps the stored value when
    // the field is absent, instead of clearing it.
    $('#setting-port').value = s.port || 3456;
    $('#setting-bind').value = s.bind || '127.0.0.1';
    $('#setting-reasoning-model').value = s.reasoning_model || '';
    $('#setting-completion-model').value = s.completion_model || '';
    $('#setting-model-map').value = s.model_map || '';
    $('#setting-sanitize-terms').value = s.sanitize_terms || '';

    // Reflect the user's persisted intent; whether the job is actually live is
    // reported separately by get_status as `launch_at_login_stale`.
    $('#setting-launch-at-login').checked = Boolean(s.launch_at_login);
    await applyDevGuards();

    // A null switch means "follow the provider preset": show the effective
    // value, and say so, so an unset switch is not mistaken for off.
    const presetStream = Boolean(s.force_stream_preset);
    const unset = s.force_stream === null || s.force_stream === undefined;
    $('#setting-force-stream').checked = unset ? presetStream : Boolean(s.force_stream);
    setText(
      'force-stream-hint',
      unset ? `跟随服务商预设：${presetStream ? '开启' : '关闭'}` : '已手动覆盖服务商预设值',
    );

    await loadClaudeConfig();
    // Everything loaded: this is the state the badges measure against.
    captureBaseline();
    refreshSectionBadges();
  } catch (err) {
    console.error('loadSettings error:', err);
  }
}

/**
 * Disable controls that a development build must not use.
 *
 * A debug build shares the installed app's launchd label, so writing the login
 * item from `task dev` would point the user's next login at a development
 * binary sitting in `target/debug`. The switch is disabled here rather than
 * only hidden, so the reason is visible.
 */
export async function applyDevGuards() {
  let status;
  try {
    status = await invoke('get_status');
  } catch {
    return;
  }
  if (!status?.is_dev) return;

  const toggle = $('#setting-launch-at-login');
  if (!toggle) return;
  toggle.disabled = true;
  toggle.checked = false;

  const desc = toggle.closest('.setting-item-inline')?.querySelector('.setting-desc');
  if (desc) {
    desc.textContent = '开发版（task dev）共用正式版的开机自启配置，已禁用以免启动到开发二进制';
  }
}

/** Load the current ~/.claude/settings.json values into the Claude Code group. */
export async function loadClaudeConfig() {
  try {
    const res = await invoke('get_claude_config');
    if (!res) return;
    const env = res.env || {};
    // Prioritise ANTHROPIC_MODEL; only fall back to the top-level model when it
    // is not the generic 'sonnet' alias.
    const model = env.ANTHROPIC_MODEL || (res.model && res.model !== 'sonnet' ? res.model : '');
    $('#setting-claude-model').value = model;
    $('#setting-claude-sonnet').value = env.ANTHROPIC_DEFAULT_SONNET_MODEL || '';
    $('#setting-claude-opus').value = env.ANTHROPIC_DEFAULT_OPUS_MODEL || '';
    $('#setting-claude-haiku').value = env.ANTHROPIC_DEFAULT_HAIKU_MODEL || '';
  } catch (e) {
    console.warn('loadClaudeConfig error:', e);
  }
}

/** Report the Codex catalog wiring so the UI can show the current state. */
export async function loadCodexConfig() {
  const status = $('#codex-config-status');
  if (!status) return;
  try {
    const cfg = await invoke('get_codex_config');
    if (!cfg.supported) {
      status.textContent = cfg.reason || '未找到 Codex 配置目录';
      return;
    }
    status.textContent = cfg.catalog_exists
      ? '当前目录: ' + cfg.catalog_path
      : '尚未写入目录文件: ' + cfg.config_path;
  } catch (err) {
    status.textContent = '';
  }
}
