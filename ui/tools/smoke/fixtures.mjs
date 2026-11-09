/**
 * Everything the harness asserts against: the mock backend, the expected values
 * and the two HTTP endpoints.
 *
 * Split out of `smoke.mjs` so the scenario modules can be read as behaviour
 * without scrolling past three hundred lines of fixture data. The values are
 * unchanged — they are the record of what the UI produced before the module
 * split, which is the whole point of keeping them.
 */

/** A mock Rust backend: the shapes the UI actually reads out of each command. */
const MOCK = {
  get_status: {
    running: true, port: 3456, provider: 'workbuddy-cn', version: '9.9.9',
    upstream_url: 'https://example.test/v1', log_path: '/tmp/proxy.log',
    data_dir: '/Users/test/.proxy-rs',
    launch_at_login: false, is_dev: false,
    current_identity: { id: 'cred-1', label: '测试账号一', points: 1234.5 },
  },
  get_stats: {
    requests_total: 1234, tokens_total: 2500000, cache_hit_pct: 42, requests_failed: 7,
    tokens_input: 100, tokens_cache_read: 200, tokens_cache_write: 300,
    tokens_output: 400, overrides_total: 5,
  },
  get_session_metrics: {
    speed_tps: 42.5, model: 'deepseek-chat', measured_turns: 3, output_tokens: 500,
    session_id: 'dsh:0.1.6',
    // Today's request count and cache-hit rate for this session (see
    // stats::SessionMetrics). `cache_hit_pct` is null when unmeasured.
    requests: 7, cache_hit_pct: 62,
    // The current session's own lineage count. The mock reports one child so
    // the header renders the suffix; `subagent_sessions: 0` below pins that a
    // session with none renders nothing at all.
    subagent_sessions: 1,
    is_subagent: false,
    recent: [{
      // A cumulative tool wait in the hours range, so this fixture exercises
      // `formatLongDuration`'s minutes path rather than a sub-minute value that
      // both formatters would render identically.
      session_id: 'dsh:0.1.6', model_ms: 1500, tool_wait_ms: 14_190_100, tool_waits: 1281,
      avg_ttft_ms: 300, output_tokens: 500, speed_tps: 42.5,
      requests: 7, cache_hit_pct: 62,
      // A non-today timestamp, so the 上次运行 row is exercised on its
      // date-bearing branch rather than only the bare-clock one.
      last_at: '2026-10-26 14:32:10',
      // This conversation delegated to two subagents; its figures above already
      // include them.
      subagent_sessions: 2, is_subagent: false,
    }, {
      // A session with no recorded children: the header must show the bare id
      // and NOT "含 0 个子智能体", because zero means "none recorded" rather
      // than "none ran".
      session_id: 'claude:aa11bb22', model_ms: 900, tool_wait_ms: null, tool_waits: 0,
      avg_ttft_ms: 250, output_tokens: 300, speed_tps: 33.3,
      requests: 4, cache_hit_pct: 50,
      // No last_at: an unmeasured row must render "—", never a fabricated time.
      last_at: null,
      subagent_sessions: 0, is_subagent: false,
    }],
  },
  get_settings: {
    provider_id: 'workbuddy-cn', custom_url: '', port: 3456, bind: '127.0.0.1',
    reasoning_model: '', completion_model: '', model_map: '', sanitize_terms: '',
    launch_at_login: false, force_stream: null, force_stream_preset: true,
  },
  get_providers: { providers: [{ id: 'workbuddy-cn', name: 'WorkBuddy CN' }, { id: 'openai', name: 'OpenAI' }] },
  get_claude_config: { env: { ANTHROPIC_MODEL: 'deepseek-chat' } },
  get_codex_config: { supported: true, catalog_exists: false, catalog_path: '/tmp/cat.json', config_path: '/tmp/cfg.toml' },
  get_dsh_config: {
    supported: true, profile: 'web',
    settings_path: '/tmp/.dsh/profiles/web/cordis.patch.yml', settings_exists: true,
    provider_exists: true, model_count: 4, base_url: 'http://127.0.0.1:3457/v1',
    credential_present: true, session_header: true,
  },
  // 会话 view. One row, one transcript. The timeline's text deliberately
  // contains markup: the assertion below proves it renders as text (the
  // transcript is model output, and this is the UI's largest injection surface).
  claude_sessions_list: {
    available: true,
    reason: null,
    root: '/Users/test/.claude/projects',
    sources: [
      { kind: 'claude_local', label: 'Claude Code', available: true, note: null },
      { kind: 'dsh_local', label: 'DSH', available: false, note: 'DSH 会话是 zstd 压缩的本地存档，暂不支持浏览' },
    ],
    stats_available: true,
    gateway_error: null,
    total: 1,
    offset: 0,
    limit: 50,
    // Flattened exactly like `SessionRow` (serde flatten): summary fields sit
    // at the top level, beside `gateway`.
    sessions: [{
      id: 'b9c3800a-311e-4c89-9a38-c533d8969b5d',
      encoded_project: '-Users-test-project', cwd: '/Users/test/project',
      title: '会话页测试', first_seen_ms: 1794000000000, last_seen_ms: 1794000100000,
      file_mtime_ms: 1794000100000, size_bytes: 2048, git_branch: 'main', version: '2.1.277',
      turns: 1, assistant_turns: 2, tool_calls: 1, subagents: 0,
      last_model: 'deepseek-v4.1-flash', counts_partial: false,
      gateway: { session_id: 'claude:b9c3800a-311e-4c89-9a38-c533d8969b5d',
                 requests: 3, input_tokens: 1200, output_tokens: 340,
                 cache_read_tokens: 900, cache_write_tokens: 10,
                 first_at: '2026-11-06 10:00:00', last_at: '2026-11-06 10:05:00',
                 models: ['deepseek-v4.1-flash'] },
      gateway_error: null,
    }],
  },
  claude_sessions_detail: {
    summary: { id: 'b9c3800a-311e-4c89-9a38-c533d8969b5d', encoded_project: '-Users-test-project',
               cwd: '/Users/test/project', title: '会话页测试', first_seen_ms: 1794000000000,
               last_seen_ms: 1794000100000, file_mtime_ms: 1794000100000, size_bytes: 2048,
               git_branch: 'main', version: '2.1.277', turns: 1, assistant_turns: 1, tool_calls: 1,
               subagents: 0, last_model: 'deepseek-v4.1-flash', counts_partial: false },
    cost_state: null,
    models: [{ model: 'deepseek-v4.1-flash', turns: 1,
               tokens: { input: 10, output: 20, cache_read: 0, cache_write: 0, thinking: 0 } }],
    tokens: { input: 10, output: 20, cache_read: 0, cache_write: 0, thinking: 0 },
    tokens_incl_subagents: { input: 10, output: 20, cache_read: 0, cache_write: 0, thinking: 0 },
    timeline: [
      { uuid: 'u1', parent_uuid: null, ts_ms: 1794000000000, role: 'user', kind: 'text',
        sidechain: false, text: '看下 <script>window.__XSS__=1</script>', text_truncated: false,
        tool_name: null, tool_input: null, tool_use_id: null, tool_result_is_error: null,
        model: null, tokens: null },
      { uuid: 'a1', parent_uuid: null, ts_ms: 1794000005000, role: 'assistant', kind: 'tool_use',
        sidechain: false, text: '', text_truncated: false, tool_name: 'Bash',
        tool_input: '{"command":"ls"}', tool_use_id: 'toolu_1', tool_result_is_error: null,
        model: 'deepseek-v4.1-flash', tokens: { input: 10, output: 20, cache_read: 0, cache_write: 0, thinking: 0 } },
    ],
    timeline_offset: 0, timeline_total: 2, has_more: false,
    ignored: { user_noise: 0, duplicate_uuid: 0, other: 0, malformed: 0 },
    subagents: [], agent_id: null,
    gateway: { session_id: 'claude:b9c3800a-311e-4c89-9a38-c533d8969b5d',
               requests: 3, input_tokens: 1200, output_tokens: 340,
               cache_read_tokens: 900, cache_write_tokens: 10,
               first_at: '2026-11-06 10:00:00', last_at: '2026-11-06 10:05:00',
               models: ['deepseek-v4.1-flash'] },
    gateway_error: null,
  },
  get_request_logs: { items: [], total: 0, models: [], clients: [], sessions: [] },
  // Opening a session in the machine's browser is a desktop-only command: the
  // backend builds the URL (it owns the console port) and shells out to the OS.
  // The mock echoes the shape the UI reads — `url` is what actually got opened,
  // so a scenario can prove the front-end asked by id and let Rust do the rest.
  open_session_in_browser: {
    ok: true,
    url: 'http://127.0.0.1:3457/admin/#sessions/b9c3800a-311e-4c89-9a38-c533d8969b5d',
    opened: true,
  },
  get_logs: { entries: [] },
  wb_credentials_list: {
    credentials: [
      { id: 'cred-1', label: '测试账号一', nickname: '甲', state: 'ok', enabled: true, points: 1234.5, sticky_sessions: 2, checked_in_today: true, masked_token: 'sk-***1', has_refresh_token: true, needs_relogin: false },
      { id: 'cred-2', label: '测试账号二', nickname: '乙', state: 'cooldown', enabled: true, points: null, sticky_sessions: 0, checked_in_today: false, masked_token: 'sk-***2', has_refresh_token: true, needs_relogin: false },
      { id: 'cred-3', label: '需重登账号', nickname: '丙', state: 'expired', enabled: true, points: null, sticky_sessions: 0, checked_in_today: false, masked_token: 'sk-***3', has_refresh_token: false, needs_relogin: true },
      // Has a refresh token, but the endpoint rejected it: the case a static
      // "no refresh token" check cannot see, and the reason the badge reads the
      // durable verdict instead.
      { id: 'cred-4', label: '令牌被拒账号', nickname: '丁', state: 'ok', enabled: true, points: 5, sticky_sessions: 0, checked_in_today: false, masked_token: 'sk-***4', has_refresh_token: true, needs_relogin: true, relogin_required_since_ms: 1790000000000, last_error: '刷新端点返回 401: token revoked' },
    ],
    default_identity_id: 'cred-1',
    // Interleaved priority order: keys participate in the same list.
    identity_order: ['cred-1', 'cred-2', 'cred-3', 'cred-4', 'key-1'],
  },
  wb_relogin: { id: 'cred-3', label: '需重登账号', enabled: true, masked_token: 'sk-***3' },
  // Drag-to-reorder the failover priority. The mock echoes what it was told to
  // store, so a test can assert the submitted order rather than the old one.
  wb_reorder_identities: { ok: true, default_moved: false },
  wb_refresh_points_single: {
    result: { id: 'cred-1', label: '测试账号一', points: 1234.5 },
    current_identity: { id: 'cred-1', label: '测试账号一', points: 1234.5 },
  },
  api_keys_list: {
    keys: [{ id: 'key-1', label: '备用密钥', enabled: true, points: 88, masked: 'sk-***k' }],
    default_identity_id: 'cred-1',
  },
  wb_preferences: { daily_checkin_enabled: true, daily_checkin_time: '08:30', last_checkin_run_date: '2026-01-01', growth_enabled: false },
  // Growth automation: the mock reports what a run would produce so the button's
  // message can be asserted. `total_claimed`/`total_credits` are what the UI reads.
  wb_growth_run: { total_accounts: 1, total_claimed: 2, total_credits: 200, accounts: [] },
  // The mock below fires `growth-progress` events the way the real command does,
  // so the smoke test can assert the progress box actually fills in.
  __growth_progress_events: [
    { phase: 'account_start', index: 1, total: 2, credential_id: 'cred-1', label: '测试账号一' },
    { phase: 'task_claimed', credential_id: 'cred-1', code: 'chat_5', credit: 100 },
    { phase: 'account_done', credential_id: 'cred-1', label: '测试账号一', claimed: 1, credits: 100 },
    { phase: 'finished', accounts: 1, claimed: 1, credits: 100 },
  ],
  wb_growth_run_single: { credential_id: 'cred-1', label: '测试账号一', claimed: [], credits_earned: 0, skipped: [], stalled: [], errors: [] },
  wb_growth_preview: { tasks: [], energy: 13, earnable: 800 },
  fetch_models: { models: [{ id: 'deepseek-chat', name: 'DeepSeek Chat' }] },
  test_upstream: { ok: true, detail: '上游流式响应正常' },
  wb_oauth_start: { state: 'abc', auth_url: 'https://example.test/auth', qr_svg: '<svg id="qrsvg"></svg>' },
  wb_oauth_poll: { status: 'success', credential: { label: '新扫码账号' } },
  pool_export: {
    ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-20260101.json', encrypted: true,
    counts: { credentials: 2, api_keys: 1 }, empty: false, warnings: [],
  },
  pool_import_preview: {
    mode: 'merge', encrypted: false,
    credentials_total: 2, credentials_added: 1, credentials_updated: 1, credentials_unchanged: 0,
    credentials_removed: 0,
    keys_total: 1, keys_added: 1, keys_updated: 0, keys_unchanged: 0, keys_removed: 0,
    default_identity_applied: 'cred-1', default_identity_dropped: false,
    checkin_enabled: true, checkin_time: '08:30',
    warnings: ['账号 wb-000000 的 id 与 token 不一致，已按 token 重新计算为 cred-1'],
  },
  pool_import: {
    mode: 'merge', encrypted: false,
    credentials_total: 2, credentials_added: 1, credentials_updated: 1, credentials_unchanged: 0,
    credentials_removed: 0,
    keys_total: 1, keys_added: 1, keys_updated: 0, keys_unchanged: 0, keys_removed: 0,
    default_identity_applied: 'cred-1', default_identity_dropped: false,
    checkin_enabled: true, checkin_time: '08:30', warnings: [],
  },
};

/** What the UI must produce. Values captured from the pre-split build. */
const EXPECTED = {
  counts: { credentialRows: 5, providerOptions: 3, speedRows: 9 },
  // 8 metric rows + the 会话 header row.
  speedMetrics: {
    '请求次数': '7',
    '缓存命中': '62%',
    '模型用时': '1.5秒',
    '工具调用用时': '236分 30秒（1281 次）',
    '首 token 平均（TTFT）': '300毫秒',
    '输出 tokens': '500',
    // The most recent turn falls on an earlier day, so this exercises the
    // date-bearing branch of `lastRunLabel` instead of a bare clock.
    '上次运行': '10-26 14:32',
    '输出速度（TPS）': '42.5 tok/s',
  },
  // Session columns, including the subagent suffix. The first conversation
  // recorded children, so its header carries the count; the second recorded
  // none and must NOT say "含 0 个子智能体" — zero means "none recorded", and
  // every row written before lineage reporting says exactly that.
  speedHeaderLabels: ['dsh:0.1.6含 2 个子智能体', 'claude:aa11bb22'],
  // The overview statistics row must show the values served by /api/stats/today,
  // not the IPC mock's (1234 / 2.50M / 42%) — that difference is what proves the
  // HTTP leg is the one answering. formatNumber abbreviates 4321 -> "4.3K".
  statsOverHttp: {
    requests: '4.3K', tokens: '7.65M', cachePct: '66%', failed: '11', overrides: '9',
  },
  text: {
    metricProvider: 'workbuddy-cn',
    metricPoints: '1234.50',
    metricPointsAccount: '测试账号一',
    metricSpeed: '42.5 tok/s',
    portValue: '3456',
    // The fixture has 每日打卡 on and growth off, so the summary must NAME the
    // running job — "已开启 1 项" would not tell a reader which background job
    // touches their accounts.
    automationSummary: '已开启：每日打卡 08:30',
    // The trigger must be present in the page itself; without it the switches
    // would be unreachable once moved into the dialog.
    automationButton: true,
    forceStreamHint: '跟随服务商预设：开启',
    statusText: '运行中 · 端口 3456',
    appVersion: 'v9.9.9',
  },
  rowLabels: ['测试账号一', '测试账号二', '需重登账号', '令牌被拒账号', '备用密钥'],
  rowStates: ['正常', '冷却中', '已过期', '正常', '已启用'],
  defaultRowFlags: [true, false, false, false, false],
  selectValue: 'cred-1',
  // The DSH group renders its live state on load, from get_dsh_config.
  dshButtonPresent: true,
  dshStatus: '当前 profile web · 4 个模型 · http://127.0.0.1:3457/v1 · 凭据已就绪 · 会话识别已开启',
  // 缓存命中率 shows a value only — no expand affordance, no detail section.
  cacheCard: { present: true, clickable: false, expandable: false, hasCaret: false, detailExists: false },
  // Account-pool import/export (2.0.0). The dialogs are modal-only, so these
  // are read after driving them from the toolbar.
  exportResult: '已导出 2 个账号、1 个密钥（口令加密） /tmp/proxy-rs-pool-20260101.json',
  // The export dialog names the directory it reads and the counts it will write,
  // so "empty export" is diagnosable from the dialog itself (mock get_status
  // plus the two pool list commands: 2 credentials + 1 key).
  exportSourceShown: '/Users/test/.proxy-rs',
  exportCountsShown: '账号 4 个、密钥 1 个',
  // The badge's tooltip must name the reason, since the row is where the user
  // learns about a revoked credential now that the live log is not the only place.
  rejectedTooltipContains: ['拒绝', 'token revoked'],
  // Dismissing the native save panel is a no-op, not a failure: the message says
  // so, carries no error tone, and the 导出 button is usable again.
  exportCancelled: {
    text: '已取消导出（未写入任何文件）',
    isError: false,
    retryable: true,
  },
  // An export that carries no identity is a failure the user must notice, even
  // though the file itself was written successfully. Asserted as substrings: the
  // message spans inline markup, so exact equality would only be testing
  // whitespace.
  exportEmpty: {
    isError: true,
    contains: [
      '不包含任何账号或密钥',
      '本机身份池为空',
      '/tmp/proxy-rs-pool-empty.json',
      '请在正式实例中导出',
    ],
  },
  importPreview: {
    credentialsAdded: '1',
    credentialsUpdated: '1',
    warningText: '账号 wb-000000 的 id 与 token 不一致，已按 token 重新计算为 cred-1',
    identity: 'cred-1',
  },
};

/**
 * The dashboard endpoints, served from the harness origin.
 *
 * Values are distinct from the IPC mock on purpose (`get_stats` reports 1234
 * requests, this reports 4321): the boot assertion below then proves which
 * transport answered, rather than passing either way.
 */
const API = {
  '/api/stats/today': {
    date: '2026-10-20',
    requests_total: 4321, requests_success: 4310, requests_failed: 11,
    tokens_total: 7654321, tokens_input: 1000, tokens_cache_read: 2000,
    tokens_cache_write: 300, tokens_output: 4000,
    cache_hit_pct: 66, overrides_total: 9,
  },
  '/api/request-logs': {
    items: [{
      id: 7, created_at: '2026-10-20 11:02:03', model: 'deepseek-chat',
      client_model: 'deepseek-chat', route: '/v1/messages',
      input_tokens: 120, output_tokens: 60, cache_read_tokens: 30, cache_write_tokens: 0,
      duration_ms: 1500, ttft_ms: 200, model_ms: 900, ended_with_tool_call: false,
      streamed: true, status: 200, error: null,
      session_id: 'claude:abc', client: 'claude',
      override_key: '', override_model: '', override_reason: '',
    }],
    total: 1, models: ['deepseek-chat'], client_models: ['deepseek-chat'],
    clients: ['claude'], sessions: [{ session_id: 'claude:abc', client: 'claude' }],
  },
};

export { MOCK, EXPECTED, API };
