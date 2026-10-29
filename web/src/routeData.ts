// Starlight route middleware (astro.config.mjs `routeMiddleware`): what each
// docs page adds to its <head> for search engines and link previews.
// - `seoTitle` from docs.manifest.mjs replaces the short sidebar title in
//   <title> and og:title ("Install" says little in a search result).
// - The social card (public/og.png) for og:image and twitter:image; Starlight
//   sets twitter:card=summary_large_image but no image.
// - JSON-LD: the page as a TechArticle, and its place in the docs as a
//   BreadcrumbList (Home > Docs > group > page).
// - The 404 page is kept out of the index.
// - Translated docs pages (/es/docs/...) use their translation's seoTitle.
// - Blog pages keep only the hreflang links they set themselves (`data-blog`):
//   Starlight's own point every locale at the same path, which blog posts in
//   another language (other slug) or without a translation do not have.
import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { groups } from '../docs.manifest.mjs';

type Head = { tag: string; attrs?: Record<string, string | boolean | undefined>; content?: string }[];

type Tr = { label: string; seoTitle?: string };
const byId = new Map<string, { group: string; groupEs?: string; label: string; seoTitle?: string; translations?: Record<string, Tr> }>();
for (const g of groups)
  for (const p of g.pages)
    byId.set(p.slug ? `docs/${p.slug}` : 'docs', { group: g.label, groupEs: g.translations?.es, label: p.label, seoTitle: p.seoTitle, translations: p.translations });

export const onRequest = defineRouteMiddleware((context) => {
  const route = context.locals.starlightRoute;
  const head = route.head as Head;
  const site = context.site ?? new URL('http://localhost:4321');
  const siteTitle = route.siteTitle;

  if (route.id === '404') {
    head.push({ tag: 'meta', attrs: { name: 'robots', content: 'noindex' } });
    return;
  }

  if (/^\/(es\/)?blog\//.test(context.url.pathname)) {
    for (let i = head.length - 1; i >= 0; i--) {
      const a = head[i].attrs;
      if (head[i].tag === 'link' && a?.rel === 'alternate' && a.hreflang && !a['data-blog']) head.splice(i, 1);
    }
  }

  const lang = route.lang;
  const base = byId.get(route.id.replace(/^es\//, ''));
  const tr = lang !== 'en' && route.id.startsWith(`${lang}/`) ? base?.translations?.[lang] : undefined;
  const page = base && { ...base, label: tr?.label ?? base.label, seoTitle: tr ? tr.seoTitle : base.seoTitle, group: (lang === 'es' && base.groupEs) || base.group };
  const pageTitle = route.entry.data.title;
  if (page?.seoTitle) {
    const title = head.find((h) => h.tag === 'title');
    if (title) title.content = `${page.seoTitle} | ${siteTitle}`;
    const og = head.find((h) => h.attrs?.property === 'og:title');
    if (og?.attrs) og.attrs.content = page.seoTitle;
  }

  const image = new URL('/og.png', site).href;
  const alt = 'litek8s: a lightweight Kubernetes in one binary';
  head.push(
    { tag: 'meta', attrs: { property: 'og:image', content: image } },
    { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
    { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
    { tag: 'meta', attrs: { property: 'og:image:alt', content: alt } },
    { tag: 'meta', attrs: { name: 'twitter:image', content: image } },
  );

  // Blog pages (src/pages/blog/) bring their own JSON-LD.
  if (!page) return;

  const url = new URL(context.url.pathname, site).href;
  const home = new URL('/', site).href;
  const docs = new URL(lang === 'en' ? '/docs/' : `/${lang}/docs/`, site).href;
  const crumbs = [{ name: siteTitle, item: home }, { name: 'Docs', item: docs }];
  if (page && route.id !== 'docs') crumbs.push({ name: page.label, item: url });
  const ld = {
    '@context': 'https://schema.org',
    '@graph': [
      {
        '@type': 'TechArticle',
        headline: page?.seoTitle ?? pageTitle,
        description: route.entry.data.description,
        url,
        image,
        inLanguage: route.lang,
        isPartOf: { '@type': 'WebSite', name: siteTitle, url: home },
        about: { '@type': 'SoftwareApplication', name: 'litek8s', url: home },
        ...(page ? { articleSection: page.group } : {}),
      },
      {
        '@type': 'BreadcrumbList',
        itemListElement: crumbs.map((c, i) => ({ '@type': 'ListItem', position: i + 1, name: c.name, item: c.item })),
      },
    ],
  };
  // `<` escaped so the JSON can never close the script element.
  head.push({ tag: 'script', attrs: { type: 'application/ld+json' }, content: JSON.stringify(ld).replace(/</g, '\\u003c') });
});
