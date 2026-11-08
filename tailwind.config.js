/** @type {import('tailwindcss').Config} */
export default {
  content: ['./ui/index.html', './ui/src/**/*.js'],
  /**
   * Preflight is OFF.
   *
   * `ui/style/base.css` is this project's reset, and it is not a generic one: it
   * owns the window chrome (`user-select: none` on the app shell), the global
   * `[hidden] { display: none !important }` that several flex panes depend on,
   * and the `box-sizing` reset every stylesheet above it assumes. Tailwind's
   * preflight sets `border-style: solid; border-width: 0` on every element (among
   * other rules) and would land *after* base.css in the cascade, silently
   * changing the box model of the existing 20 stylesheets. Keeping preflight off
   * makes adding Tailwind a no-op for every current rule — verified by running
   * `smoke.mjs` against the built CSS.
   */
  corePlugins: { preflight: false },

  theme: {
    /**
     * The palette is the existing one, read straight off the CSS variables in
     * `style/tokens.css` rather than copied as literal values. That matters for
     * dark mode: `--surface` is redefined under `prefers-color-scheme: dark`, so
     * `bg-surface` follows the theme with no `dark:` variant and no second
     * palette to keep in sync. Copying hex values here is exactly the drift the
     * README's rule 1 exists to prevent.
     */
    colors: {
      transparent: 'transparent',
      current: 'currentColor',
      inherit: 'inherit',

      bg: 'var(--bg)',
      surface: 'var(--surface)',
      surface2: 'var(--surface2)',
      border: 'var(--border)',
      text: 'var(--text)',
      muted: 'var(--muted)',
      'muted-ink': 'var(--muted-ink)',

      accent: 'var(--accent)',
      'accent-hover': 'var(--accent-hover)',
      'accent-ink': 'var(--accent-ink)',
      success: 'var(--success)',
      'success-ink': 'var(--success-ink)',
      warning: 'var(--warning)',
      'warning-ink': 'var(--warning-ink)',
      error: 'var(--error)',
      'error-ink': 'var(--error-ink)',
    },

    extend: {
      fontFamily: {
        ui: 'var(--font-ui)',
        mono: 'var(--mono)',
      },
      // Fixed button heights come from the design system (README: ".btn is 34px,
      // .btn-small is 29px"). Exposing them as named sizes keeps new utility
      // markup on the same scale instead of inventing `h-8`/`h-9`.
      spacing: {
        btn: '34px',
        'btn-sm': '29px',
        header: 'var(--header-height)',
        tabs: 'var(--tabs-height)',
        chrome: 'var(--chrome-offset)',
      },
      borderRadius: {
        sm: 'var(--radius-sm)',
        md: 'var(--radius-md)',
        DEFAULT: 'var(--radius)',
      },
      boxShadow: {
        sm: 'var(--shadow-sm)',
        DEFAULT: 'var(--shadow)',
        focus: 'var(--shadow-focus)',
        palette: 'var(--shadow-palette)',
      },
    },
  },

  plugins: [],
};
