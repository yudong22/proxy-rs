/**
 * The overview's loading state: does the page hold still while data arrives?
 *
 * On a slow link the first reads take seconds. The page is built synchronously
 * at boot (every card, the endpoint list, the header), and only the *values*
 * arrive later — so the page must not reflow when they land. It did: the
 * skeleton was a second block in the value slot, which made the cards 30px
 * taller until data arrived and pushed every section below them down.
 *
 * ## How "before data" is captured
 *
 * The value reads are held at the mock bridge (`__HOLD__` in `smoke.mjs`) and
 * released explicitly. Holding instead of slowing is what makes the test
 * deterministic: a slow response still races the measurement on a loaded
 * machine.
 *
 * Network interception was the first attempt and cannot work here: each value
 * may arrive over `GET /api/*` *or* the IPC bridge, and `core/api.js` falls
 * back from the former to the latter after its own timeout — so a request held
 * at the network only delays the read by 4 s before the bridge answers it.
 *
 * It runs **last** in `smoke.mjs`: it reloads the page, discarding the DOM every
 * earlier scenario worked against.
 */

/**
 * Read by the mock bridge at boot (see `__HOLD__` in `smoke.mjs`). These are
 * every command that fills the overview: the status read (配置厂商 / 剩余积分),
 * the two totals reads, and the 输出速度 read.
 */
const HOLD = ['get_status', 'get_stats', 'get_session_metrics'];

/**
 * A geometry read of the overview, as a short JSON-able object.
 *
 * Each visible top-level block contributes `[top, height]`. Height matters as
 * much as top: the cards grid starts at a fixed inset, so a card growing leaves
 * its top edge in place and moves only its bottom — and everything after it.
 */
const GEOMETRY = `(() => {
  const overview = document.querySelector('#tab-overview');
  const toggle = document.querySelector('#btn-toggle-service');
  return {
    blocks: [...overview.children]
      .filter((el) => !el.hidden && el.getBoundingClientRect().height > 0)
      .map((el) => {
        const r = el.getBoundingClientRect();
        return [Math.round(r.top), Math.round(r.height)];
      }),
    // The status pill widens from 未启动 to 运行中 · 端口 3456 (36 px → 104 px).
    // Its *left* edge and the toggle's must both stay put: a centred pill in a
    // box that grows both ways would push each sideways, and an unreserved one
    // would push them left as it grew right.
    anchor: toggle ? Math.round(toggle.getBoundingClientRect().left) : null,
    pillLeft: (() => {
      const el = document.querySelector('.status-badge');
      return el ? Math.round(el.getBoundingClientRect().left) : null;
    })(),
    // Slots still showing a skeleton. Used only to prove the page really was
    // measured mid-load.
    pending: [...document.querySelectorAll('#tab-overview .metric-value.pending')].length,
    // The 剩余积分 card's sub-line. A poll never refills it, so its *text* is
    // the one signal that the page has not yet had its first load at all.
    subText: (document.querySelector('#metric-points-account') || {}).textContent || '',
  };
})()`;

const measure = async (send) => {
  const r = await send('Runtime.evaluate', { returnByValue: true, expression: GEOMETRY });
  return r.result.value;
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Reload the page with the value reads held, measure it, release, measure again.
 */
export async function checkOverviewLayout(ctx) {
  const { send, check } = ctx;

  // Read by the bridge's `__HOLD__` at load, so it must be registered before the
  // navigation — and removed after, or every later reload would inherit it.
  const { identifier } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `window.__HOLD_NAMES__ = ${JSON.stringify(HOLD)};`,
  });

  await send('Page.navigate', { url: `${ctx.origin}/index.html` });

  // Poll until the page has rendered (the cards exist) *and* its first reads
  // are in flight. Both matter: measuring before the render would compare an
  // empty page with a full one, which is a jump of a different kind.
  let early = null;
  for (let i = 0; i < 40; i += 1) {
    await sleep(100);
    const g = await measure(send);
    const heldCount = (await send('Runtime.evaluate', {
      returnByValue: true, expression: 'window.__heldCount__ ? window.__heldCount__() : -1',
    })).result.value;
    // The sub-line text is set by `refreshStatus` alone, and its command is
    // held — so an empty one proves nothing has painted values yet.
    if (heldCount > 0 && g.blocks.length >= 3 && g.subText === '') {
      early = g;
      break;
    }
  }
  check('overview: the page rendered and its value reads were held', Boolean(early), true);
  if (!early) return;

  await send('Runtime.evaluate', { expression: 'window.__RELEASE__()' });
  await sleep(1200);
  const settled = await measure(send);

  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier });

  check('overview: skeleton was showing before the values landed', early.pending > 0, true);
  check('overview: no skeleton survives once the values land', settled.pending, 0);
  check('overview: the page does not move when the values land', settled.blocks, early.blocks);
  check('overview: the header does not move when the status pill widens',
    [settled.anchor, settled.pillLeft], [early.anchor, early.pillLeft]);
}
