// Blog posts (src/content/blog/, Spanish ones in src/content/blog/es/), newest
// first; drafts only in `astro dev`.
import { getCollection, type CollectionEntry } from 'astro:content';
import { pages, localRoute } from '../../docs.manifest.mjs';

export type Post = CollectionEntry<'blog'>;
export type Lang = 'en' | 'es';

export async function posts(lang: Lang = 'en'): Promise<Post[]> {
  const all = await getCollection('blog', (p) => (import.meta.env.DEV || !p.data.draft) && p.data.lang === lang);
  return all.sort((a, b) => b.data.date.valueOf() - a.data.date.valueOf());
}

/** A post's slug in its language's URL (`es/x` → `x`). */
export const postSlug = (p: Post) => p.id.replace(/^es\//, '');

/** The blog's root for a language. */
export const blogRoot = (lang: Lang) => (lang === 'en' ? '/blog/' : `/${lang}/blog/`);

export const postUrl = (p: Post) => `${blogRoot(p.data.lang)}${postSlug(p)}/`;

/** The same post in the other language, if it is published (or in `astro dev`). */
export async function translationOf(p: Post): Promise<Post | undefined> {
  if (!p.data.translation) return undefined;
  const all = await getCollection('blog', (q) => import.meta.env.DEV || !q.data.draft);
  return all.find((q) => q.id === p.data.translation);
}

/** The docs pages a post lists under "Read the docs", by manifest slug, in its language. */
export function docsLinks(p: Post) {
  const lang = p.data.lang;
  return p.data.docs.map((slug) => {
    const page = pages.find((d) => d.slug === slug);
    if (!page) throw new Error(`blog/${p.id}: no docs page "${slug}" in docs.manifest.mjs`);
    const tr = lang === 'en' ? undefined : page.translations?.[lang];
    return { label: tr?.seoTitle ?? tr?.label ?? page.seoTitle ?? page.title ?? page.label, href: localRoute(slug, lang) };
  });
}

/** Series of posts on one subject, by id (a post's `series`), in each language. */
export const series = {
  speed: {
    en: { title: 'Making litek8s fast', description: 'Measuring the API server, finding where its time goes and taking it back, one profile at a time.' },
    es: { title: 'Haciendo litek8s veloz', description: 'Medir el API server, encontrar en qué se le va el tiempo y recuperarlo, un perfil a la vez.' },
  },
} as const satisfies Record<string, Record<Lang, { title: string; description: string }>>;

export type SeriesId = keyof typeof series;

/** A series' page for a language. */
export const seriesUrl = (id: string, lang: Lang) => `${blogRoot(lang)}series/${id}/`;

/** A series' id, title and parts (oldest first) in a language; throws on an unknown id. */
export async function seriesParts(id: string, lang: Lang) {
  const meta = (series as Record<string, (typeof series)[SeriesId]>)[id];
  if (!meta) throw new Error(`blog: no series "${id}" in src/lib/blog.ts`);
  const parts = (await posts(lang)).filter((p) => p.data.series === id).reverse();
  return { id, ...meta[lang], url: seriesUrl(id, lang), parts };
}

/** The series a post is part of, with its position (0-based). */
export async function seriesOf(p: Post) {
  if (!p.data.series) return undefined;
  const s = await seriesParts(p.data.series, p.data.lang);
  return { ...s, index: s.parts.findIndex((q) => q.id === p.id) };
}

export const dateFmt = (d: Date, lang: Lang = 'en') =>
  d.toLocaleDateString(lang, { year: 'numeric', month: 'long', day: 'numeric', timeZone: 'UTC' });

/** The blog's own words, per language. */
export const strings = {
  en: {
    title: 'Blog',
    name: 'litek8s blog',
    description: 'Guides and notes about litek8s, a lightweight Kubernetes in one binary: home labs, dev clusters, CI, observability and more.',
    feedDescription: 'Guides and notes about litek8s, a lightweight Kubernetes in one binary.',
    intro: (docs: string, rss: string) => `Longer walk-throughs of what the <a href="${docs}">docs</a> describe. <a href="${rss}">RSS feed</a>.`,
    other: { label: 'En español', lang: 'es' as Lang },
    readDocs: 'Read the docs',
    allPosts: '← All posts',
    translated: 'Leer en español',
    series: 'Series',
    seriesPart: (n: number, total: number) => `Part ${n} of ${total} in the series`,
    seriesLead: (n: number) => `${n} ${n === 1 ? 'post' : 'posts'}, oldest first.`,
    previous: 'Previous',
    next: 'Next',
  },
  es: {
    title: 'Blog',
    name: 'Blog de litek8s',
    description: 'Guías y notas sobre litek8s, un Kubernetes ligero en un solo binario: home labs, clusters de desarrollo, CI, observabilidad y más.',
    feedDescription: 'Guías y notas sobre litek8s, un Kubernetes ligero en un solo binario.',
    intro: (docs: string, rss: string) => `Recorridos más largos de lo que describe la <a href="${docs}">documentación</a>. <a href="${rss}">Feed RSS</a>.`,
    other: { label: 'In English', lang: 'en' as Lang },
    readDocs: 'Lee la documentación',
    allPosts: '← Todos los posts',
    translated: 'Read in English',
    series: 'Series',
    seriesPart: (n: number, total: number) => `Parte ${n} de ${total} de la serie`,
    seriesLead: (n: number) => `${n} ${n === 1 ? 'post' : 'posts'}, del más antiguo al más nuevo.`,
    previous: 'Anterior',
    next: 'Siguiente',
  },
} as const;
