import { defineCollection } from 'astro:content';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';

// src/content/docs/docs/ is generated from the repository's markdown by
// scripts/sync-docs.mjs (npm run sync); do not edit it by hand.
// src/content/i18n/<lang>.json overrides Starlight's UI strings per language.
export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};
