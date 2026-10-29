// The blog's RSS feed.
import rss from '@astrojs/rss';
import type { APIRoute } from 'astro';
import { posts, postUrl } from '../../lib/blog';

export const GET: APIRoute = async ({ site }) =>
  rss({
    title: 'litek8s blog',
    description: 'Guides and notes about litek8s, a lightweight Kubernetes in one binary.',
    site: site ?? 'http://localhost:4321',
    items: (await posts()).map((p) => ({
      title: p.data.title,
      description: p.data.description,
      pubDate: p.data.date,
      link: postUrl(p),
      categories: p.data.tags,
    })),
  });
