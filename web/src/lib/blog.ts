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
  },
} as const;
