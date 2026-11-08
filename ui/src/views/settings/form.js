/**
 * The settings form skeleton.
 *
 * Group order is deliberate (see `renderSettings`). Everything that fills or
 * reads the form lives in the sibling modules.
 */

import { $, html, raw } from '../../core/dom.js';
import { input, section, toggle } from './fields.js';

/**
 * Build the settings form.
 *
 * Order is deliberate: 身份池 first (the page's primary control, and not
 * collapsible), then 模型提供商, then the per-client writers, then the two groups
 * that are only revisited when something needs changing.
 *
 * Only 模型提供商 opens by default — it is what a new user must fill in. Every
 * other collapsible group starts folded.
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
    <p class="hint">
      上游密钥在这里添加：粘贴 API Key 后会加入身份池（账号 / 密钥）并可在其中切换默认身份。
    </p>
    <!-- The key-entry row lives here rather than in 身份池: this is the
         provider section, and a key belongs to the provider being configured.
         身份池 manages which saved identity is the default, not how to add one. -->
    <div class="wb-key-add-row">
      <input type="password" id="wb-new-key" class="log-input wb-key-input"
        placeholder="粘贴上游 API Key" autocomplete="off">
      <input type="text" id="wb-new-key-label" class="log-input wb-label-input"
        placeholder="备注 (可选)" autocomplete="off">
      <button type="button" class="btn btn-small btn-primary" id="btn-wb-key-add">＋ 添加密钥</button>
      <button type="button" class="btn btn-small btn-show-hide" id="btn-wb-key-visibility">显示</button>
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
    <!-- The mode is a closed list, not a free-text field: the CLI silently
         ignores an unrecognised value and falls back to its own default, so a
         typo would look like a successful write that changed nothing. The empty
         option is the "write nothing" choice and is first, because leaving the
         mode alone is what most model-slot updates want. -->
    <div class="form-group">
      <label for="setting-claude-permission-mode">权限模式 (permissions.defaultMode)</label>
      <select id="setting-claude-permission-mode">
        <option value="">跟随 Claude Code 默认</option>
        <option value="default">default — 每次都询问</option>
        <option value="acceptEdits">acceptEdits — 自动接受文件编辑</option>
        <option value="plan">plan — 只读规划</option>
        <option value="auto">auto — 分类器逐条判定（会产生额外请求）</option>
        <option value="dontAsk">dontAsk — 会询问的一律拒绝</option>
        <option value="bypassPermissions">bypassPermissions — 全部自动通过（不再询问）</option>
      </select>
      <p class="hint">
        bypassPermissions 会取消文件写入与系统命令的<b>全部</b>确认，写入后 Claude Code
        不再就任何操作征求同意；只在对当前目录完全信任时使用。
      </p>
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

  // DSH reads its model list straight from the provider profile, so — like
  // Codex — the registry has to be written out rather than discovered. The
  // write targets the active profile's patch file (current DSH removed the old
  // ~/.dsh/settings.yaml), and only the `proxy-rs` provider's address, session
  // header and models are touched; every other row and comment is kept.
  const dsh = section('dsh', 'DSH 一键写入 (profile 的 cordis.patch.yml)', html`
    <p class="hint">
      将本代理写入 DSH 活动 profile 的 <code>llm-pi-ai.providers.proxy-rs</code>，并把当前服务商的
      <b>全部可用模型</b>同步过去。<b>只更新该 provider 的地址、会话头与模型列表</b>，
      文件中的其他内容（其他 patch 行、其他 provider、默认模型、注释）原样保留。
      同时写入 <code>sessionHeader</code>，让日志能按<b>会话</b>而非版本号归类。
      写入前会自动备份原文件。
    </p>
    <div class="actions-row">
      <button type="button" class="btn btn-small btn-primary" id="btn-apply-dsh-config">写入 DSH 配置</button>
      <span class="hint" id="dsh-config-status"></span>
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
    ${raw(toggle(
      'setting-intercept-classifier',
      '拦截 Claude Code 自动模式的安全分类器（直接放行）',
      'auto 权限模式下 Claude Code 会把待执行的命令交给一个分类器判定是否安全；开启后本网关直接返回「允许」，不再转发。',
    ))}
    <!-- The warning is not decoration. This switch turns off a safety check, and
         the gateway can be shared: everyone behind it loses the check at once,
         and they cannot see this page. It is off by default for that reason, and
         the text says so rather than letting the default speak for itself. -->
    <p class="hint" id="intercept-classifier-hint">
      <b>默认关闭。</b>开启后：<b>所有</b>经过本网关的 Claude Code 客户端的
      auto 模式判定都会得到「允许」，无论请求的是 <code>claude-sonnet-5</code> 还是其他模型 ——
      如果网关不只有你本机在用，别人会在不知情的情况下失去这道安全检查。
      仅在客户端已经用 <code>bypassPermissions</code> 全放行、只是想省掉那一次无用的分类器请求时才打开。
    </p>
  `);

  const identityPool = section('identity-pool', '身份池 (账号 / 密钥)', html`
    <p class="hint">
      账号与密钥合并管理：支持<b>微信 / QQ 扫码一键登录授权</b>并自动保存至账号池（登录态）；
      也支持直接粘贴上游 <b>API Key</b>（在上方「模型提供商」中添加）。两者可随时切换<b>默认身份</b>——
      默认身份下的请求走该账号/密钥，某会话遇到限流/超额 (429/402) 时自动切换到备用身份并保持粘滞。
    </p>

    <div class="wb-default-identity-row">
      <label for="wb-default-identity-select">默认使用的身份</label>
      <select id="wb-default-identity-select" class="log-input wb-default-identity-select">
        <option value="">（默认：账号优先，否则密钥 / 单 Key）</option>
        <option value="__none__">不使用账号池（仅用密钥 / 单 Key）</option>
      </select>
      <span class="hint wb-default-key-hint" id="wb-default-identity-hint">账号与密钥统一切换，选中即生效</span>
    </div>

    <div class="wb-pool-toolbar">
      <button type="button" class="btn btn-small btn-primary" id="btn-wb-qrcode">
        📱 扫码添加账号
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-checkin-all">
        🎁 账号池一键打卡
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-growth"
        title="自动完成 Growth 成长任务并领取积分（只做可自动化的；需你手动完成的会在面板列明）">
        📈 增长积分
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-refresh-points"
        title="刷新账号与密钥的剩余积分（全部身份）；只想刷某一个请用该行的「刷新积分」">
        💎 刷新全部积分
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-sticky-reset">
        重置会话粘滞
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-export" title="把账号与密钥导出为可携带的文件">
        📤 导出账号池
      </button>
      <button type="button" class="btn btn-small" id="btn-wb-import" title="从导出文件恢复账号与密钥">
        📥 导入账号池
      </button>
    </div>

    <div class="hint wb-transfer-hint">
      导出文件内含<b>可直接使用的登录态与密钥</b>，等同于密码本：请只在自己的设备之间传递，用完即删。
      跨机迁移时可勾选口令加密；导入不会影响其他设置，且写入前会自动备份。
    </div>

    <!-- Both automation switches live in one dialog. They are a single policy
         ("what runs by itself on this machine") that used to be two inline rows
         in different parts of the pool section, so it was easy to leave one of
         them running by accident. The summary line is what stays in the page:
         an automation that is currently ON must still be visible without
         opening anything, because a silent background job is exactly what a
         user forgets about. -->
    <div class="wb-auto-row">
      <button type="button" class="btn btn-small" id="btn-wb-automation">
        ⏱ 自动化设置
      </button>
      <span class="wb-schedule-hint" id="wb-automation-summary"></span>
    </div>

    <pre id="wb-growth-progress" class="wb-growth-progress" hidden></pre>

    <div id="wb-identity-list" class="wb-credential-list"></div>
    <div class="hint" id="wb-status"></div>
  `, { collapsible: false });

  // 访问账户 (client access accounts).
  //
  // Deliberately its own section, placed immediately after 身份池 and stated as
  // the opposite direction. Both are "lists of secrets", so the risk is real:
  // 身份池 holds the credentials the gateway presents *to* the upstream, while
  // these are the keys callers must present *to* the gateway. Merging them into
  // one list would invite exactly the mistake of pasting an inbound key where an
  // upstream key belongs (or the reverse), so the two never share a store, a
  // command or a panel.
  const clientAccounts = section('client-accounts', '访问账户 (客户端接入)', html`
    <p class="wb-transfer-hint">
      这是<b>入站</b>凭据（别人访问你的网关要用它），与上面的<b>上游</b>身份池方向相反，互不影响。
      上游身份池决定网关拿什么身份去请求模型；这里决定谁可以调用你的网关。
    </p>

    ${raw(toggle(
      'client-auth-enabled',
      '启用入站鉴权',
      '开启后，调用方必须带上 x-proxy-rs-key 请求头才能访问 /v1/*；关闭时所有调用方都可直接访问',
    ))}

    <div class="wb-pool-toolbar">
      <button type="button" class="btn btn-small btn-primary" id="btn-client-account-add">
        ＋ 添加访问账户
      </button>
    </div>

    <div id="client-account-list" class="wb-credential-list"></div>
  `);

  root.innerHTML = html`
    <form id="settings-form" class="settings-form">
      ${raw(identityPool)}
      ${raw(clientAccounts)}
      ${raw(provider)}
      ${raw(claude)}
      ${raw(codex)}
      ${raw(dsh)}
      ${raw(network)}
      ${raw(advanced)}
      <div class="settings-save-bar">
        <button type="button" class="btn btn-primary" id="btn-save-settings">保存配置</button>
        <span class="save-status" id="save-status"></span>
      </div>
    </form>
  `;
}
