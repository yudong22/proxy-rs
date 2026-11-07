/**
 * Timeline entry markup for the 会话 detail pane.
 *
 * Pure functions: a `TimelineEntry` from `src/claude_sessions.rs` in, an HTML
 * string out. Nothing here touches the DOM or fetches.
 *
 * ## The shape follows Claude Code's own transcript
 *
 * The reader already knows how a Claude Code session reads, so the detail pane
 * reproduces it rather than inventing a chat UI:
 *
 *   ❯ user prompt                          (a bubble, the only bordered thing)
 *   ⏺ Bash(ls -la)                         (a tool call: name + one-line args)
 *     ⎿ / 12 lines of output               (its result, expanded, inline)
 *   assistant text                         (inline, no bubble)
 *
 * Thinking renders as a dim italic block, meta lines as a quiet aside.
 *
 * ## Tool output follows Claude Code's rule: never scroll, collapse instead
 *
 * Claude Code does not put tool output in a scrolling box. Short results are
 * shown inline; a long one shows its first lines and a `… +N lines` marker, and
 * the full text is one press away (`Ctrl+O` in the terminal). This pane does
 * the same, with a click replacing the key: the output block never has its own
 * scrollbar — a nested scroll area inside the transcript scroll is how you lose
 * your place while reading a long session.
 *
 * ## Escaping is the whole job
 *
 * Every string on the timeline is transcript content — model output, user
 * prompts, tool arguments, file contents returned by tools. It is rendered
 * through one auditable `esc` helper; `raw` is never used with a file value.
 * This is the single largest injection surface in the UI, and
 * `ui/tools/smoke.mjs` asserts that a `<script>` in the transcript renders as
 * text.
 *
 * ## Tool results are paired, not stacked
 *
 * Claude Code stores every `tool_result` as a user-side entry. Matched to its
 * `tool_use` by id, it renders under the call; unpaired ones (the call is on an
 * older page) still render, so nothing silently disappears.
 */

import { Raw } from '../../core/dom.js';
import { formatLocalTime, formatTokens } from '../../lib/format.js';

function esc(v) {
  return String(v ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

const NL = '\n';

/**
 * How many leading lines a tool result shows before it is folded.
 *
 * Matches Claude Code's feel: a `Read` of 50 lines shows its head and a
 * "+N lines" marker, not 50 lines of noise between two assistant messages.
 */
const RESULT_PREVIEW_LINES = 6;

function tokenSpan(d) {
  if (!d.tokens) return '';
  const inp = Number(d.tokens.input) || 0;
  const outp = Number(d.tokens.output) || 0;
  if (!inp && !outp) return '';
  return `<span class="sess-tokens" title="输入 ${inp} · 输出 ${outp}">↓${formatTokens(inp)} ↑${formatTokens(outp)}</span>`;
}

/**
 * A small per-line footer (time · model · tokens).
 *
 * Deliberately sparse: Claude Code's transcript does not decorate every line,
 * and a timestamp on each of 200 entries is noise. The tool line and the user
 * prompt carry it; assistant prose does not.
 */
function lineMeta(d) {
  const bits = [];
  if (d.sidechain) bits.push('<span class="sess-ribbon">子代理</span>');
  const t = formatLocalTime(d.ts_ms);
  if (t) bits.push(`<span class="sess-time">${t}</span>`);
  if (d.model) bits.push(`<span class="sess-model">${esc(d.model)}</span>`);
  const tok = tokenSpan(d);
  if (tok) bits.push(tok);
  return bits.length ? `<span class="sess-line-meta">${bits.join(' ')}</span>` : '';
}

/** One line of a tool's arguments: collapse JSON/whitespace so the call reads. */
function argPreview(entry) {
  const raw = entry.tool_input || '';
  let text = '参数';
  let parsed = null;
  try {
    parsed = JSON.parse(raw);
  } catch {
    parsed = null;
  }
  if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
    // The first meaningful string field reads far better than raw JSON:
    // Bash → command, Read/Edit/Write → file_path, Grep → pattern.
    const order = ['command', 'file_path', 'path', 'pattern', 'url', 'query', 'description', 'prompt'];
    for (const k of order) {
      if (typeof parsed[k] === 'string' && parsed[k].trim()) {
        text = parsed[k].trim().replace(/\s+/g, ' ');
        break;
      }
    }
    if (text === '参数') {
      text = Object.keys(parsed).slice(0, 3).map((k) => `${k}=${typeof parsed[k] === 'string' ? parsed[k] : JSON.stringify(parsed[k])}`).join(' ');
    }
  } else if (raw && parsed === null) {
    text = raw.replace(/\s+/g, ' ').trim();
  }
  if (text.length > 160) text = `${text.slice(0, 160)}…`;
  return text;
}

/** Pretty-print a JSON string if it parses; otherwise pass it through. */
function prettyJson(text) {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

function preOf(text, cls = '') {
  return `<pre class="sess-pre ${cls}">${esc(text)}</pre>`;
}

/**
 * A tool result: first [`RESULT_PREVIEW_LINES`] lines inline, the rest behind a
 * click.
 *
 * No scrollbar anywhere: the preview is a plain `<pre>`, and the full text is a
 * `<details>`. That is the whole point — a scrolling result box inside the
 * timeline makes a long transcript impossible to read.
 */
function resultBody(text, isError) {
  const body = String(text || '（无输出）');
  const lines = body.split(NL);
  const errCls = isError ? ' is-error' : '';
  if (lines.length <= RESULT_PREVIEW_LINES + 2) {
    return `<div class="sess-tool-result${errCls}"><span class="sess-tool-arrow">⎿</span>${preOf(body)}</div>`;
  }
  const head = lines.slice(0, RESULT_PREVIEW_LINES).join(NL);
  const hidden = lines.length - RESULT_PREVIEW_LINES;
  return `<div class="sess-tool-result sess-tool-result--folded${errCls}"><span class="sess-tool-arrow">⎿</span><div class="sess-result-body"><details class="sess-result"><summary><pre class="sess-pre">${esc(head)}</pre><span class="sess-result-more">… 还有 ${hidden} 行，点击展开</span></summary><pre class="sess-pre sess-pre--full">${esc(body)}</pre></details></div></div>`;
}

function userBubble(entry) {
  const cut = entry.text_truncated ? '<span class="sess-cut">…（已截断）</span>' : '';
  return `<div class="sess-turn is-user"><div class="sess-prompt">${esc(entry.text.replace(/\s*\n\s*/g, NL))}${cut}</div>${lineMeta(entry)}</div>`;
}

function assistantText(entry) {
  const cut = entry.text_truncated ? '<span class="sess-cut">…（已截断）</span>' : '';
  return `<div class="sess-turn is-assistant"><div class="sess-prose">${esc(entry.text)}${cut}</div></div>`;
}

function thinking(entry) {
  return `<div class="sess-turn is-thinking"><span class="sess-thinking-label">思考</span><div class="sess-thinking-text">${esc(entry.text)}</div></div>`;
}

function toolCall(entry, result) {
  const name = esc(entry.tool_name || '工具');
  const args = esc(argPreview(entry));
  // Full arguments in a disclosure only when they carry more than the preview.
  const fullArgs = entry.tool_input && entry.tool_input.length > 0
    ? `<details class="sess-tool-args"><summary>完整参数</summary>${preOf(prettyJson(entry.tool_input))}</details>`
    : '';
  const resultHtml = result ? resultBody(result.text, result.tool_result_is_error) : '';
  return `<div class="sess-turn is-tool"><div class="sess-tool-line"><span class="sess-tool-dot">⏺</span><span class="sess-tool-name">${name}</span><span class="sess-tool-args-preview">(${args})</span>${lineMeta(entry)}</div>${fullArgs}${resultHtml}</div>`;
}

function metaLine(entry) {
  return `<div class="sess-turn is-meta"><div class="sess-meta-text">${esc(entry.text)}</div></div>`;
}

function orphanResult(entry) {
  return `<div class="sess-turn is-tool is-orphan"><div class="sess-tool-line"><span class="sess-tool-dot">⎿</span><span class="sess-tool-name">工具结果</span>${lineMeta(entry)}</div>${resultBody(entry.text, entry.tool_result_is_error)}</div>`;
}

/**
 * Render one page of entries. `results` maps `tool_use_id` → result entry so a
 * tool call can carry its output inline.
 *
 * Every transcript string goes through `esc` here, and nothing is ever marked
 * raw. That is deliberate: one flat, auditable escape helper beats relying on
 * template-tag internals across nested snippets.
 */
export function renderTimeline(entries, results) {
  const used = new Set();
  let out = '';
  for (const entry of entries) {
    if (entry.kind === 'tool_result') {
      if (!results.has(entry.tool_use_id) || used.has(entry.uuid)) continue;
      out += orphanResult(entry);
      continue;
    }
    if (entry.kind === 'text') {
      out += entry.role === 'user' ? userBubble(entry) : assistantText(entry);
      continue;
    }
    if (entry.kind === 'thinking') {
      out += thinking(entry);
      continue;
    }
    if (entry.kind === 'tool_use') {
      const result = entry.tool_use_id ? results.get(entry.tool_use_id) : null;
      if (result) used.add(result.uuid);
      out += toolCall(entry, result);
      continue;
    }
    if (entry.kind === 'meta' && entry.text) {
      out += metaLine(entry);
      continue;
    }
  }
  return new Raw(out);
}

/**
 * `tool_use_id` → the paired result entry over one timeline page.
 *
 * Keyed by the result's `uuid` too so the caller can tell which results were
 * consumed by a call (and skip them rather than render them twice).
 */
export function pairResults(entries) {
  const map = new Map();
  for (const e of entries) {
    if (e.kind === 'tool_result' && e.tool_use_id) map.set(e.tool_use_id, e);
  }
  return map;
}
