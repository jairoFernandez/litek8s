// Which social card (og:image / twitter:image) a page uses: a blog post's
// own card from scripts/og-blog.mjs (public/og/blog/<post id>.png) when it
// exists, else the site's public/og.png.
import fs from 'node:fs';
import path from 'node:path';

// `astro build` and `astro dev` run in web/.
const publicDir = path.resolve(process.cwd(), 'public');

/** The card's site path for a blog post id (`x`, `es/x`), if one was rendered. */
export function blogCard(postId: string): string | undefined {
  const rel = `og/blog/${postId}.png`;
  return fs.existsSync(path.join(publicDir, rel)) ? `/${rel}` : undefined;
}

/** The blog post id a site path belongs to (`/blog/x/` → `x`, `/es/blog/x/` → `es/x`). */
export function postIdOfPath(pathname: string): string | undefined {
  const m = pathname.match(/^\/(?:(es)\/)?blog\/([^/]+)\/?$/);
  return m ? (m[1] ? `${m[1]}/${m[2]}` : m[2]) : undefined;
}

/** og:image path for any page. */
export function cardFor(pathname: string): string {
  const id = postIdOfPath(pathname);
  return (id && blogCard(id)) || '/og.png';
}
