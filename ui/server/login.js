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
const rememberInput = document.getElementById('remember');
const submit = document.getElementById('submit');
const errorBox = document.getElementById('err');
const warnBox = document.getElementById('warn');

function showError(message) {
  warnBox.classList.remove('show');
  errorBox.textContent = message;
  errorBox.classList.add('show');
}

/**
 * Warn that a lockout is approaching, without implying the token was wrong.
 *
 * Shown *before* the lockout lands because that is the only moment it can change
 * the outcome: the operator can stop and re-read the token instead of burning
 * the last attempt on the same typo.
 */
function showWarning(message) {
  warnBox.textContent = message;
  warnBox.classList.add('show');
}

/**
 * Log in by submitting the form natively.
 *
 * Deliberately NOT `fetch()`. Browsers only offer to save a password after a real
 * form submission that navigates; an XHR/fetch login looks like an AJAX call and
 * the "save password?" prompt never appears. Since saving the token is exactly
 * what the operator wants here, the page posts for real and the server answers
 * with a redirect to the console.
 *
 * The one thing still done in JS is refusing to submit an empty token, so the
 * common mistake produces the inline message instead of a round trip.
 */
/**
 * The shape of a session id, for the deep-link form `sessions/<uuid>`.
 *
 * Duplicated from the console's own session view on purpose: this page is a
 * separate document that loads before (and without) the console bundle, so it
 * cannot import it. The server validates again — this only decides what to carry
 * through a login round trip.
 */
const SESSION_ID = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}';

/**
 * Landing targets this page will carry through a (re)login.
 *
 * A bare tab, or a deep link into one session. Both are closed shapes: neither
 * can express a host, a scheme or a traversal, so accepting the longer one does
 * not turn this into an open redirect — the server re-validates the same way
 * (`landing_path`), and that is the real boundary.
 *
 * The deep link is here at all because otherwise the operator's whole reason for
 * visiting this page ("log in, then show me that session") would be lost the
 * moment the cookie had expired.
 */
const LANDING = new RegExp(`^(?:(?:overview|logs|sessions|settings)|sessions/${SESSION_ID})$`);

/**
 * The tab to return to after (re)login.
 *
 * Order: a `?next=` on this URL (carried forward by a failed attempt) → the
 * hash this page was opened with. The server re-validates, so this is
 * convenience, not the security boundary.
 */
function landingTab() {
  const fromQuery = new URLSearchParams(window.location.search).get('next');
  const fromHash = window.location.hash.replace(/^#/, '');
  const candidate = fromQuery || fromHash;
  return LANDING.test(candidate) ? candidate : '';
}

const nextInput = document.getElementById('next');
const landing = landingTab();
if (nextInput) nextInput.value = landing;
// Keep the tab in the URL bar while on the login page, so the operator can see
// where they will land and a refresh does not lose it.
if (landing && window.location.hash !== `#${landing}`) {
  window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}#${landing}`);
}

form.addEventListener('submit', (event) => {
  const token = tokenInput.value.trim();
  if (!token) {
    event.preventDefault();
    showError('请输入管理令牌。');
    return;
  }
  // The button must not be re-enabled on navigation failure; the browser owns the
  // page from here. Disabling it also prevents a double post.
  submit.disabled = true;
  submit.textContent = '登录中…';
  errorBox.classList.remove('show');
  warnBox.classList.remove('show');
});

/**
 * The server redirects back with `?e=<code>` when a native post fails.
 *
 * A failed form post cannot return JSON (the browser navigates), so the reason
 * travels in the query string and is rendered here.
 */
(function reportFormError() {
  const params = new URLSearchParams(window.location.search);
  const code = params.get('e');
  // Carry the landing tab forward so a retry still returns to it. Same shape as
  // `landingTab` — a deep link must survive a wrong-token retry too, or the
  // second attempt lands on the overview and the session is lost.
  const carried = params.get('next');
  if (carried && LANDING.test(carried)) {
    if (nextInput) nextInput.value = carried;
    window.history.replaceState(null, '', `${window.location.pathname}?${params.toString()}#${carried}`);
  }
  if (!code) return;

  submit.disabled = false;
  submit.textContent = '登录';
  const wait = Number(params.get('wait')) || 0;

  if (code === 'rate_limited') {
    showError(`登录尝试过于频繁。请等待 ${wait || 60} 秒后重试。`);
    let remaining = wait || 60;
    submit.disabled = true;
    submit.textContent = `请等待 ${remaining} 秒`;
    const timer = setInterval(() => {
      remaining -= 1;
      if (remaining <= 0) {
        clearInterval(timer);
        submit.disabled = false;
        submit.textContent = '登录';
        return;
      }
      submit.textContent = `请等待 ${remaining} 秒`;
    }, 1000);
    return;
  }
  if (code === 'admin_disabled') {
    showError(
      '管理台未启用：服务器上没有配置 PROXY_API_TOKEN。' +
        '请在 /etc/proxy-rs.env 中设置该项后重启服务（systemctl restart proxy-rs）。'
    );
    return;
  }
  showError('令牌错误，请检查 /etc/proxy-rs.env 中的 PROXY_API_TOKEN。');
  const left = Number(params.get('left'));
  if (Number.isFinite(left) && left > 0) {
    showWarning(
      left === 1 ? '再失败 1 次将锁定登录 1 分钟。' : `再失败 ${left} 次将锁定登录 1 分钟。`
    );
  }
})();

