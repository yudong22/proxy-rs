/**
 * QR-code OAuth login for the account pool.
 *
 * The pool module drives this flow, so the "an account landed, repaint" step
 * arrives as an injected callback rather than an import — importing the pool
 * back would close a dependency cycle (the same reason the palette takes
 * `switchTab` as an argument).
 */

import { $, html } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { openModal, closeModal } from '../../components/modal.js';
import { toast } from '../../components/toast.js';

/** Repaints the credential list; injected by credential-pool.js at init. */
let refreshPool = async () => {};

/** @param {() => Promise<void>} fn */
export function setOAuthPoolRefresh(fn) {
  refreshPool = fn;
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
export async function startQrCodeLogin() {
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
            statusEl.innerHTML = html`<span class="wb-qr-status-ok">✅ 扫码成功！账号已自动加入账号池</span>`;
          }
          toast(`WorkBuddy 账号「${poll.credential?.label || '新账号'}」已加入账号池！`, 'ok');
          await refreshPool();
          setTimeout(() => closeModal(), 1500);
        } else if (poll.status === 'failed') {
          clearOAuthPolling();
          if (statusEl) {
            statusEl.innerHTML = html`<span class="wb-qr-status-err">❌ 授权失败: ${poll.error}</span>`;
          }
        }
      } catch (err) {
        clearOAuthPolling();
        const statusEl = $('#wb-qr-poll-status');
        if (statusEl) {
          statusEl.innerHTML = html`<span class="wb-qr-status-err">轮询异常: ${err}</span>`;
        }
      }
    }, 2000);
  } catch (err) {
    const qrTarget = $('#wb-qr-target');
    if (qrTarget) {
      qrTarget.innerHTML = html`<div class="wb-qr-error">生成授权二维码失败: ${err}</div>`;
    }
  }
}
