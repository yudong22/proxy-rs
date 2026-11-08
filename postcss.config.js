/**
 * PostCSS for the Tailwind entry only.
 *
 * Vite runs every stylesheet imported from the bundle through this config.
 * The 20 hand-written files in `ui/style/` contain no at-rules PostCSS needs to
 * process, so they pass through unchanged (and unminified) — the only file that
 * needs a plugin is `ui/style/tailwind.css`, whose `@tailwind` directives are
 * expanded here.
 */
export default {
  plugins: {
    tailwindcss: {},
    autoprefixer: {},
  },
};
