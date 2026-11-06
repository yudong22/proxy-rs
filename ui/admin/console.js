/**
 * Proxy RS server-mode admin console.
 *
 * Plain ES module, no framework and no bundler — the same constraint the desktop
 * UI works under, and it matters more here: this file ships inside the gateway
 * binary and is served to whatever browser the operator has, so there is no
 * build step to fall back on.
 *
 * ## Where the data comes from
 *
 * Every read and write goes to `/admin/api/*` on the same origin, authenticated
 * by the session cookie the login form obtains. There is no IPC channel and no
 * privileged back door: the console has exactly the authority the HTTP API gives
 * it, which is why the API's own tests are the security tests.
 *
 * ## What this front-end deliberately cannot do
 *
 * Start or stop the gateway, toggle autostart, open a log directory, or scan a
 * QR code for login. Those are desktop capabilities with no server meaning; see
 * `docs/admin-api.md` §"桌面端专有".
 */

// ── HTTP ───────────────────────────────────────────────────────────────────

/**
 * Absolute API base.
 *
 * Absolute, not relative: the console page is served at `/admin/api/`, so a
 * relative base would resolve `session` to `/admin/api/admin/api/session`.
 * Reverse proxies that mount the gateway under a sub-path can rewrite this one
 * line (or serve it with a `<base>`), which is why it is a single constant.
 */
const API = '/admin/api';

/**
 * Call the admin API.
 *
 * `credentials: 'same-origin'` is what sends the session cookie. `no-store` on
 * every read keeps a stale settings form from being resurrected by the browser's
 * back/forward cache after a save.
 */
async function apiFetch(path, options = {}) {
  const response = await fetch(`${API}/${path}`, {
    credentials: 'same-origin',
    cache: 'no-store',
    headers: {
      'content-type': 'application/json',
      ...(options.headers || {}),
    },
    ...options,
  });

  if (response.status === 401) {
    // The session expired (12h idle, or the gateway restarted). Drop straight to
    // the login form rather than surfacing an error the operator cannot act on.
    showLogin('会话已过期，请重新登录');
    throw new Error('unauthorized');
  }

  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = { raw: text };
    }
  }

  if (!response.ok) {
    const message =
      body?.error?.message || body?.raw || `HTTP ${response.status}`;
    throw new Error(message);
  }
  return body;
}

const getJson = (path) => apiFetch(path);
const postJson = (path, body) =>
  apiFetch(path, { method: 'POST', body: JSON.stringify(body ?? {}) });
const patchJson = (path, body) =>
  apiFetch(path, { method: 'PATCH', body: JSON.stringify(body ?? {}) });
const deleteJson = (path) => apiFetch(path, { method: 'DELETE' });

// ── DOM helpers ────────────────────────────────────────────────────────────

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

/**
 * Build an element.
 *
 * Text always goes through `textContent`, never `innerHTML`. The values rendered
 * here come from a credential store and from upstream APIs; a label containing
 * `<img onerror=…>` must be displayed, not executed. The console is authenticated
 * and session-cookied, so an XSS here would hand over the whole gateway.
 */
function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key === 'html') node.innerHTML = value; // caller-controlled only
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else if (value !== null && value !== undefined) node.setAttribute(key, value);
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined || child === false) continue;
    node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

let toastTimer = null;
function toast(message) {
  const node = $('#toast');
  node.textContent = message;
  node.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => node.classList.remove('show'), 1800);
}

/** Run an async action with a toast on failure, so no click ever fails silently. */
async function guarded(label, fn) {
  try {
    return await fn();
  } catch (err) {
    if (err.message === 'unauthorized') return null;
    toast(`${label}失败：${err.message}`);
    console.error(label, err);
    return null;
  }
}

// ── Modal ──────────────────────────────────────────────────────────────────

let modalOnClose = null;

/**
 * Open a modal. `render(close)` builds the body; the caller owns the actions.
 */
function openModal(render, onClose = null) {
  const back = $('#modal-back');
  const host = $('#modal-body');
  host.replaceChildren();
  modalOnClose = onClose;
  const close = () => {
    back.classList.remove('show');
    const cb = modalOnClose;
    modalOnClose = null;
    if (cb) cb();
  };
  host.appendChild(render(close));
  back.classList.add('show');
  return close;
}

function closeModal() {
  $('#modal-back').classList.remove('show');
  const cb = modalOnClose;
  modalOnClose = null;
  if (cb) cb();
}

$('#modal-back').addEventListener('click', (event) => {
  if (event.target.id === 'modal-back') closeModal();
});

/** A modal with a title, body nodes and a confirm/cancel pair. */
function confirmModal({ title, body, confirmText = '确定', danger = false, onConfirm }) {
  openModal((close) =>
    el('div', {}, [
      el('h3', { text: title }),
      ...[].concat(body).map((b) => (typeof b === 'string' ? el('p', { text: b }) : b)),
      el('div', { class: 'actions' }, [
        el('button', { class: 'act', text: '取消', onClick: close }),
        el('button', {
          class: `act primary${danger ? ' danger' : ''}`,
          text: confirmText,
          onClick: async () => {
            const btn = event?.target;
            try {
              await onConfirm();
            } catch (err) {
              if (err.message !== 'unauthorized') toast(`操作失败：${err.message}`);
              return;
            }
            close();
          },
        }),
      ]),
    ])
  );
}

// ── Login ──────────────────────────────────────────────────────────────────

function showLogin(message) {
  $('#shell').classList.remove('show');
  $('#login-view').style.display = 'grid';
  const err = $('#login-err');
  if (message) {
    err.textContent = message;
    err.classList.add('show');
  } else {
    err.classList.remove('show');
  }
}

function showShell() {
  $('#login-view').style.display = 'none';
  $('#shell').classList.add('show');
}

$('#login-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = $('#login-btn');
  const err = $('#login-err');
  button.disabled = true;
  err.classList.remove('show');
  try {
    await postJson('login', { token: $('#token').value });
    $('#token').value = '';
    showShell();
    await refreshAll();
  } catch (e) {
    err.textContent = e.message === 'unauthorized' ? '令牌错误' : e.message;
    err.classList.add('show');
  } finally {
    button.disabled = false;
  }
});

$('#logout-btn').addEventListener('click', async () => {
  await guarded('退出登录', async () => {
    await postJson('logout', {});
  });
  showLogin();
});

// ── Tabs ───────────────────────────────────────────────────────────────────

const tabLoaders = {
  overview: loadOverview,
  identities: loadIdentities,
  access: loadAccessKeys,
  settings: loadSettings,
  logs: loadLogs,
  pool: () => {},
};

let activeTab = 'overview';

function switchTab(name) {
  activeTab = name;
  $$('#tabs button').forEach((b) =>
    b.classList.toggle('active', b.dataset.tab === name)
  );
  $$('main section').forEach((s) =>
    s.classList.toggle('active', s.id === `tab-${name}`)
  );
  tabLoaders[name]?.();
}

$('#tabs').addEventListener('click', (event) => {
  const button = event.target.closest('button[data-tab]');
  if (button) switchTab(button.dataset.tab);
});

// ── Overview ───────────────────────────────────────────────────────────────

function statCard(label, value) {
  return el('div', { class: 'stat' }, [
    el('div', { class: 'k', text: label }),
    el('div', { class: 'v', text: value }),
  ]);
}

async function loadOverview() {
  const [stats, status] = await Promise.all([
    guarded('读取统计', () => getJson('stats')),
    guarded('读取状态', () => getJson('status')),
  ]);

  if (stats) {
    const pct = (n) => (n === null || n === undefined ? '—' : `${n}%`);
    $('#stat-grid').replaceChildren(
      statCard('请求总数', stats.requests_total ?? 0),
      statCard('成功 / 失败', `${stats.requests_success ?? 0} / ${stats.requests_failed ?? 0}`),
      statCard('输入 tokens', stats.tokens_input ?? 0),
      statCard('输出 tokens', stats.tokens_output ?? 0),
      statCard('缓存命中率', pct(stats.cache_hit_pct))
    );
  }

  if (status) {
    $('#hdr-meta').textContent = `v${status.version} · :${status.port} · ${status.provider}`;
    const rows = [
      ['版本', status.version],
      ['监听', `${status.bind}:${status.port}`],
      ['数据目录', status.data_dir],
      ['上游', status.upstream],
      ['运行时长', formatUptime(status.uptime_secs)],
      ['进程托管', status.managed_by === 'systemd' ? 'systemd（无启停按钮）' : status.managed_by],
      ['入站鉴权', status.client_auth_enabled ? '已开启' : '未开启'],
    ];
    $('#status-card').replaceChildren(
      el('table', {}, rows.map(([k, v]) =>
        el('tr', {}, [el('td', { class: 'muted', text: k }), el('td', {}, el('code', { text: String(v ?? '—') }))])
      ))
    );
  }

  await loadScheduler();
}

async function loadScheduler() {
  const data = await guarded('读取定时任务', () => getJson('scheduler'));
  if (!data) return;
  const rows = (data.loops || []).map((loop) => {
    const last = loop.last_run;
    const outcome = last?.outcome || '未运行';
    const cls = outcome === 'ok' ? 'ok' : outcome === 'failed' ? 'err' : 'dim';
    return el('tr', {}, [
      el('td', { text: loop.title }),
      el('td', {}, el('span', { class: `badge ${loop.enabled ? 'ok' : 'dim'}`, text: loop.enabled ? '已启用' : '已停用' })),
      el('td', {}, el('span', { class: `badge ${cls}`, text: outcome })),
      el('td', { class: 'muted', text: last?.at || '—' }),
      el('td', { class: 'muted', text: last?.summary || '—' }),
    ]);
  });
  $('#scheduler-card').replaceChildren(
    data.loops?.length
      ? el('table', {}, [
          el('thead', {}, el('tr', {}, [
            el('th', { text: '任务' }), el('th', { text: '开关' }),
            el('th', { text: '上次结果' }), el('th', { text: '时间' }), el('th', { text: '摘要' }),
          ])),
          el('tbody', {}, rows),
        ])
      : el('div', { class: 'empty', text: '没有定时任务' })
  );
}

function formatUptime(secs) {
  if (secs === null || secs === undefined) return '—';
  const day = Math.floor(secs / 86400);
  const hour = Math.floor((secs % 86400) / 3600);
  const min = Math.floor((secs % 3600) / 60);
  if (day) return `${day} 天 ${hour} 小时`;
  if (hour) return `${hour} 小时 ${min} 分`;
  return `${min} 分`;
}

// ── Identities ─────────────────────────────────────────────────────────────

async function loadIdentities() {
  const [data, prefs] = await Promise.all([
    guarded('读取身份池', () => getJson('identities')),
    guarded('读取偏好', () => getJson('preferences')),
  ]);
  if (!data) return;

  const credentialRows = (data.credentials || []).map((c) => {
    const stateCls =
      c.state === 'ok' ? 'ok' : c.state === 'disabled' ? 'dim' : 'warn';
    const stateText =
      c.state === 'ok' ? '可用'
      : c.state === 'disabled' ? '已停用'
      : c.state === 'cooldown' ? '冷却中'
      : c.state === 'expired' ? '已过期'
      : c.state;
    return el('tr', {}, [
      el('td', {}, [
        el('div', { text: c.label || c.nickname || c.id }),
        el('div', { class: 'muted', style: 'font-size:12px', text: c.id }),
        c.is_default ? el('span', { class: 'badge ok', text: '默认' }) : null,
      ]),
      el('td', {}, el('span', { class: 'badge dim', text: '登录态' })),
      el('td', {}, el('span', { class: `badge ${stateCls}`, text: stateText })),
      el('td', { class: 'muted', text: c.checked_in_today ? '已打卡' : '未打卡' }),
      el('td', { class: 'muted', text: c.points === null || c.points === undefined ? '—' : Number(c.points).toFixed(1) }),
      el('td', {}, [
        el('button', { class: 'act', text: c.enabled ? '停用' : '启用',
          onClick: () => guarded('切换', async () => {
            await patchJson(`credentials/${encodeURIComponent(c.id)}`, { enabled: !c.enabled });
            await loadIdentities();
          }) }),
        ' ',
        !c.is_default ? el('button', { class: 'act', text: '设默认',
          onClick: () => guarded('设默认', async () => {
            await postJson('identities/default', { id: c.id });
            toast('已设为默认身份');
            await loadIdentities();
          }) }) : null,
        ' ',
        el('button', { class: 'act danger', text: '删除',
          onClick: () => confirmModal({
            title: '删除账号',
            body: `确定删除「${c.label || c.id}」？该账号的登录态将被移除。`,
            danger: true,
            onConfirm: async () => {
              await deleteJson(`credentials/${encodeURIComponent(c.id)}`);
              toast('已删除');
              await loadIdentities();
            },
          }) }),
      ]),
    ]);
  });

  const keyRows = (data.keys || []).map((k) =>
    el('tr', {}, [
      el('td', {}, [
        el('div', { text: k.label || k.id }),
        el('div', { class: 'muted', style: 'font-size:12px', text: k.masked || '' }),
        k.is_default ? el('span', { class: 'badge ok', text: '默认' }) : null,
      ]),
      el('td', {}, el('span', { class: 'badge dim', text: 'API Key' })),
      el('td', {}, el('span', { class: `badge ${k.enabled ? 'ok' : 'dim'}`, text: k.enabled ? '可用' : '已停用' })),
      el('td', { class: 'muted', text: '—' }),
      el('td', { class: 'muted', text: k.points === null || k.points === undefined ? '—' : Number(k.points).toFixed(1) }),
      el('td', {}, [
        el('button', { class: 'act', text: k.enabled ? '停用' : '启用',
          onClick: () => guarded('切换', async () => {
            await patchJson(`keys/${encodeURIComponent(k.id)}`, { enabled: !k.enabled });
            await loadIdentities();
          }) }),
        ' ',
        el('button', { class: 'act danger', text: '删除',
          onClick: () => confirmModal({
            title: '删除 API Key',
            body: `确定删除「${k.label || k.id}」？`,
            danger: true,
            onConfirm: async () => {
              await deleteJson(`keys/${encodeURIComponent(k.id)}`);
              toast('已删除');
              await loadIdentities();
            },
          }) }),
      ]),
    ])
  );

  const allRows = [...credentialRows, ...keyRows];
  $('#identity-list').replaceChildren(
    allRows.length
      ? el('table', {}, [
          el('thead', {}, el('tr', {}, [
            el('th', { text: '身份' }), el('th', { text: '类型' }), el('th', { text: '状态' }),
            el('th', { text: '今日' }), el('th', { text: '积分' }), el('th', { text: '操作' }),
          ])),
          el('tbody', {}, allRows),
        ])
      : el('div', { class: 'empty', text: '身份池为空。添加登录态或 API Key 后即可转发请求。' })
  );

  renderPrefs(prefs);
}

function renderPrefs(prefs) {
  if (!prefs) return;
  const timeInput = el('input', { type: 'time', value: prefs.daily_checkin_time || '09:00' });
  const enabledInput = el('input', { type: 'checkbox' });
  enabledInput.checked = Boolean(prefs.daily_checkin_enabled);
  const growthInput = el('input', { type: 'checkbox' });
  growthInput.checked = Boolean(prefs.growth_enabled);

  $('#prefs-card').replaceChildren(
    el('div', { class: 'row' }, [
      el('div', { class: 'field' }, [
        el('label', {}, [enabledInput, ' 每日自动打卡']),
      ]),
      el('div', { class: 'field' }, [
        el('label', { text: '打卡时间' }),
        timeInput,
      ]),
      el('div', { class: 'field' }, [
        el('label', {}, [growthInput, ' 成长任务自动化']),
      ]),
    ]),
    el('button', { class: 'act primary', text: '保存', onClick: () => guarded('保存', async () => {
      await postJson('preferences/checkin', {
        enabled: enabledInput.checked,
        time: timeInput.value || '09:00',
        growth: growthInput.checked,
      });
      toast('已保存');
    }) })
  );
}

$('#add-credential').addEventListener('click', () => {
  const textarea = el('textarea', { placeholder: '粘贴 WorkBuddy 登录态 JSON' });
  const label = el('input', { placeholder: '备注名（可选）' });
  openModal((close) =>
    el('div', {}, [
      el('h3', { text: '添加登录态' }),
      el('div', { class: 'note' }, '从桌面版导出登录态，或直接粘贴会话 JSON。'),
      el('div', { class: 'field' }, [el('label', { text: '登录态' }), textarea]),
      el('div', { class: 'field' }, [el('label', { text: '备注名' }), label]),
      el('div', { class: 'actions' }, [
        el('button', { class: 'act', text: '取消', onClick: close }),
        el('button', { class: 'act primary', text: '添加', onClick: async () => {
          await guarded('添加', async () => {
            await postJson('credentials', { login_state: textarea.value, label: label.value });
            toast('已添加');
            close();
            await loadIdentities();
          });
        } }),
      ]),
    ])
  );
});

$('#add-key').addEventListener('click', () => {
  const key = el('input', { placeholder: 'sk-...' });
  const label = el('input', { placeholder: '备注名（可选）' });
  openModal((close) =>
    el('div', {}, [
      el('h3', { text: '添加 API Key' }),
      el('div', { class: 'field' }, [el('label', { text: 'API Key' }), key]),
      el('div', { class: 'field' }, [el('label', { text: '备注名' }), label]),
      el('div', { class: 'actions' }, [
        el('button', { class: 'act', text: '取消', onClick: close }),
        el('button', { class: 'act primary', text: '添加', onClick: async () => {
          await guarded('添加', async () => {
            await postJson('keys', { key: key.value, label: label.value });
            toast('已添加');
            close();
            await loadIdentities();
          });
        } }),
      ]),
    ])
  );
});

$('#checkin-all').addEventListener('click', () => guarded('打卡', async () => {
  toast('正在打卡…');
  const out = await postJson('checkin', {});
  toast(out.summary || '打卡完成');
  await loadIdentities();
}));

$('#refetch-points').addEventListener('click', () => guarded('刷新积分', async () => {
  toast('服务器模式下积分随后台循环刷新');
  await loadIdentities();
}));

// ── Access keys ────────────────────────────────────────────────────────────

async function loadAccessKeys() {
  const data = await guarded('读取访问密钥', () => getJson('client-accounts'));
  if (!data) return;

  const enabledNote = el('div', { class: `note ${data.enabled ? '' : 'warn'}` },
    data.enabled
      ? `入站鉴权已开启${data.env_forced ? '（由 PROXY_CLIENT_AUTH 强制）' : ''}。客户端必须携带密钥。`
      : '入站鉴权当前关闭：任何能访问网关的人都能使用上游凭据。请在设置中开启。'
  );

  const rows = (data.accounts || []).map((a) =>
    el('tr', {}, [
      el('td', {}, [
        el('div', { text: a.label }),
        a.note ? el('div', { class: 'muted', style: 'font-size:12px', text: a.note }) : null,
      ]),
      el('td', {}, el('code', { text: `${a.key_prefix}…` })),
      el('td', {}, el('span', { class: `badge ${a.enabled ? 'ok' : 'dim'}`, text: a.enabled ? '启用' : '停用' })),
      el('td', { class: 'muted', text: a.last_used_ms ? new Date(a.last_used_ms).toLocaleString() : '从未使用' }),
      el('td', {}, [
        el('button', { class: 'act', text: a.enabled ? '停用' : '启用',
          onClick: () => guarded('切换', async () => {
            await patchJson(`client-accounts/${encodeURIComponent(a.id)}`, { enabled: !a.enabled });
            await loadAccessKeys();
          }) }),
        ' ',
        el('button', { class: 'act danger', text: '删除',
          onClick: () => confirmModal({
            title: '删除访问密钥',
            body: `确定删除「${a.label}」？使用它的客户端将立即收到 401。`,
            danger: true,
            onConfirm: async () => {
              await deleteJson(`client-accounts/${encodeURIComponent(a.id)}`);
              toast('已删除');
              await loadAccessKeys();
            },
          }) }),
      ]),
    ])
  );

  $('#access-list').replaceChildren(
    enabledNote,
    rows.length
      ? el('table', {}, [
          el('thead', {}, el('tr', {}, [
            el('th', { text: '名称' }), el('th', { text: '前缀' }), el('th', { text: '状态' }),
            el('th', { text: '最近使用' }), el('th', { text: '操作' }),
          ])),
          el('tbody', {}, rows),
        ])
      : el('div', { class: 'empty', text: '还没有访问密钥。' })
  );
}

$('#add-access').addEventListener('click', () => {
  const label = el('input', { placeholder: '例如：我的笔记本' });
  const note = el('input', { placeholder: '备注（可选）' });
  openModal((close) =>
    el('div', {}, [
      el('h3', { text: '新建访问密钥' }),
      el('div', { class: 'field' }, [el('label', { text: '名称' }), label]),
      el('div', { class: 'field' }, [el('label', { text: '备注' }), note]),
      el('div', { class: 'actions' }, [
        el('button', { class: 'act', text: '取消', onClick: close }),
        el('button', { class: 'act primary', text: '创建', onClick: async () => {
          const out = await guarded('创建', () =>
            postJson('client-accounts', { label: label.value, note: note.value })
          );
          if (!out) return;
          // The plaintext key exists in this response only. Show it once, with
          // the header name, and say plainly that it cannot be recovered.
          openModal((close2) =>
            el('div', {}, [
              el('h3', { text: '访问密钥已创建' }),
              el('div', { class: 'note warn' }, '这是唯一一次显示明文。关闭后无法再次查看，只能吊销重发。'),
              el('div', { class: 'key-reveal', text: out.api_key }),
              el('div', { class: 'field' }, [
                el('label', { text: '请求头名' }),
                el('code', { text: out.header }),
              ]),
              el('div', { class: 'actions' }, [
                el('button', { class: 'act', text: '复制', onClick: async () => {
                  try {
                    await navigator.clipboard.writeText(out.api_key);
                    toast('已复制');
                  } catch {
                    toast('复制失败，请手动选择');
                  }
                } }),
                el('button', { class: 'act primary', text: '我已保存', onClick: () => {
                  close2();
                  loadAccessKeys();
                } }),
              ]),
            ])
          );
        } }),
      ]),
    ])
  );
});

// ── Settings ───────────────────────────────────────────────────────────────

async function loadSettings() {
  const data = await guarded('读取设置', () => getJson('settings'));
  if (!data) return;

  const provider = el('input', { value: data.provider_id || '' });
  const customUrl = el('input', { value: data.custom_url || '', placeholder: '留空则用厂商预设' });
  const apiKey = el('input', { type: 'password', value: '', placeholder: data.api_key_set ? '已设置（留空则不修改）' : '未设置' });
  const port = el('input', { type: 'number', value: String(data.port ?? 3456) });
  const bind = el('input', { value: data.bind || '127.0.0.1' });
  const modelMap = el('textarea', { placeholder: 'claude-sonnet-4-5=deepseek-chat' });
  modelMap.value = data.model_map || '';
  const sanitize = el('textarea', {});
  sanitize.value = data.sanitize_terms || '';
  const clientAuth = el('input', { type: 'checkbox' });
  clientAuth.checked = Boolean(data.client_auth_enabled);

  const field = (label, input, hint) =>
    el('div', { class: 'field' }, [
      el('label', { text: label }),
      input,
      hint ? el('div', { class: 'hint', text: hint }) : null,
    ]);

  $('#settings-card').replaceChildren(
    el('div', { class: 'row' }, [field('厂商 ID', provider), field('自定义上游 URL', customUrl)]),
    field('上游 API Key', apiKey, '留空表示不修改已保存的密钥'),
    el('div', { class: 'row' }, [field('端口', port, '需重启生效'), field('绑定地址', bind, '需重启生效')]),
    field('模型映射', modelMap, '每行一条 src=target'),
    field('系统提示词清洗词', sanitize, '每行一条'),
    el('div', { class: 'field' }, [el('label', {}, [clientAuth, ' 要求客户端携带访问密钥'])]),
    el('div', { style: 'display:flex;gap:10px' }, [
      el('button', { class: 'act primary', text: '保存设置', onClick: () => guarded('保存设置', async () => {
        await postJson('settings', {
          provider_id: provider.value,
          custom_url: customUrl.value,
          api_key: apiKey.value,
          port: Number(port.value) || 0,
          bind: bind.value,
          model_map: modelMap.value,
          sanitize_terms: sanitize.value,
          client_auth_enabled: clientAuth.checked,
        });
        toast('已保存');
        await loadSettings();
      }) }),
    ])
  );
}

// ── Logs ───────────────────────────────────────────────────────────────────

async function loadLogs() {
  const data = await guarded('读取日志', () => getJson('logs'));
  if (!data) return;
  const entries = data.entries || [];
  const pane = $('#log-pane');
  pane.replaceChildren();
  if (!entries.length) {
    pane.textContent = '暂无日志';
    return;
  }
  // Newest last, matching the desktop console's ordering.
  for (const entry of entries.slice(-400)) {
    const level = String(entry.level || 'INFO').toLowerCase();
    pane.appendChild(el('span', { class: 't', text: `${entry.ts} ` }));
    pane.appendChild(el('span', {
      class: level === 'error' ? 'l-error' : level === 'warn' ? 'l-warn' : '',
      text: `[${entry.level}] ${entry.message}`,
    }));
    pane.appendChild(document.createTextNode('\n'));
  }
  pane.scrollTop = pane.scrollHeight;
}

$('#logs-refresh').addEventListener('click', () => loadLogs());
$('#logs-clear').addEventListener('click', () => guarded('清空日志', async () => {
  await postJson('logs/clear', {});
  toast('已清空');
  await loadLogs();
}));

// ── Pool export / import ───────────────────────────────────────────────────

$('#do-export').addEventListener('click', () => guarded('导出', async () => {
  const password = $('#export-pass').value;
  // A download needs a real navigation, so fetch the bytes and hand them to the
  // browser as a blob rather than letting the browser navigate to the endpoint
  // directly — that way a failure renders as a toast instead of replacing the
  // console with a JSON error page.
  const response = await fetch(
    `${API}/pool/export?password=${encodeURIComponent(password)}`,
    { credentials: 'same-origin', cache: 'no-store' }
  );
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body?.error?.message || `HTTP ${response.status}`);
  }
  const blob = await response.blob();
  const url = URL.createObjectURL(blob);
  const link = el('a', { href: url, download: `proxy-rs-pool-${Date.now()}.json` });
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
  toast('已开始下载');
}));

$('#preview-import').addEventListener('click', () => guarded('预览', async () => {
  const out = await postJson('pool/import/preview', {
    text: $('#import-text').value,
    password: $('#import-pass').value,
    mode: $('#import-mode').value,
  });
  renderImportResult(out, true);
}));

$('#do-import').addEventListener('click', () => {
  confirmModal({
    title: '执行导入',
    body: '导入会覆盖同身份的条目。继续前请确认已了解预览结果。',
    confirmText: '导入',
    danger: true,
    onConfirm: async () => {
      const out = await postJson('pool/import', {
        text: $('#import-text').value,
        password: $('#import-pass').value,
        mode: $('#import-mode').value,
      });
      renderImportResult(out, false);
      toast('导入完成');
    },
  });
});

function renderImportResult(out, preview) {
  if (!out) return;
  const rows = [
    ['模式', out.mode],
    ['加密', out.encrypted ? '是' : '否'],
    ['登录态', `新增 ${out.credentials_added ?? 0} / 更新 ${out.credentials_updated ?? 0} / 未变 ${out.credentials_unchanged ?? 0}` +
      (out.credentials_removed ? ` / 移除 ${out.credentials_removed}` : '')],
    ['API Key', `新增 ${out.keys_added ?? 0} / 更新 ${out.keys_updated ?? 0} / 未变 ${out.keys_unchanged ?? 0}` +
      (out.keys_removed ? ` / 移除 ${out.keys_removed}` : '')],
    ['访问账户', `新增 ${out.client_accounts_added ?? 0} / 更新 ${out.client_accounts_updated ?? 0}` +
      (out.client_accounts_removed ? ` / 移除 ${out.client_accounts_removed}` : '')],
  ];
  $('#import-result').replaceChildren(
    el('div', { class: 'note' }, preview ? '预览（尚未写入）' : '导入结果'),
    el('table', {}, rows.map(([k, v]) =>
      el('tr', {}, [
        el('td', { class: 'muted', text: k }),
        el('td', { text: String(v ?? '—') }),
      ])
    ))
  );
}

// ── Boot ───────────────────────────────────────────────────────────────────

async function refreshAll() {
  await loadOverview();
}

async function boot() {
  try {
    // A valid cookie means we can skip the login form entirely.
    await getJson('session');
    showShell();
    await refreshAll();
  } catch {
    showLogin();
  }
}

boot();

// The desktop console is driven by the Rust side through `window.refreshStatus`;
// this one only needs the same hook for the auth-expiry path.
window.refreshStatus = refreshAll;
