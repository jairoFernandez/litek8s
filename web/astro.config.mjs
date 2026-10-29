// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import vercel from '@astrojs/vercel';
import { REPO, groups, LOCALES } from './docs.manifest.mjs';

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
      // English at the root, Spanish under /es/ (docs.manifest.mjs `translations`;
      // untranslated pages fall back to English with a notice).
      locales: { root: { label: 'English', lang: 'en' }, ...LOCALES },
      customCss: ['./src/styles/docs.css'],
      lastUpdated: false,
      pagination: true,
      sidebar: [
        ...groups.map((g) => ({
          label: g.label,
          ...(g.translations ? { translations: g.translations } : {}),
          items: g.pages.map((p) => ({
            label: p.label,
            ...(p.translations ? { translations: Object.fromEntries(Object.entries(p.translations).map(([l, tr]) => [l, tr.label])) } : {}),
            slug: p.slug ? `docs/${p.slug}` : 'docs',
            ...(p.badge ? { badge: { text: p.badge, variant: 'caution' } } : {}),
          })),
        })),
        { label: 'Blog', link: '/blog/' },
      ],
      // The blog's RSS feed, announced on every page.
      head: [{ tag: 'link', attrs: { rel: 'alternate', type: 'application/rss+xml', title: 'litek8s blog', href: '/blog/rss.xml' } }],
      components: { SiteTitle: './src/components/docs/SiteTitle.astro' },
      // Per-page SEO: seoTitle, social card image, JSON-LD (src/routeData.ts).
      routeMiddleware: './src/routeData.ts',
    }),
  ],
});
