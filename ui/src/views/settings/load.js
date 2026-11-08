/**
 * Loading persisted state into the form.
 *
 * Read-only with respect to settings: it fills the fields from the backend and
 * then re-baselines the badges.
 */

import { $, html, setText } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { captureBaseline, refreshSectionBadges } from './badges.js';
import { renderClientAccounts } from './client-accounts.js';

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

    // Inbound authentication switch. Absent from an older backend's payload, so
    // it reads as off rather than leaving a stale checked state on screen.
    $('#client-auth-enabled').checked = Boolean(s.client_auth_enabled);

    await applyDevGuards();

    // A null switch means "follow the provider preset": show the effective
    // value, and say so, so an unset switch is not mistaken for off.
    const presetStream = Boolean(s.force_stream_preset);
    const unset = s.force_stream === null || s.force_stream === undefined;
    $('#setting-force-stream').checked = unset ? presetStream : Boolean(s.force_stream);
    // Absent on an older settings file means off, which is also the safe default:
    // a missing field must never read as "the operator asked to skip the check".
    $('#setting-intercept-classifier').checked = Boolean(s.intercept_safety_classifier);
    setText(
      'force-stream-hint',
      unset ? `跟随服务商预设：${presetStream ? '开启' : '关闭'}` : '已手动覆盖服务商预设值',
    );

    await loadClaudeConfig();
    // The access-account list is server state, not form state: it is fetched
    // rather than read off the settings payload, and it is independent of the
    // badge baseline below.
    await renderClientAccounts();
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
    // A missing `permissions.defaultMode` (null) is the "follow the CLI's own
    // default" choice, not an unknown state: reading it as the first option is
    // what keeps a model-only load from showing a mode the file does not contain.
    //
    // A value the list does not offer (`manual` is a real alias someone may have
    // hand-written) would otherwise fall back to that same first option and read
    // as "follow the default" — a claim about the file that is simply false. It
    // gets a disabled row instead, so the file's own value stays visible while
    // remaining unselectable: `applyClaudeConfig` skips it, because re-sending a
    // mode the writer would reject turns an unrelated model edit into a failure.
    const modeSelect = $('#setting-claude-permission-mode');
    if (modeSelect) {
      // Repeated loads (every successful write calls this) must not stack rows.
      modeSelect.querySelectorAll('option[data-unrecognised]').forEach(o => o.remove());
      const mode = res.permission_mode || '';
      modeSelect.value = mode;
      if (mode && modeSelect.value !== mode) {
        const row = document.createElement('option');
        row.value = mode;
        row.dataset.unrecognised = '1';
        row.disabled = true;
        row.textContent = `${mode} — 未识别的模式（写入时保持现状）`;
        modeSelect.append(row);
        modeSelect.value = mode;
      }
    }
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
