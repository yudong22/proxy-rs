/**
 * "已修改" badges.
 *
 * Which fields are tracked per group, and the load-time snapshot they are
 * compared against. Reads `document` directly, so it imports nothing.
 */

/**
 * Snapshot of every tracked field as the backend last reported it.
 *
 * The badge means "this value has been changed since it was loaded", so it is
 * measured against this snapshot rather than against a hard-coded default
 * table. A table would have to duplicate the provider-preset logic — e.g.
 * `force_stream: null` means "follow the preset", whose effective value only
 * the backend knows — and would flag untouched fields as modified.
 */
const baseline = new Map();

/** IDs whose value the badges track, grouped by the section they belong to. */
const SECTION_FIELDS = {
  provider: ['setting-provider', 'setting-custom-url'],
  network: ['setting-port', 'setting-bind', 'setting-launch-at-login'],
  // Only the enable switch is a form field here; the account list is server
  // state managed by its own commands, so it has nothing to compare against and
  // must not make the section read as "已修改".
  'client-accounts': ['client-auth-enabled'],
  // The permission mode is tracked with the model ids because it is written by
  // the same button, into the same file. Leaving it out would let "pick
  // bypassPermissions" — a change with far more consequence than a model id —
  // show no 已修改 badge at all.
  claude: [
    'setting-claude-model',
    'setting-claude-sonnet',
    'setting-claude-opus',
    'setting-claude-haiku',
    'setting-claude-permission-mode',
  ],
  codex: [],
  advanced: [
    'setting-reasoning-model',
    'setting-completion-model',
    'setting-model-map',
    'setting-sanitize-terms',
    'setting-force-stream',
    'setting-intercept-classifier',
  ],
};

/** Current value of a tracked field, normalised for comparison. */
function fieldValue(el) {
  return el.type === 'checkbox' ? el.checked : String(el.value);
}

/** Record the form's current values as the new baseline (badges clear). */
export function captureBaseline() {
  baseline.clear();
  for (const id of Object.values(SECTION_FIELDS).flat()) {
    const el = document.getElementById(id);
    if (el) baseline.set(id, fieldValue(el));
  }
}

/**
 * Re-baseline one group, leaving every other group's badge alone.
 *
 * Needed by the writers that save *one* group rather than the whole form: the
 * Claude Code button writes only `~/.claude/settings.json`, so calling
 * [`captureBaseline`] there would also swallow unsaved edits in 网络与自启 and
 * 模型重定向 — their badges would clear without the change having been saved.
 */
export function captureSectionBaseline(section) {
  for (const id of SECTION_FIELDS[section] ?? []) {
    const el = document.getElementById(id);
    if (el) baseline.set(id, fieldValue(el));
  }
}

/** True when the field differs from the value captured at load time. */
function isModified(id) {
  const el = document.getElementById(id);
  if (!el || !baseline.has(id)) return false;
  return fieldValue(el) !== baseline.get(id);
}

/**
 * Recompute every group's badge. Called after the form is loaded and after any
 * edit, so the badge tracks the form rather than the last saved state.
 */
export function refreshSectionBadges() {
  for (const [section, fields] of Object.entries(SECTION_FIELDS)) {
    const group = document.querySelector(`.form-section[data-section="${section}"]`);
    if (!group) continue;

    const summary = group.querySelector('summary');
    const modified = fields.some(isModified);
    let badge = summary?.querySelector('.form-section-badge');

    if (modified && !badge && summary) {
      badge = document.createElement('span');
      badge.className = 'form-section-badge';
      badge.textContent = '已修改';
      summary.appendChild(badge);
    } else if (!modified && badge) {
      badge.remove();
    }
  }
}
