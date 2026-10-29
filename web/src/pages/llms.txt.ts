import type { APIRoute } from 'astro';
import { llmsTxt } from '../lib/llms';

export const prerender = true;

export const GET: APIRoute = ({ site }) =>
  new Response(llmsTxt(site ?? new URL('http://localhost:4321')), { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
