// El feed RSS del blog en español.
import type { APIRoute } from 'astro';
import { feed } from '../../../lib/rss';

export const GET: APIRoute = ({ site }) => feed('es', site);
