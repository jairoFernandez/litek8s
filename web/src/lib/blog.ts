// Blog posts (src/content/blog/), newest first; drafts only in `astro dev`.
import { getCollection, type CollectionEntry } from 'astro:content';
import { pages, route } from '../../docs.manifest.mjs';

export type Post = CollectionEntry<'blog'>;

export async function posts(): Promise<Post[]> {
  const all = await getCollection('blog', (p) => import.meta.env.DEV || !p.data.draft);
  return all.sort((a, b) => b.data.date.valueOf() - a.data.date.valueOf());
}

export const postUrl = (p: Post) => `/blog/${p.id}/`;

/** The docs pages a post lists under "Read the docs", by manifest slug. */
export function docsLinks(p: Post) {
  return p.data.docs.map((slug) => {
    const page = pages.find((d) => d.slug === slug);
    if (!page) throw new Error(`blog/${p.id}: no docs page "${slug}" in docs.manifest.mjs`);
    return { label: page.seoTitle ?? page.title ?? page.label, href: route(slug) };
  });
}

export const dateFmt = (d: Date) => d.toLocaleDateString('en', { year: 'numeric', month: 'long', day: 'numeric', timeZone: 'UTC' });
