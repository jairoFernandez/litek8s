import { defineCollection, z } from 'astro:content';
import { glob } from 'astro/loaders';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';

// src/content/docs/docs/ is generated from the repository's markdown by
// scripts/sync-docs.mjs (npm run sync); do not edit it by hand.
// src/content/i18n/<lang>.json overrides Starlight's UI strings per language.
// src/content/blog/*.md are the blog posts (/blog/<file name>/), and
// src/content/blog/es/*.md the Spanish ones (/es/blog/<file name>/, `lang: es`);
// `translation` names the same post in the other language (its id: `es/<file>`
// or `<file>`). `docs` lists the docs pages a post builds on (slugs from
// docs.manifest.mjs, '' for the introduction), linked at its end.
// `series` puts a post in a series (an id from `series` in src/lib/blog.ts):
// its parts in date order, listed on each part and at /blog/series/<id>/.
export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
  blog: defineCollection({
    loader: glob({ pattern: '**/*.md', base: './src/content/blog' }),
    schema: z.object({
      title: z.string(),
      description: z.string().max(160),
      date: z.coerce.date(),
      updated: z.coerce.date().optional(),
      tags: z.array(z.string()).default([]),
      docs: z.array(z.string()).default([]),
      draft: z.boolean().default(false),
      lang: z.enum(['en', 'es']).default('en'),
      translation: z.string().optional(),
      series: z.string().optional(),
    }),
  }),
};
