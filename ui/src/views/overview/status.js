/**
 * The service-state readers: header, identity cards and the endpoint reference.
 *
 * Split out of `overview.js`. This is the function every other view calls after
 * a mutation (`refreshStatus`), so it is deliberately small and free of markup —
 * it only writes into the nodes `cards.js` created.
 */

import { $, setText, setClass } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { appState } from '../../core/state.js';
import { formatPoints } from '../../lib/format.js';

/** Read the current service state and repaint everything that depends on it. */
export async function refreshStatus() {
  let status;
  try {
    status = await invoke('get_status');
  } catch (err) {
    console.error('refreshStatus error:', err);
    return;
  }
  if (!status) return;

  appState.running = status.running || status.service_running;
  appState.port = status.port || status.configured_port || 3456;

  // Header
  setClass($('#status-dot'), 'running', appState.running);
  setClass($('#status-dot'), 'stopped', !appState.running);
  setText('status-text', appState.running ? `运行中 · 端口 ${appState.port}` : '已停止');

  const toggle = $('#btn-toggle-service');
  if (toggle) {
    toggle.textContent = appState.running ? '停止服务' : '启动服务';
    toggle.className = 'btn btn-small' + (appState.running ? '' : ' btn-primary');
  }

  // The identity in force right now: 配置厂商 / 剩余积分 (+账号) / 输出速度.
  const identity = status.current_identity || {};
  appState.currentIdentity = identity;
  // 配置厂商: the backend reports the provider id, as before. An identity
  // label describes *who* is serving, this describes *where* — and only the
  // latter is known unconditionally (it survives a stopped pool).
  setText('metric-provider', status.provider || '-');
  setText('metric-points', formatPoints(identity.points));
  // The account name is shown as the 剩余积分 card's sub-line: the balance
  // belongs to that account, so reading them together is what the number means.
  setText('metric-points-account', identity.label || '—');

  // Endpoint reference.
  //
  // The base differs by deployment and the difference matters: in the desktop app
  // the client runs on the same machine, so `127.0.0.1:port` is what to paste.
  // In the server console the page is open in a browser somewhere else, and a
  // client on that machine would have to reach the gateway's public address — so
  // `127.0.0.1` there is not merely useless, it is wrong in a way that costs the
  // user a debugging session.
  const endpointBase = window.__PROXY_RS_SERVER__
    ? `${window.location.protocol}//${window.location.host}`
    : `http://127.0.0.1:${appState.port}`;
  setText('endpoint-messages', `${endpointBase}/v1/messages`);
  setText('endpoint-responses', `${endpointBase}/v1/responses`);
  setText('endpoint-models', `${endpointBase}/v1/models`);
  setText('overview-upstream', status.upstream_url || '-');
  if (status.version) setText('app-version', `v${status.version}`);
  if (status.log_path) setText('overview-log-path', status.log_path);

  // Reflect the user's persisted intent; whether the job is actually live is
  // reported separately by `launch_at_login_stale`.
  setText('overview-lal', status.launch_at_login ? '已开启' : '未开启');
  if (status.launch_at_login_stale) {
    // Setting says yes but launchd has no job: the login item is dead and the
    // app will not start at next login until it is re-armed.
    setText('overview-lal', '已开启（未生效）');
  }
}
