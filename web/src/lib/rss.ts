// The blog's RSS feed for one language.
import rss from '@astrojs/rss';
import { posts, postUrl, strings, type Lang } from './blog';

export const feed = async (lang: Lang, site: URL | undefined) =>
  rss({
    title: strings[lang].name,
    description: strings[lang].feedDescription,
    site: site ?? 'http://localhost:4321',
    customData: `<language>${lang}</language>`,
    items: (await posts(lang)).map((p) => ({
      title: p.data.title,
      description: p.data.description,
      pubDate: p.data.date,
      link: postUrl(p),
      categories: p.data.tags,
    })),
  });
