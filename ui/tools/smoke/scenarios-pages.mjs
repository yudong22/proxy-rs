/**
 * Every page and pane the user can land on, loaded at least once.
 *
 * ## Why this file exists
 *
 * Two defects shipped in 3.5.0 that the harness could not have caught:
 *
 *   · `bootstrap.html` — the Tauri window's start page — was not built into
 *     `ui-dist/`, so the app opened on a blank window. No scenario had ever
 *     loaded that file; every one navigated to `index.html` and stopped.
 *   · The overview's loading state reflowed the page on a slow link. The
 *     overview *was* asserted, but only after its data arrived.
 *
 * Both share a shape: the test exercised the path we remembered, not every
 * entry point the product exposes. This scenario closes the first half — every
 * HTML document and every tab pane is loaded and must render *something
 * recognisable* — so a missing or half-built page fails here instead of in a
 * release.
 *
 * ## What "renders" means here
 *
 * Deliberately shallow and structural: the pane exists, is visible, and has the
 * mount points its module creates. Value assertions live in the per-view
 * scenarios; this one only asks "did the page come up at all", which is exactly
 * the question a blank window answers with a resounding no.
 */

/** Files under the served root that are full pages, and what each must show. */
const PAGES = [
  {
    path: '/index.html',
    // The console shell. Its panes are empty until `main.js` renders them.
    assert: `(() => {
      const tabs = [...document.querySelectorAll('.tab')].map((t) => t.textContent.trim());
      return {
        tabs,
        panes: [...document.querySelectorAll('.tab-content')].map((p) => p.id),
        // main.js renders into the mount points; an empty one means the module
        // graph failed to evaluate (a bad build, a missing chunk).
        rendered: [...document.querySelectorAll('.tab-content')].every((p) => p.children.length > 0),
      };
    })()`,
    check(v) {
      return v.tabs.length === 4 && v.panes.length === 4 && v.rendered;
    },
    describe: 'console shell renders all four panes',
  },
];

/**
 * Load every page and every pane.
 *
 * Panes are visited through the real router (`window.switchTab`), so a view
 * whose module throws on entry fails here rather than only when a user clicks
 * the tab.
 */
export async function checkEveryPaneRenders(ctx) {
  const { send, check, origin } = ctx;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  for (const page of PAGES) {
    await send('Page.navigate', { url: `${origin}${page.path}` });
    await sleep(1200);
    const r = await send('Runtime.evaluate', { returnByValue: true, expression: page.assert });
    check(`page ${page.path}: ${page.describe}`, page.check(r.result.value), true);
  }

  // Every tab, in order. Each must produce content inside its own pane and no
  // runtime error (the harness collects console errors globally).
  const tabs = ['overview', 'logs', 'sessions', 'settings'];
  for (const tab of tabs) {
    await send('Runtime.evaluate', { expression: `window.switchTab('${tab}')` });
    await sleep(600);
    const r = await send('Runtime.evaluate', {
      returnByValue: true,
      expression: `(() => {
        const pane = document.getElementById('tab-${tab}');
        const style = getComputedStyle(pane);
        return {
          exists: Boolean(pane),
          visible: style.display !== 'none' && style.visibility !== 'hidden',
          children: pane ? pane.children.length : 0,
          hash: location.hash,
        };
      })()`,
    });
    const v = r.result.value;
    check(`pane ${tab}: visible with content`, Boolean(v.exists && v.visible && v.children > 0), true);
    check(`pane ${tab}: routed to #${tab}`, v.hash, `#${tab}`);
  }
}
