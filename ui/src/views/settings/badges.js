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
  claude: ['setting-claude-model', 'setting-claude-sonnet', 'setting-claude-opus', 'setting-claude-haiku'],
  codex: [],
  advanced: [
    'setting-reasoning-model',
    'setting-completion-model',
    'setting-model-map',
    'setting-sanitize-terms',
    'setting-force-stream',
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
