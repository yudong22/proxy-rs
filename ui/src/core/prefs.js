/**
 * 浏览器里那一点点「记住」：本仓库前端第一个 Web Storage 模块。
 *
 * ## 为什么需要它
 *
 * 地址（hash）承载「现在在看哪个会话」，但切到别的 tab 时 hash 必然被改写成
 * `#logs`，id 就此丢掉。「切走再回来还停在同一会话」这条要求靠地址做不到，
 * 只能靠存储。
 *
 * ## 分工：URL 是地址，storage 是记忆
 *
 * 地址里指定了会话就照地址（可分享、可收藏的那一半）；地址没指定时才用这里的
 * 记录兜底。所以本模块**不写** URL —— 写入地址是会话视图的事（`selectSession`
 * 成功后 `replaceState`），一个模块只负责一件事。
 *
 * ## 三条硬要求（都是「存储不可用不能把页面搞坏」）
 *
 * 1. 每次读写都 try/catch：隐私模式、禁用 cookie、配额满都会抛异常；
 * 2. 不可用时降级成内存 Map：行为退化成「刷新丢会话」（与 3.5.5 相同，不是新故障）；
 * 3. 键名带版本：值的形状变了就换键，不去猜旧值能不能读。
 *
 * 多标签页各写各的、互不同步，是本版已知限制（见 `ui/README.md`）。
 */

/** 键名带版本号。改值的形状就换成 v2，旧键留着不读即可。 */
const KEY = 'proxy-rs.lastSession.v1';

/**
 * 来源 slug 的形状（`[a-z][a-z0-9_]{0,15}`）。
 *
 * 这里再校验一次的原因和 `index.js` 里的那份不同：那份守的是地址栏，这份守的是
 * **存储**——值可能是别的版本、上一次发布、或人手改的，读出来当不可信输入处理。
 */
const SOURCE_RE = /^[a-z][a-z0-9_]{0,15}$/;

/** `localStorage` 不可用时的兜底；生命周期就是页面生命周期。 */
const memory = new Map();

function readRaw(key) {
  try {
    const value = window.localStorage.getItem(key);
    if (value !== null) return value;
  } catch {
    /* 不可用就落到内存 */
  }
  return memory.has(key) ? memory.get(key) : null;
}

function writeRaw(key, value) {
  try {
    window.localStorage.setItem(key, value);
    return;
  } catch {
    /* 配额满／被禁：内存里留着，至少同一页面生命周期内还能恢复 */
  }
  memory.set(key, value);
}

function removeRaw(key) {
  try {
    window.localStorage.removeItem(key);
  } catch {
    /* 清不掉也不该让调用方失败 */
  }
  memory.delete(key);
}

/**
 * 上次成功打开过的会话，`{ source, id, at }`，没有或读不出来时为 null。
 *
 * 形状不对就当没有：这个值不参与任何路径拼接，但它会被当成一次
 * `sessions_detail` 的参数，所以宁可丢掉也不要把半截值送下去。
 */
export function getLastSession() {
  const raw = readRaw(KEY);
  if (!raw) return null;
  try {
    const value = JSON.parse(raw);
    if (!value || typeof value.source !== 'string' || typeof value.id !== 'string') return null;
    if (!SOURCE_RE.test(value.source) || !value.id) return null;
    return { source: value.source, id: value.id, at: Number(value.at) || 0 };
  } catch {
    return null;
  }
}

/** 记下刚打开的会话。只记成功了的那次（调用点见 `selectSession`）。 */
export function setLastSession({ source, id }) {
  if (!source || !id) return;
  writeRaw(KEY, JSON.stringify({ source, id, at: Date.now() }));
}

/** 忘掉它。恢复失败（文件被删、来源下线）时清键，不再反复试同一个死会话。 */
export function clearLastSession() {
  removeRaw(KEY);
}
