// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import vercel from '@astrojs/vercel';
import { REPO, groups } from './docs.manifest.mjs';

// The public URL, for canonical links and the sitemap: SITE_URL if set, else
// litek8s.dev on Vercel (previews too, so they point search engines at the
// real site), else the local dev server.
const site = process.env.SITE_URL ?? (process.env.VERCEL ? 'https://litek8s.dev' : 'http://localhost:4321');

export default defineConfig({
  site,
  trailingSlash: 'always',
  adapter: vercel({
    webAnalytics: { enabled: true },
  }),
  integrations: [
    starlight({
      title: 'litek8s',
      description: 'A lightweight Kubernetes in one binary, for local development, home labs and the edge.',
      logo: { src: './src/assets/logo.svg', replacesTitle: false },
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: REPO }],
      // English only for now. Spanish: add `es: { label: 'Español', lang: 'es' }`,
      // put translated pages in src/content/docs/es/ and strings in src/i18n/.
      locales: { root: { label: 'English', lang: 'en' } },
      customCss: ['./src/styles/docs.css'],
      lastUpdated: false,
      pagination: true,
      sidebar: groups.map((g) => ({
        label: g.label,
        items: g.pages.map((p) => ({
          label: p.label,
          slug: p.slug ? `docs/${p.slug}` : 'docs',
          ...(p.badge ? { badge: { text: p.badge, variant: 'caution' } } : {}),
        })),
      })),
      components: { SiteTitle: './src/components/docs/SiteTitle.astro' },
    }),
  ],
});
