/**
 * The Claude Code permission-mode control, as a user meets it.
 *
 * ## Why this scenario exists
 *
 * The back end's rules are covered by Rust tests (`claude_config::tests`,
 * `tests/admin_api.rs`), but the two failures that matter here only appear in a
 * rendered page:
 *
 *   · the `<select>` must offer exactly the modes the writer accepts. A value
 *     only the UI knows about is a dead option; one only the writer knows is a
 *     setting a user can never choose. Both are silent at build time.
 *   · the 已修改 badge must clear after a successful write. It did not: the
 *     write re-reads the file into the fields, so they keep differing from the
 *     baseline captured at page load unless the group is re-baselined. The
 *     badge then claims unsaved work forever, which is the kind of lie that
 *     teaches users to ignore badges.
 *
 * The save path is driven for real (`window.__TAURI__` mock records the command
 * and its arguments), so what is asserted is the body the backend would get,
 * not a reimplementation of it.
 */

/** Modes the Rust writer accepts — duplicated here on purpose. */
const ACCEPTED = ['', 'default', 'acceptEdits', 'plan', 'auto', 'dontAsk', 'bypassPermissions'];

const SEL_ID = 'setting-claude-permission-mode';

export async function checkClaudePermissionMode(ctx) {
  const { send, check } = ctx;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { returnByValue: true, expression });
    return r.result.value;
  };

  await send('Runtime.evaluate', { expression: `window.switchTab('settings')` });
  await sleep(600);

  // 1. The control exists, is inside the Claude group, and offers exactly the
  //    accepted set in the order the UI documents.
  const offered = await evaluate(`(() => {
    const sel = document.getElementById('${SEL_ID}');
    const group = document.querySelector('.form-section[data-section="claude"]');
    if (!sel) return { found: false };
    return {
      found: true,
      inGroup: Boolean(group && group.contains(sel)),
      values: [...sel.options].map((o) => o.value),
      labelled: Boolean(sel.labels && sel.labels.length),
    };
  })()`);
  check(`claude permission: control exists in the Claude group`,
    Boolean(offered.found && offered.inGroup), true);
  check(`claude permission: offers every accepted mode, in order`,
    JSON.stringify(offered.values), JSON.stringify(ACCEPTED));
  check(`claude permission: has a <label>`, Boolean(offered.labelled), true);

  // 2. Choosing bypass and saving must send it, and must clear the badge.
  //    Seed an edit first: without one the badge assertion could pass for the
  //    wrong reason (no badge ever appeared).
  await evaluate(`(() => {
    const sel = document.getElementById('${SEL_ID}');
    sel.value = 'default';
    sel.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  const badgeBefore = await evaluate(`Boolean(document.querySelector(
    '.form-section[data-section="claude"] .form-section-badge'))`);
  check(`claude permission: choosing a mode marks the group 已修改`, badgeBefore, true);

  await evaluate(`(() => {
    const sel = document.getElementById('${SEL_ID}');
    sel.value = 'bypassPermissions';
    sel.dispatchEvent(new Event('change', { bubbles: true }));
    document.getElementById('btn-apply-claude-config').click();
  })()`);
  await sleep(800);

  const sent = await evaluate(`window.__INVOKE_ARGS__['apply_claude_config'] || null`);
  check(`claude permission: the write carries the chosen mode`,
    sent && sent.body ? sent.body.permission_mode : '<no call>', 'bypassPermissions');

  const after = await evaluate(`(() => ({
    badge: Boolean(document.querySelector(
      '.form-section[data-section="claude"] .form-section-badge')),
    status: (document.getElementById('claude-config-status') || {}).textContent || '',
  }))()`);
  check(`claude permission: a successful write clears 已修改`, after.badge, false);
  check(`claude permission: success is reported on the page`,
    /写入/.test(after.status), true);
}
