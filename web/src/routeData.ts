// Starlight route middleware (astro.config.mjs `routeMiddleware`): what each
// docs page adds to its <head> for search engines and link previews.
// - `seoTitle` from docs.manifest.mjs replaces the short sidebar title in
//   <title> and og:title ("Install" says little in a search result).
// - The social card (public/og.png) for og:image and twitter:image; Starlight
//   sets twitter:card=summary_large_image but no image.
// - JSON-LD: the page as a TechArticle, and its place in the docs as a
//   BreadcrumbList (Home > Docs > group > page).
// - The 404 page is kept out of the index.
import { defineRouteMiddleware } from '@astrojs/starlight/route-data';
import { groups } from '../docs.manifest.mjs';

type Head = { tag: string; attrs?: Record<string, string | boolean | undefined>; content?: string }[];

const byId = new Map<string, { group: string; label: string; seoTitle?: string }>();
for (const g of groups) for (const p of g.pages) byId.set(p.slug ? `docs/${p.slug}` : 'docs', { group: g.label, label: p.label, seoTitle: p.seoTitle });

export const onRequest = defineRouteMiddleware((context) => {
  const route = context.locals.starlightRoute;
  const head = route.head as Head;
  const site = context.site ?? new URL('http://localhost:4321');
  const siteTitle = route.siteTitle;

  if (route.id === '404') {
    head.push({ tag: 'meta', attrs: { name: 'robots', content: 'noindex' } });
    return;
  }

  const page = byId.get(route.id);
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
  const docs = new URL('/docs/', site).href;
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
