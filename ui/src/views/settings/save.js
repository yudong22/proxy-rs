/**
 * Writing settings out: the form itself, the per-client config writers
 * (Claude Code / Codex), and the upstream connectivity probe.
 */

import { $, html, raw } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { refreshStatus } from '../overview.js';
import { captureBaseline, refreshSectionBadges } from './badges.js';
import { loadClaudeConfig } from './load.js';

/** Persist the form. */
export async function saveSettings() {
  const statusEl = $('#save-status');
  statusEl.className = 'save-status';
  statusEl.textContent = '保存中...';

  const payload = {
    provider_id: $('#setting-provider').value,
    custom_url: $('#setting-custom-url').value,
    // Deliberately no `api_key`: the form has no field for it, and the backend
    // treats an empty value as "keep the stored one". Posting a phantom "" would
    // be indistinguishable from clearing it.
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

/**
 * Build a model chip's inner label: id, optional name, the per-model 积分 ratio
 * (e.g. `×0.29`) and an upstream free badge (e.g. `夜间免费`). Free / zero-ratio
 * models read as `×0` so the cost is obvious at a glance.
 *
 * `html`/`raw` are the caller-provided render helpers (imported from their
 * module); passing them in keeps this pure rather than reaching into globals.
 */
export function modelChipLabel(m, { html, raw }) {
  const ratio = m.points_ratio;
  const ratioTxt = ratio == null ? '' : ` <small class="ratio">×${Number(ratio).toFixed(2)}</small>`;
  const badge = m.free_badge ? ` <small class="badge">${m.free_badge}</small>` : '';
  const nameTxt = m.name && m.name !== m.id ? ` <small>(${m.name})</small>` : '';
  return raw(`${m.id}${nameTxt}${ratioTxt}${badge}`);
}

/** Fetch the provider's model list and offer each as a clickable chip. */
export async function fetchModels() {
  const container = $('#models-container');
  container.innerHTML = '<span class="hint">正在拉取模型列表中...</span>';
  try {
    const res = await invoke('fetch_models');
    if (res?.models?.length) {
      container.innerHTML = res.models.map(m => html`
        <span class="model-chip" title="点击填入主模型" data-model="${m.id}">${
          modelChipLabel(m, { html, raw })
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
    container.innerHTML = html`<span class="hint err">拉取失败: ${err}</span>`;
  }
}

/** Write the model slots into ~/.claude/settings.json. */
export async function applyClaudeConfig() {
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
    status.className = 'hint ok';
    status.textContent = '✓ 写入 ~/.claude/settings.json 成功';
    setTimeout(() => { status.textContent = ''; }, 3000);
    await loadClaudeConfig();
  } catch (err) {
    status.className = 'hint err';
    status.textContent = '✗ 写入失败: ' + err;
  }
}

/** Write the real model list into the Codex catalog file. */
export async function applyCodexConfig() {
  const status = $('#codex-config-status');
  status.className = 'hint';
  status.textContent = '正在拉取模型并写入...';
  try {
    const res = await invoke('apply_codex_config');
    status.className = 'hint ok';
    status.textContent = `✓ 已写入 ${res.models} 个模型到 ${res.catalog_path}，重启 Codex 后生效`;
  } catch (err) {
    status.className = 'hint err';
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
