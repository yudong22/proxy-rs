import { defineConfig } from 'vite';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

/**
 * Vite build for the console front-end.
 *
 * The output layout is not a free choice. `src/console.rs` compiles `ui/` into
 * the binary via `build.rs` and serves it at three fixed prefixes —
 * `/admin/src/*`, `/admin/style/*`, `/admin/assets/*` — and the settings below
 * keep every built file under those names so the routes and `build.rs`'s roots
 * need no change.
 *
 * ## One thing the console cannot do without
 *
 * `console.rs` injects the server bridge by replacing the literal
 * `<script type="module" src="src/main.js"></script>` in the page. A Vite
 * build rewrites that tag (hash, `crossorigin`, moved into `<head>`) and the
 * replacement would silently find nothing — a server console with no IPC at
 * all. So the plugin at the bottom re-emits the tag in exactly that shape, with
 * the hashed file as its `src`; the console's string match then works
 * unchanged. `ui/tools/check-dist.mjs` asserts the tag is there.
 *
 * `ui/` is never modified: it stays the source tree the checks and `ui-dev`
 * read. `ui-dist/` is the build output (gitignored).
 */
export default defineConfig({
  root: resolve(here, 'ui'),
  base: './', // relative URLs, so the page works from any /admin/ mount
  publicDir: false, // `ui/` has no public/ dir; assets come from imports

  build: {
    outDir: resolve(here, 'ui-dist'),
    emptyOutDir: true,
    // Never inline assets as data URIs: `build.rs` embeds files into the
    // binary, and a data URI would be a string inside a JS chunk instead.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    rollupOptions: {
      input: { main: resolve(here, 'ui/index.html') },
      output: {
        // Chunks: `src/` — the console's `/admin/src/*` route.
        entryFileNames: 'src/[name]-[hash].js',
        chunkFileNames: 'src/[name]-[hash].js',
        // Stylesheets: `style/`; everything else (images, fonts): `assets/`.
        assetFileNames: (info) => {
          const name = info.name || '';
          if (name.endsWith('.css')) return 'style/[name]-[hash][extname]';
          return 'assets/[name]-[hash][extname]';
        },
      },
    },
  },

  plugins: [
    {
      name: 'proxy-rs:console-script-tag',
      // Build only. In dev there is no hashed entry to restore, and the source
      // `index.html` already carries the tag in the shape `console.rs` matches;
      // running this there would (and did) fail every page load.
      apply: 'build',
      /**
       * Re-emit the entry script in the exact form `console.rs` searches for.
       *
       * Runs after Vite has produced its HTML, so it can pick the hashed entry
       * out of the generated tag rather than guessing the name.
       */
      transformIndexHtml: {
        order: 'post',
        handler(html) {
          const m = /<script type="module" crossorigin src="\.\/(src\/main-[^"]+\.js)"><\/script>/.exec(html);
          if (!m) {
            throw new Error(
              'proxy-rs:console-script-tag: could not find the built entry script in index.html. '
              + 'console.rs injects the server bridge by matching this tag; update both together.',
            );
          }
          return html.replace(
            m[0],
            `<script type="module" src="${m[1]}"></script>`,
          );
        },
      },
    },
  ],

  server: {
    // The dev loop (`task ui-dev`) runs beside the gateway, so it must not share
    // the gateway's port. `ui/tools/dev.mjs` starts Vite with `createServer`
    // and passes this same config, so the port lives here, in one place.
    host: '127.0.0.1',
    port: Number(process.env.PORT) || 4300,
    strictPort: true,
    // Everything the gateway owns is proxied to a real `proxy-rs-server`; the
    // page and its modules come from `ui/`. There is deliberately no mock
    // bridge here: a panel broken against the real gateway must look broken in
    // the dev loop too, which is the whole reason for reading the real backend.
    proxy: Object.fromEntries(
      ['/admin', '/api'].map((p) => [
        p,
        { target: (process.env.BACKEND || 'http://127.0.0.1:3456').replace(/\/+$/, ''), changeOrigin: false },
      ]),
    ),
  },
});
