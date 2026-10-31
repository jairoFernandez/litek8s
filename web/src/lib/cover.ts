// A blog post's page image (the index card and the post's header): its
// illustration public/blog/covers/<english post id>.webp, shared by a post
// and its translation, else its social card (lib/og.ts), else none. Covers
// are only for the pages; og:image stays the social card.
import fs from 'node:fs';
import path from 'node:path';
import { blogCard } from './og';
import type { Post } from './blog';

// `astro build` and `astro dev` run in web/.
const publicDir = path.resolve(process.cwd(), 'public');

export interface Cover {
  src: string;
  width: number;
  height: number;
  /** true for the illustration, false for the social card fallback. */
  illustration: boolean;
}

/** The English post a post belongs to (its own id, or its translation's for a Spanish post). */
export const englishId = (p: Post) => (p.data.lang === 'en' ? p.id : (p.data.translation ?? p.id));

/** A valid CSS <custom-ident> for a post: `<prefix>-<english id>`, shared by EN and ES. */
export const transitionName = (prefix: 'cover' | 'title', p: Post) =>
  `${prefix}-${englishId(p).replace(/[^a-zA-Z0-9_-]/g, '-')}`;

/** Width and height of a WebP file (VP8, VP8L or VP8X), if it parses. */
function webpSize(file: string): { width: number; height: number } | undefined {
  const b = Buffer.alloc(30);
  const fd = fs.openSync(file, 'r');
  try {
    fs.readSync(fd, b, 0, 30, 0);
  } finally {
    fs.closeSync(fd);
  }
  if (b.toString('ascii', 0, 4) !== 'RIFF' || b.toString('ascii', 8, 12) !== 'WEBP') return undefined;
  const chunk = b.toString('ascii', 12, 16);
  if (chunk === 'VP8X') return { width: 1 + b.readUIntLE(24, 3), height: 1 + b.readUIntLE(27, 3) };
  if (chunk === 'VP8 ') return { width: b.readUInt16LE(26) & 0x3fff, height: b.readUInt16LE(28) & 0x3fff };
  if (chunk === 'VP8L') {
    const bits = b.readUInt32LE(21);
    return { width: 1 + (bits & 0x3fff), height: 1 + ((bits >> 14) & 0x3fff) };
  }
  return undefined;
}

/** The image a post shows on the blog's pages, if any. */
export function coverOf(p: Post): Cover | undefined {
  const rel = `blog/covers/${englishId(p)}.webp`;
  const file = path.join(publicDir, rel);
  if (fs.existsSync(file)) {
    const size = webpSize(file) ?? { width: 1536, height: 1024 };
    return { src: `/${rel}`, ...size, illustration: true };
  }
  const card = blogCard(p.id);
  return card ? { src: card, width: 1200, height: 630, illustration: false } : undefined;
}
