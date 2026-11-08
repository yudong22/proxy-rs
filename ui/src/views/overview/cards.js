/**
 * The overview's static layout: the metric cards, the statistics row and the
 * endpoint reference.
 *
 * Split out of `overview.js` so the markup and the readers that fill it in are
 * separate files: this one owns what the page looks like, `status.js` and
 * `stats.js` own what goes in it.
 */

import { $, html, raw } from '../../core/dom.js';

/**
 * A metric card.
 *
 * Every value starts as a skeleton: a transparent `0` plus `.pending`, which CSS
 * paints as a grey bar over the slot's line (see `cards.css`). The first reader
 * to answer replaces it through `setValue`, and only then is a number shown.
 * Before this, a slow link showed dashes that read like real figures for as long
 * as the first read took — and a fetch that fell back to IPC showed `-` long
 * enough to look like "no data" rather than "still loading".
 *
 * `&nbsp;` rather than a digit is the placeholder text: it is one line box like
 * any glyph, but it gives the slot zero *width*, so a skeleton is a grey bar and
 * not a bar behind a faint "0" that reads as a real value. (A digit was the
 * first choice for width stability; the bar is absolutely positioned and sized
 * by percentage, so it never depended on the text's width.)
 *
 * @param {string} id       Element the value is written into.
 * @param {string} label
 * @param {{cardId?: string, clickable?: boolean, expandable?: boolean,
 *          title?: string, sub?: string, wide?: boolean}} [opts]
 *        `cardId` is only needed when something targets the card itself.
 *        `clickable` means the card does something on click (cursor + hover
 *        shadow). `expandable` means it opens a detail panel *below itself*, and
 *        is what earns the disclosure caret and the open-state surface — a
 *        caret on a card that merely refreshes a value or jumps to another tab
 *        would promise content that never appears.
 *        `sub` renders a secondary element (an id for the sub-line) so a card
 *        can carry a second, smaller line under its value. It is rendered empty
 *        from the start so the card's height does not wait on its text.
 *        `wide` is a hint for the skeleton bar only: cards whose values are
 *        strings (`42 tok/s`, `1.2M`) get a wider bar than the short-number ones.
 */
function metric(
  id,
  label,
  {
    cardId = '', clickable = false, expandable = false, title = '',
    sub = '', wide = false,
  } = {},
) {
  return html`
    <div class="metric-card${raw(clickable ? ' clickable' : '')}${raw(expandable ? ' expandable' : '')}"${raw(cardId ? ` id="${cardId}"` : '')}${raw(title ? ` title="${title}"` : '')}>
      <div class="metric-label">${label}${raw(expandable ? ' <span class="stat-caret"></span>' : '')}</div>
      <div class="metric-value pending${raw(wide ? ' metric-value--wide' : '')}" id="${id}">&nbsp;</div>
      ${raw(sub ? `<div class="metric-sub" id="${sub}"></div>` : '')}
    </div>
  `;
}

/** The endpoint reference rows: label → id, with a copy button on some. */
const ENDPOINTS = [
  { label: 'Messages API', id: 'endpoint-messages', copy: true },
  { label: 'Responses API', id: 'endpoint-responses', copy: true },
  { label: 'Models API', id: 'endpoint-models', copy: true },
  { label: '当前上游地址', id: 'overview-upstream' },
  { label: '配置文件', id: 'overview-config-path', value: '~/.proxy-rs/gui-settings.json' },
  { label: '日志文件', id: 'overview-log-path', value: '~/.proxy-rs/logs/proxy.log' },
  { label: '开机自启动', id: 'overview-lal', plain: true },
];

/** Render this view into its mount point. Called once, from main.js. */
export function renderOverview() {
  const root = $('#tab-overview');
  if (!root) return;

  const rows = ENDPOINTS.map(e => html`
    <dt>${e.label}</dt>
    <dd><code id="${e.id}">${e.value || ''}</code>${
      raw(e.copy ? ` <button class="btn-copy" data-copy="${e.id}">复制</button>` : '')
    }</dd>
  `).join('');

  root.innerHTML = html`
    <div class="metrics-grid">
      ${raw(metric('metric-provider', '配置厂商', { wide: true }))}
      ${raw(metric('metric-points', '剩余积分', {
        cardId: 'stat-card-points', clickable: true, title: '点击刷新剩余积分',
        sub: 'metric-points-account',
      }))}
      ${raw(metric('metric-speed', '输出速度', {
        cardId: 'stat-card-speed', clickable: true, expandable: true,
        title: '点击查看本次生成详情', wide: true,
      }))}
      ${raw(metric('metric-overrides', '今日 override 次数', {
        cardId: 'stat-card-overrides', clickable: true, title: '点击查看请求日志',
      }))}
    </div>

    <div class="metrics-grid">
      ${raw(metric('stat-requests', '请求总数', { wide: true }))}
      ${raw(metric('stat-tokens-total', 'Token 总量', { wide: true }))}
      ${raw(metric('stat-cache-pct', '缓存命中率'))}
      ${raw(metric('stat-requests-failed', '失败请求'))}
    </div>

    <!-- Expanded by the 输出速度 card: one transposed table, metrics as rows and
         the three most recent conversations as columns, so the same measure can
         be read across sessions at a glance. A cell with no measurement shows
         "—" rather than a plausible-looking guess. -->
    <div class="section stat-detail" id="speed-detail" hidden>
      <table class="speed-sessions">
        <tbody id="speed-sessions-body"></tbody>
      </table>
    </div>

    <div class="section">
      <div class="section-title">本地调用接口</div>
      <dl class="kv">${raw(rows)}</dl>
    </div>
  `;
}
