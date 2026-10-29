// The blog's RSS feed (English).
import type { APIRoute } from 'astro';
import { feed } from '../../lib/rss';

export const GET: APIRoute = ({ site }) => feed('en', site);
