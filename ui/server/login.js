/**
 * Login for the server-mode console.
 *
 * One job: exchange the operator's `PROXY_API_TOKEN` for a session cookie, then
 * go to the console. The console itself is the *desktop* front-end served
 * verbatim (see `src/console.rs`), and it has no notion of authentication — so
 * this page is where authentication happens, and it must be a separate document
 * rather than a gate inside that UI.
 */

const form = document.getElementById('form');
const tokenInput = document.getElementById('token');
const submit = document.getElementById('submit');
const errorBox = document.getElementById('err');

function showError(message) {
  errorBox.textContent = message;
  errorBox.classList.add('show');
}

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  submit.disabled = true;
  errorBox.classList.remove('show');

  try {
    const response = await fetch('api/login', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ token: tokenInput.value }),
    });

    if (response.ok) {
      // Clear the field before navigating: the token must not survive in a
      // password input that the back button could restore.
      tokenInput.value = '';
      window.location.replace('.');
      return;
    }

    const body = await response.json().catch(() => null);
    const code = body?.error?.type;

    if (response.status === 503 || code === 'admin_disabled') {
      // Not a wrong password — the console is switched off. Saying so avoids a
      // loop of retyping a token that cannot possibly work.
      showError(
        '管理台未启用：服务器上没有配置 PROXY_API_TOKEN。' +
          '请在 /etc/proxy-rs.env 中设置该项后重启服务（systemctl restart proxy-rs）。'
      );
    } else if (response.status === 401) {
      showError('令牌错误，请检查 /etc/proxy-rs.env 中的 PROXY_API_TOKEN。');
    } else {
      showError(body?.error?.message || `登录失败（HTTP ${response.status}）`);
    }
  } catch (err) {
    showError(`无法连接网关：${err.message}`);
  } finally {
    submit.disabled = false;
  }
});
