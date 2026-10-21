# litek8s website

The landing page and the documentation site, built with [Astro](https://astro.build)
and [Starlight](https://starlight.astro.build) into static files. It is not part of
the Rust crate: Cargo's `include` list leaves `web/` out, the release workflow
never looks at it, and the `test` workflow skips commits that only touch `web/`.

```bash
cd web
npm ci
npm run dev          # http://localhost:4321, rebuilds on change
npm run build        # static site in dist/
npm run preview      # serve dist/
npm run check-links  # after a build: every internal link and #anchor resolves
```

Node ≥ 22.22 (Astro 7).

## Where the content comes from

- **Docs** are the repository's own markdown, not a copy. `npm run sync` (run by
  `dev` and `build`) reads `docs/*.md` and `CONTRIBUTING.md` and writes
  `src/content/docs/docs/*.md` (git-ignored): `docs.manifest.mjs` says which
  file becomes which page and its place in the sidebar (the "Start here" pages
  are `docs/overview.md`, `install.md`, `quickstart.md`, `numbers.md`,
  `platforms.md`, `multi-node.md`, `limitations.md`). Links between synced
  files become site links, `docs/ui/*.svg` is copied to `public/docs-assets/`,
  and links to anything else in the repository (`../bench/RESULTS.md`,
  `../compat/...`, examples) go to GitHub. A new `docs/*.md` that is not in
  the manifest is reported as a warning. Edit the markdown in the repository,
  never the generated files.
- **README.md** is the GitHub and crates.io landing page and is not synced.
  Its images use absolute `raw.githubusercontent.com` URLs (crates.io renders
  neither relative images nor Mermaid); its links point at litek8s.dev.
  `docs/readme/demo.svg`, the animated terminal at the top, comes from
  `node scripts/readme-demo.mjs` (re-run it after changing the script or
  `docs/ui/home-dark.svg`).
- **Diagrams**: `src/diagrams/*.svg`, drawn from the Mermaid diagrams in
  `docs/overview.md`, `quickstart.md` and `multi-node.md`. The landing page
  inlines them (animated); the sync step puts the same SVG in place of each
  Mermaid block on the docs pages (still).
- **Landing copy and numbers**: `src/i18n/en.ts`. Numbers come from
  `docs/numbers.md` (and the README's comparison table) and link to the
  `bench/` and `compat/` files that measured them; update them together.
- **Logo**: `src/assets/logo.svg` (the mark; also `public/favicon.svg` and
  `public/brand/litek8s-mark.svg`). `node scripts/brand.mjs` renders the PNGs
  (`favicon-32.png`, `apple-touch-icon.png`, `brand/litek8s-mark-512.png`,
  the `og.png` social card); they are committed, re-run it after changing the logo.

## Deploying on Vercel

Import the GitHub repository as a new project with:

| setting | value |
|---|---|
| Framework Preset | Astro |
| Root Directory | `web` |
| Install Command | `npm ci` |
| Build Command | `npm run build` |
| Output Directory | `dist` |
| Node.js Version | 22.x or newer |

`vercel.json` sets the same values, plus long caching for `/_astro/*` and an
ignored-build step that skips deployments when neither `web/` nor the synced
markdown changed. Keep **"Include files outside the Root Directory in the
Build Step"** on (Vercel's default): the sync step reads `../docs/` and
`../CONTRIBUTING.md`. No environment variables are needed: on Vercel the canonical URL
and sitemap use `https://litek8s.dev` (add the domain under Settings →
Domains); `SITE_URL` overrides it.

## Another language

The landing page renders a dictionary (`src/i18n/en.ts`). For Spanish: copy it
to `src/i18n/es.ts` and translate the strings, add `src/pages/es/index.astro`
rendering `<Landing t={es} />`, add `es: { label: 'Español', lang: 'es' }` to
`locales` in `astro.config.mjs`, and put translated docs pages under
`src/content/docs/es/docs/` (Starlight shows the English page where a
translation is missing).
