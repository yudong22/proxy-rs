/**
 * Settings view.
 *
 * Groups are native `<details>`: collapsing costs no JS. What this module adds
 * is the "已修改" badge, which marks a group holding unsaved edits, so a change
 * made in a collapsed group is still visible at a glance.
 *
 * The whole form is built here, into the empty `#tab-settings` mount point, so
 * index.html stays a shell. `renderSettings` runs once from main.js, before
 * `initSettings` and before the first `loadSettings`, because every id below is
 * what those two functions reach for.
 */

import { $, setText, html, raw } from '../core/dom.js';
import { invoke } from '../core/ipc.js';
import { appState } from '../core/state.js';
import { refreshStatus } from './overview.js';
import { openModal, closeModal } from '../components/modal.js';
import { toast } from '../components/toast.js';


/**
 * A labelled input inside a `.form-group`.
 *
 * `group` is the extra class on the wrapper — `half` is what makes a field share
 * a `.form-row` with its neighbour. It must go on the wrapper rather than the
 * input: the flex item is the `.form-group`, not the `<input>` inside it.
 */
function input(id, label, placeholder, { type = 'text', group = '', attrs = '' } = {}) {
  return html`
    <div class="form-group${raw(group ? ` ${group}` : '')}">
      <label for="${id}">${label}</label>
      <input type="${type}" id="${id}" placeholder="${placeholder}"${raw(attrs)}>
    </div>
  `;
}

/**
 * A collapsible group: `<details>` + `<summary>` + body.
 *
 * `section` doubles as the `data-section` value the badge code matches on, and
 * as the accented caret's anchor. A group with no tracked fields (codex) still
 * gets a caret so every summary reads the same.
 */
function section(section, legend, body, { open = false } = {}) {
  return html`
    <details class="form-section" data-section="${section}"${raw(open ? ' open' : '')}>
      <summary><span class="form-section-caret"></span>${legend}</summary>
      <div class="form-section-body">${raw(body)}</div>
    </details>
  `;
}

/** A `<label class="toggle">` switch, as used by the two boolean settings. */
function toggle(id, label, desc) {
  return html`
    <div class="setting-item-inline">
      <div>
        <div class="setting-label">${label}</div>
        <div class="setting-desc">${desc}</div>
      </div>
      <label class="toggle">
        <input type="checkbox" id="${id}">
        <span class="toggle-slider"></span>
      </label>
    </div>
  `;
}

/**
 * Build the settings form.
 *
 * Every group is open except the two advanced ones: the first group is what a
 * new user must fill in, while 网络与自启 and 模型重定向 (高级) are only revisited
 * when something needs changing.
 */
export function renderSettings() {
  const root = $('#tab-settings');
  if (!root) return;

  const provider = section('provider', '模型提供商', html`
    <div class="form-group">
      <label for="setting-provider">预设服务商</label>
      <select id="setting-provider"></select>
    </div>
    ${raw(input(
      'setting-custom-url',
      '自定义接口地址 (可选)',
      '留空使用服务商默认接口地址',
    ))}
    <div class="form-group">
      <label for="setting-api-key">API Key</label>
      <div class="input-with-btn">
        <input type="password" id="setting-api-key" placeholder="输入对应服务商的 API 密钥" autocomplete="off">
        <button type="button" class="btn btn-small btn-show-hide" id="btn-toggle-key-visibility">显示</button>
      </div>
    </div>
    <div class="actions-row form-actions">
      <button type="button" class="btn btn-small" id="btn-fetch-models">🔍 拉取可用模型</button>
      <button type="button" class="btn btn-small" id="btn-test-upstream-settings">⚡️ 测试连接</button>
    </div>
    <!-- Connection test result renders here, on the settings page itself. -->
    <div class="test-result" id="settings-test-result"></div>
    <div id="models-container" class="models-container"></div>
  `, { open: true });

  const claude = section('claude', 'Claude Code 一键写入 (~/.claude/settings.json)', html`
    <div class="form-row">
      ${raw(input('setting-claude-model', '默认主模型 (ANTHROPIC_MODEL)', '例如 deepseek-chat', { group: 'half' }))}
      ${raw(input('setting-claude-sonnet', 'Sonnet 槽位模型', '例如 deepseek-chat', { group: 'half' }))}
    </div>
    <div class="form-row">
      ${raw(input('setting-claude-opus', 'Opus 槽位模型', '例如 deepseek-reasoner', { group: 'half' }))}
      ${raw(input('setting-claude-haiku', 'Haiku 槽位模型', '例如 deepseek-chat', { group: 'half' }))}
    </div>
    <div class="actions-row">
      <button type="button" class="btn btn-small btn-primary" id="btn-apply-claude-config">写入 Claude Code 配置</button>
      <span class="hint" id="claude-config-status"></span>
    </div>
  `);

  const codex = section('codex', 'Codex 模型选择器 (~/.codex/config.toml)', html`
    <p class="hint">
      Codex 不会读取自定义 provider 的 <code>/v1/models</code>，因此选择器只显示内置模型。
      此操作会把本 provider 的真实模型写入 <code>model_catalog_json</code> 目录文件。
    </p>
    <div class="actions-row">
      <button type="button" class="btn btn-small btn-primary" id="btn-apply-codex-config">写入 Codex 模型目录</button>
      <span class="hint" id="codex-config-status"></span>
    </div>
  `);

  const network = section('network', '网络与自启', html`
    <div class="form-row">
      ${raw(input('setting-port', '本地端口', '3456', { type: 'number', group: 'half', attrs: ' min="1024" max="65535"' }))}
      ${raw(input('setting-bind', '绑定 IP', '127.0.0.1', { group: 'half' }))}
    </div>
    ${raw(toggle('setting-launch-at-login', '开机自动启动', '登录 macOS 时自动在后台启动代理服务'))}
  `);

  const advanced = section('advanced', '模型重定向 (高级)', html`
    <div class="form-row">
      ${raw(input('setting-reasoning-model', '思考模型重写 (Reasoning Model)', '留空使用默认', { group: 'half' }))}
      ${raw(input('setting-completion-model', '普通补全模型重写 (Completion Model)', '留空使用默认', { group: 'half' }))}
    </div>
    ${raw(input(
      'setting-model-map',
      '映射列表 (源模型:目标模型，逗号隔开)',
      'claude-3-5-sonnet:deepseek-chat,claude-3-opus:deepseek-reasoner',
    ))}
    ${raw(input(
      'setting-sanitize-terms',
      '指纹清洗短语 (高级，分号隔开)',
      '留空使用内置默认；用于覆盖/追加上游内容过滤敏感的固定模板句',
    ))}
    ${raw(toggle(
      'setting-force-stream',
      '强制流式请求上游',
      '该服务商不支持非流式时开启：所有请求都以流式发送，非流式客户端仍收到一次完整响应',
    ))}
    <div class="hint" id="force-stream-hint"></div>
  `);

  const credentials = section('credentials', 'WorkBuddy 账号池 (登录态)', html`
    <p class="hint">
      支持<b>微信 / QQ 扫码一键登录授权</b>并自动保存至账号池；
      也可以一键为账号池所有账号<b>每日打卡领积分</b>。默认凭据健康时所有请求走默认凭据；
      某会话遇到限流/超额 (429/402) 时自动切换到备用凭据并保持粘滞。
    </p>

    <div class="wb-pool-toolbar">
      <button type="button" class="btn btn-small btn-primary" id="btn-wb-qrcode">
        📱 扫码添加账号
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-checkin-all">
        🎁 账号池一键打卡
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-sticky-reset">
        重置会话粘滞
      </button>
      <button type="button" class="btn btn-small btn-ghost" id="btn-wb-toggle-manual">
        ⚙️ 手动粘贴 JSON
      </button>
    </div>

    <!-- Collapsible manual JSON import -->
    <div id="wb-manual-section" class="wb-manual-section" style="display: none;">
      <div class="form-group">
        <label for="wb-login-state">登录态 JSON</label>
        <textarea id="wb-login-state" class="log-input" rows="3"
          placeholder='{"auth": {"accessToken": "...", "refreshToken": "..."}, "account": {"uid": "..."}}'
          autocomplete="off"></textarea>
      </div>
      <div class="form-row">
        ${raw(input('wb-label', '备注标签 (可选)', '例如 工作号', { group: 'half' }))}
        <div class="form-group half">
          <div class="actions-row">
            <button type="button" class="btn btn-small btn-primary" id="btn-wb-add">＋ 导入凭据</button>
          </div>
        </div>
      </div>
    </div>

    <div id="wb-credential-list" class="wb-credential-list"></div>
    <div class="hint" id="wb-status"></div>
  `);


  root.innerHTML = html`
    <form id="settings-form" class="settings-form">
      ${raw(provider)}
      ${raw(credentials)}
      ${raw(claude)}
      ${raw(codex)}
      ${raw(network)}
      ${raw(advanced)}
      <div class="settings-save-bar">
        <button type="button" class="btn btn-primary" id="btn-save-settings">保存配置</button>
        <span class="save-status" id="save-status"></span>
      </div>
    </form>
  `;
}

/**
 * Snapshot of every tracked field as the backend last reported it.
 *
 * The badge means "this value has been changed since it was loaded", so it is
 * measured against this snapshot rather than against a hard-coded default
 * table. A table would have to duplicate the provider-preset logic — e.g.
 * `force_stream: null` means "follow the preset", whose effective value only
 * the backend knows — and would flag untouched fields as modified.
 */
const baseline = new Map();

/** IDs whose value the badges track, grouped by the section they belong to. */
const SECTION_FIELDS = {
  provider: ['setting-provider', 'setting-custom-url', 'setting-api-key'],
  network: ['setting-port', 'setting-bind', 'setting-launch-at-login'],
  claude: ['setting-claude-model', 'setting-claude-sonnet', 'setting-claude-opus', 'setting-claude-haiku'],
  codex: [],
  advanced: [
    'setting-reasoning-model',
    'setting-completion-model',
    'setting-model-map',
    'setting-sanitize-terms',
    'setting-force-stream',
  ],
};

/** Current value of a tracked field, normalised for comparison. */
function fieldValue(el) {
  return el.type === 'checkbox' ? el.checked : String(el.value);
}

/** Record the form's current values as the new baseline (badges clear). */
function captureBaseline() {
  baseline.clear();
  for (const id of Object.values(SECTION_FIELDS).flat()) {
    const el = document.getElementById(id);
    if (el) baseline.set(id, fieldValue(el));
  }
}

/** True when the field differs from the value captured at load time. */
function isModified(id) {
  const el = document.getElementById(id);
  if (!el || !baseline.has(id)) return false;
  return fieldValue(el) !== baseline.get(id);
}


/**
 * Recompute every group's badge. Called after the form is loaded and after any
 * edit, so the badge tracks the form rather than the last saved state.
 */
export function refreshSectionBadges() {
  for (const [section, fields] of Object.entries(SECTION_FIELDS)) {
    const group = document.querySelector(`.form-section[data-section="${section}"]`);
    if (!group) continue;

    const summary = group.querySelector('summary');
    const modified = fields.some(isModified);
    let badge = summary?.querySelector('.form-section-badge');

    if (modified && !badge && summary) {
      badge = document.createElement('span');
      badge.className = 'form-section-badge';
      badge.textContent = '已修改';
      summary.appendChild(badge);
    } else if (!modified && badge) {
      badge.remove();
    }
  }
}

/** Populate the provider dropdown from the backend's built-in presets. */
async function loadProviders() {
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
    $('#setting-api-key').value = s.api_key || '';
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
async function applyDevGuards() {
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
async function loadClaudeConfig() {
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
async function loadCodexConfig() {
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

/** Persist the form. */
async function saveSettings() {
  const statusEl = $('#save-status');
  statusEl.className = 'save-status';
  statusEl.textContent = '保存中...';

  const payload = {
    provider_id: $('#setting-provider').value,
    custom_url: $('#setting-custom-url').value,
    api_key: $('#setting-api-key').value,
    port: parseInt($('#setting-port').value, 10) || 3456,
    bind: $('#setting-bind').value || '127.0.0.1',
    reasoning_model: $('#setting-reasoning-model').value,
    completion_model: $('#setting-completion-model').value,
    model_map: $('#setting-model-map').value,
    launch_at_login: $('#setting-launch-at-login').checked,
    sanitize_terms: $('#setting-sanitize-terms').value,
    force_stream: $('#setting-force-stream').checked,
  };

  try {
    await invoke('save_settings', { body: payload });
    statusEl.className = 'save-status ok';
    statusEl.textContent = '✓ 配置保存成功';
    // The form is now the persisted state, so nothing is pending: re-baseline
    // and the badges clear.
    captureBaseline();
    refreshSectionBadges();
    await refreshStatus();
    setTimeout(() => { statusEl.textContent = ''; }, 3000);
  } catch (err) {
    statusEl.className = 'save-status err';
    statusEl.textContent = '✗ 保存失败: ' + err;
  }
}

/** Fetch the provider's model list and offer each as a clickable chip. */
async function fetchModels() {
  const container = $('#models-container');
  container.innerHTML = '<span class="hint">正在拉取模型列表中...</span>';
  try {
    const res = await invoke('fetch_models');
    if (res?.models?.length) {
      container.innerHTML = res.models.map(m => html`
        <span class="model-chip" title="点击填入主模型" data-model="${m.id}">${
          raw(`${m.id}${m.name ? ` <small>(${m.name})</small>` : ''}`)
        }</span>`).join('');

      for (const chip of container.querySelectorAll('.model-chip')) {
        chip.addEventListener('click', () => {
          const id = chip.dataset.model;
          $('#setting-claude-model').value = id;
          $('#setting-claude-sonnet').value = id;
        });
      }
    } else {
      container.innerHTML = '<span class="hint">未找到可用模型或当前提供商不支持模型列表查询</span>';
    }
  } catch (err) {
    container.innerHTML = html`<span class="hint" style="color: var(--error);">拉取失败: ${err}</span>`;
  }
}

/** Write the model slots into ~/.claude/settings.json. */
async function applyClaudeConfig() {
  const status = $('#claude-config-status');
  status.textContent = '正在写入...';
  try {
    const body = {
      model: $('#setting-claude-model').value.trim() || undefined,
      sonnet: $('#setting-claude-sonnet').value.trim() || undefined,
      opus: $('#setting-claude-opus').value.trim() || undefined,
      haiku: $('#setting-claude-haiku').value.trim() || undefined,
    };
    await invoke('apply_claude_config', { body });
    status.style.color = 'var(--success)';
    status.textContent = '✓ 写入 ~/.claude/settings.json 成功';
    setTimeout(() => { status.textContent = ''; }, 3000);
    await loadClaudeConfig();
  } catch (err) {
    status.style.color = 'var(--error)';
    status.textContent = '✗ 写入失败: ' + err;
  }
}

/** Write the real model list into the Codex catalog file. */
async function applyCodexConfig() {
  const status = $('#codex-config-status');
  status.style.color = '';
  status.textContent = '正在拉取模型并写入...';
  try {
    const res = await invoke('apply_codex_config');
    status.style.color = 'var(--success)';
    status.textContent = `✓ 已写入 ${res.models} 个模型到 ${res.catalog_path}，重启 Codex 后生效`;
  } catch (err) {
    status.style.color = 'var(--error)';
    status.textContent = '✗ ' + err;
  }
}

/**
 * Probe the upstream with a real streaming request. Renders in place on the
 * settings page rather than jumping back to the overview.
 */
export async function testUpstream(resultContainer) {
  if (!resultContainer) return;
  resultContainer.textContent = '正在向上游发起测试请求...';
  resultContainer.className = 'test-result';
  try {
    const res = await invoke('test_upstream', { body: {} });
    if (res?.ok) {
      resultContainer.className = 'test-result ok';
      resultContainer.textContent = `✓ 连通性测试通过 (${res.detail || '上游流式响应正常'})`;
    } else {
      resultContainer.className = 'test-result err';
      resultContainer.textContent = `✗ 测试异常: ${res ? res.detail : '未收到有效内容'}`;
    }
  } catch (err) {
    resultContainer.className = 'test-result err';
    resultContainer.textContent = `✗ 连接失败: ${err}`;
  }
}

/** Wire every control in the settings form. Called once, from main.js. */
export function initSettings() {
  $('#btn-toggle-key-visibility')?.addEventListener('click', () => {
    const input = $('#setting-api-key');
    const btn = $('#btn-toggle-key-visibility');
    appState.keyVisible = !appState.keyVisible;
    input.type = appState.keyVisible ? 'text' : 'password';
    btn.textContent = appState.keyVisible ? '隐藏' : '显示';
  });

  $('#btn-save-settings')?.addEventListener('click', saveSettings);
  $('#btn-fetch-models')?.addEventListener('click', fetchModels);
  $('#btn-apply-claude-config')?.addEventListener('click', applyClaudeConfig);
  $('#btn-apply-codex-config')?.addEventListener('click', applyCodexConfig);
  $('#btn-test-upstream-settings')?.addEventListener('click', () => {
    testUpstream($('#settings-test-result'));
  });

  // Keep the "已修改" badges in step with the form as it is edited. `input`
  // covers typing and pasting; `change` covers selects and checkboxes.
  const form = $('#settings-form');
  form?.addEventListener('input', refreshSectionBadges);
  form?.addEventListener('change', refreshSectionBadges);

  initCredentialPool();
  loadCodexConfig();
}

// ── WorkBuddy credential pool management ───────────────────────────────────

/** State label → badge class, so a cooled credential reads differently. */
const WB_STATE_LABELS = {
  ok: ['正常', 'wb-state-ok'],
  cooldown: ['冷却中', 'wb-state-cooldown'],
  expired: ['已过期', 'wb-state-expired'],
  disabled: ['已禁用', 'wb-state-disabled'],
};

/** Repaint the credential list from the backend. */
async function renderCredentialList() {
  const listEl = $('#wb-credential-list');
  if (!listEl) return;
  try {
    const res = await invoke('wb_credentials_list');
    const items = res?.credentials || [];
    if (!items.length) {
      listEl.innerHTML = html`<div class="hint">账号池暂无可用账号。点击上方「📱 扫码添加账号」完成微信/QQ 扫码即可添加。</div>`;
      return;
    }
    listEl.innerHTML = items.map((c) => {
      const [stateLabel, stateClass] = WB_STATE_LABELS[c.state] || ['未知', ''];
      const stickyNote = c.sticky_sessions > 0
        ? raw(`<span class="wb-sticky-note">${c.sticky_sessions} 个会话粘滞</span>`)
        : '';
      return html`<div class="wb-credential-row">
        <div class="wb-credential-main">
          <span class="wb-credential-label">${c.label || c.nickname || c.id}</span>
          <span class="badge-pill wb-state-pill ${stateClass}">${stateLabel}</span>
          ${stickyNote}
          <span class="wb-credential-id mono" title="${c.masked_token}">${c.id}</span>
        </div>
        <div class="wb-credential-actions">
          <button type="button" class="btn btn-small" data-wb-checkin="${c.id}" title="为此账号每日打卡领积分">打卡</button>
          <button type="button" class="btn btn-small" data-wb-toggle="${c.id}" data-wb-enabled="${c.enabled ? '1' : '0'}">
            ${c.enabled ? '禁用' : '启用'}
          </button>
          <button type="button" class="btn btn-small btn-danger" data-wb-delete="${c.id}">删除</button>
        </div>
      </div>`;
    }).join('');

    for (const btn of listEl.querySelectorAll('[data-wb-checkin]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.wbCheckin;
        btn.disabled = true;
        btn.textContent = '...';
        try {
          const res = await invoke('wb_checkin_single', { id });
          const isOk = res.status === 'success' || res.status === 'already_checked_in';
          toast(`${res.label || id}: ${res.message}`, isOk ? 'ok' : 'error');
        } catch (e) {
          toast(`打卡失败: ${e}`, 'error');
        } finally {
          btn.disabled = false;
          btn.textContent = '打卡';
        }
      });
    }

    for (const btn of listEl.querySelectorAll('[data-wb-toggle]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.wbToggle;
        const enable = btn.dataset.wbEnabled !== '1';
        try {
          await invoke('wb_credentials_toggle', { id, enabled: enable });
          await renderCredentialList();
        } catch (e) {
          showWbStatus('操作失败: ' + e, true);
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-wb-delete]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.wbDelete;
        try {
          await invoke('wb_credentials_delete', { id });
          await renderCredentialList();
          showWbStatus('凭据已删除（重启代理后完全生效）');
        } catch (e) {
          showWbStatus('删除失败: ' + e, true);
        }
      });
    }
  } catch (e) {
    listEl.innerHTML = html`<div class="hint">凭据加载失败: ${String(e)}</div>`;
  }
}

function showWbStatus(message, isError = false) {
  const el = $('#wb-status');
  if (!el) return;
  el.textContent = message;
  el.classList.toggle('error', isError);
}

/** Active OAuth polling timer reference. */
let oauthPollTimer = null;

function clearOAuthPolling() {
  if (oauthPollTimer) {
    clearInterval(oauthPollTimer);
    oauthPollTimer = null;
  }
}

/** Start QR code OAuth login flow. */
async function startQrCodeLogin() {
  clearOAuthPolling();
  openModal(
    '📱 扫码添加 WorkBuddy 账号',
    html`
      <div class="wb-qr-modal">
        <p class="wb-qr-tip">请使用微信或腾讯客户端扫描下方二维码完成授权：</p>
        <div id="wb-qr-target" class="wb-qr-box">
          <div class="wb-qr-loading">正在请求授权二维码...</div>
        </div>
        <div class="wb-qr-poll-status" id="wb-qr-poll-status">
          <span class="wb-pulse-dot"></span> 等待扫码确认...
        </div>
        <div class="wb-qr-link-row">
          <a id="wb-qr-browser-link" href="#" target="_blank" class="wb-external-link">在浏览器中打开授权页面 ↗</a>
        </div>
      </div>
    `
  );

  try {
    const res = await invoke('wb_oauth_start');
    const { state, auth_url, qr_svg } = res;

    const qrTarget = $('#wb-qr-target');
    if (qrTarget && qr_svg) {
      qrTarget.innerHTML = qr_svg;
    }

    const browserLink = $('#wb-qr-browser-link');
    if (browserLink && auth_url) {
      browserLink.href = auth_url;
    }

    // Start polling every 2 seconds
    oauthPollTimer = setInterval(async () => {
      // Check if modal is still open
      const overlay = $('#request-modal-overlay');
      if (!overlay || !overlay.classList.contains('active')) {
        clearOAuthPolling();
        return;
      }

      try {
        const poll = await invoke('wb_oauth_poll', { state });
        const statusEl = $('#wb-qr-poll-status');

        if (poll.status === 'success') {
          clearOAuthPolling();
          if (statusEl) {
            statusEl.innerHTML = html`<span style="color:var(--success, #2e8b45);font-weight:600;">✅ 扫码成功！账号已自动加入账号池</span>`;
          }
          toast(`WorkBuddy 账号「${poll.credential?.label || '新账号'}」已加入账号池！`, 'ok');
          await renderCredentialList();
          setTimeout(() => closeModal(), 1500);
        } else if (poll.status === 'failed') {
          clearOAuthPolling();
          if (statusEl) {
            statusEl.innerHTML = html`<span style="color:var(--error, #e53e3e);">❌ 授权失败: ${poll.error}</span>`;
          }
        }
      } catch (err) {
        clearOAuthPolling();
        const statusEl = $('#wb-qr-poll-status');
        if (statusEl) {
          statusEl.innerHTML = html`<span style="color:var(--error, #e53e3e);">轮询异常: ${err}</span>`;
        }
      }
    }, 2000);
  } catch (err) {
    const qrTarget = $('#wb-qr-target');
    if (qrTarget) {
      qrTarget.innerHTML = html`<div style="color:var(--error, #e53e3e);padding:20px;">生成授权二维码失败: ${err}</div>`;
    }
  }
}

/** Trigger batch checkin across all pool accounts and show report modal. */
async function triggerBatchCheckin() {
  const btn = $('#btn-wb-checkin-all');
  if (btn) {
    btn.disabled = true;
    btn.textContent = '⏳ 正在打卡...';
  }

  try {
    const report = await invoke('wb_checkin_all');
    if (report.total === 0) {
      toast('账号池中没有已启用的账号', 'error');
      return;
    }

    const rows = (report.details || []).map((d) => {
      let badge = '';
      if (d.status === 'success') {
        badge = '<span class="badge-pill wb-state-pill wb-state-ok">打卡成功</span>';
      } else if (d.status === 'already_checked_in') {
        badge = '<span class="badge-pill wb-state-pill" style="background:rgba(59,130,246,0.15);color:#3b82f6;">今日已打卡</span>';
      } else {
        badge = '<span class="badge-pill wb-state-pill wb-state-expired">失败</span>';
      }

      return html`
        <tr>
          <td style="font-weight:600;">${d.label} <span class="mono" style="color:var(--muted);font-size:11px;">(${d.id})</span></td>
          <td>${raw(badge)}</td>
          <td style="font-size:12px;">${d.message}</td>
        </tr>
      `;
    }).join('');

    openModal(
      '🎁 账号池每日打卡结果',
      html`
        <div class="wb-checkin-report-modal">
          <div class="wb-checkin-summary-cards">
            <div class="wb-stat-card">
              <span class="wb-stat-val">${report.total}</span>
              <span class="wb-stat-label">总账号数</span>
            </div>
            <div class="wb-stat-card">
              <span class="wb-stat-val" style="color:var(--success, #2e8b45);">${report.success}</span>
              <span class="wb-stat-label">本次领取</span>
            </div>
            <div class="wb-stat-card">
              <span class="wb-stat-val" style="color:#3b82f6;">${report.already_checked_in}</span>
              <span class="wb-stat-label">今日已打卡</span>
            </div>
            <div class="wb-stat-card">
              <span class="wb-stat-val" style="color:var(--error, #e53e3e);">${report.failed}</span>
              <span class="wb-stat-label">失败</span>
            </div>
          </div>

          <table class="wb-checkin-table">
            <thead>
              <tr>
                <th>账号</th>
                <th>状态</th>
                <th>详情</th>
              </tr>
            </thead>
            <tbody>
              ${raw(rows)}
            </tbody>
          </table>
        </div>
      `
    );

    toast(`打卡完成：${report.success} 个成功，${report.already_checked_in} 个已打卡`, 'ok');
  } catch (err) {
    toast(`每日打卡失败: ${err}`, 'error');
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = '🎁 账号池一键打卡';
    }
  }
}

/** Wire the credential-pool controls and paint the initial list. */
function initCredentialPool() {
  $('#btn-wb-qrcode')?.addEventListener('click', startQrCodeLogin);
  $('#btn-wb-checkin-all')?.addEventListener('click', triggerBatchCheckin);

  $('#btn-wb-toggle-manual')?.addEventListener('click', () => {
    const sec = $('#wb-manual-section');
    if (!sec) return;
    const isHidden = sec.style.display === 'none';
    sec.style.display = isHidden ? 'block' : 'none';
  });

  $('#btn-wb-add')?.addEventListener('click', async () => {
    const loginState = $('#wb-login-state')?.value.trim();
    if (!loginState) {
      showWbStatus('请先粘贴登录态 JSON', true);
      return;
    }
    try {
      await invoke('wb_credentials_add', {
        body: { loginState, label: $('#wb-label')?.value || '' },
      });
      if ($('#wb-login-state')) $('#wb-login-state').value = '';
      if ($('#wb-label')) $('#wb-label').value = '';
      showWbStatus('凭据已导入。重启代理后以账号身份请求上游。');
      await renderCredentialList();
    } catch (e) {
      showWbStatus('导入失败: ' + e, true);
    }
  });

  $('#btn-wb-sticky-reset')?.addEventListener('click', async () => {
    try {
      await invoke('wb_sticky_reset');
      showWbStatus('已重置会话粘滞');
      await renderCredentialList();
    } catch (e) {
      showWbStatus('重置失败: ' + e, true);
    }
  });

  renderCredentialList();
}

