import { defineCollection } from 'astro:content';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';

// The docs collection is populated by scripts/sync-docs.mjs, which mirrors the
// repo-root docs/ tree into src/content/docs/ (git-ignored) so that docs and
// code stay versioned together in one place: docs/.
export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
};
