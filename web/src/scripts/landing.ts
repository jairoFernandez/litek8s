// Everything the landing page does in the browser, without libraries:
// reveals, the terminal demo, the sticky feature story, the tagline that
// lights up with scroll, tabs, copy buttons, theme and menu. Work only
// happens for what is on screen; with reduced motion nothing animates.

declare global {
  interface Window { __lk?: boolean }
}
window.__lk = true;

const root = document.documentElement;
const motion = root.classList.contains('motion');
const $ = <T extends Element = HTMLElement>(sel: string, el: ParentNode = document) => el.querySelector<T>(sel);
const $$ = <T extends Element = HTMLElement>(sel: string, el: ParentNode = document) => [...el.querySelectorAll<T>(sel)];
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** Calls `fn(true|false)` as `el` enters or leaves the viewport. */
function watch(el: Element, fn: (visible: boolean) => void, margin = '0px') {
  new IntersectionObserver((entries) => entries.forEach((e) => fn(e.isIntersecting)), { rootMargin: margin }).observe(el);
}

// ---------- the hero intro: any input skips to the end ----------
const intro = root.classList.contains('play-intro') && motion;
if (intro) {
  const hero = $('.hero')!;
  const skip = () => {
    for (const a of hero.getAnimations({ subtree: true })) {
      if (a.effect?.getComputedTiming().iterations !== Infinity) a.finish();
    }
    stop();
  };
  const events = ['pointerdown', 'keydown', 'wheel', 'touchstart'] as const;
  const stop = () => events.forEach((e) => removeEventListener(e, skip));
  events.forEach((e) => addEventListener(e, skip, { passive: true }));
  setTimeout(stop, 3000);
}

// ---------- theme (same localStorage key as Starlight, so the docs agree) ----------
$('[data-theme-toggle]')?.addEventListener('click', () => {
  const next = root.dataset.theme === 'light' ? 'dark' : 'light';
  root.dataset.theme = next;
  try { localStorage.setItem('starlight-theme', next); } catch {}
});

// ---------- nav ----------
const nav = $('[data-nav]')!;
const onScrollNav = () => nav.classList.toggle('is-scrolled', scrollY > 8);
addEventListener('scroll', onScrollNav, { passive: true });
onScrollNav();
const menu = $('[data-menu]');
menu?.addEventListener('click', () => {
  const open = nav.classList.toggle('is-open');
  menu.setAttribute('aria-expanded', String(open));
});
$$('.nav-links a').forEach((a) => a.addEventListener('click', () => { nav.classList.remove('is-open'); menu?.setAttribute('aria-expanded', 'false'); }));

// ---------- copy buttons ----------
$$<HTMLButtonElement>('[data-copy]').forEach((b) => {
  const label = b.querySelector('span')!;
  const text = label.textContent;
  b.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(b.dataset.copy!); } catch { return; }
    b.classList.add('is-copied');
    label.textContent = b.dataset.done ?? text;
    setTimeout(() => { b.classList.remove('is-copied'); label.textContent = text; }, 1600);
  });
});

// ---------- tabs (WAI-ARIA tablist: click, arrows, Home/End) ----------
function tablist(list: HTMLElement, onSelect?: (tab: HTMLElement, i: number) => void) {
  const tabs = $$<HTMLElement>('[role=tab]', list);
  const select = (i: number, focus = false) => {
    tabs.forEach((t, j) => {
      const on = i === j;
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
      const panel = document.getElementById(t.getAttribute('aria-controls')!);
      if (panel) panel.hidden = !on;
    });
    if (focus) tabs[i].focus();
    onSelect?.(tabs[i], i);
  };
  tabs.forEach((t, i) => {
    t.addEventListener('click', () => select(i));
    t.addEventListener('keydown', (e) => {
      const k = e.key;
      const n = tabs.length;
      const j = k === 'ArrowRight' ? (i + 1) % n : k === 'ArrowLeft' ? (i - 1 + n) % n : k === 'Home' ? 0 : k === 'End' ? n - 1 : -1;
      if (j >= 0) { e.preventDefault(); select(j, true); }
    });
  });
  return select;
}
$$('[data-tabs]').forEach((box) => tablist($('[role=tablist]', box)!));

// ---------- reveals ----------
const revealIO = new IntersectionObserver(
  (entries) => entries.forEach((e) => { if (e.isIntersecting) { e.target.classList.add('is-in'); revealIO.unobserve(e.target); } }),
  { rootMargin: '0px 0px -12% 0px' },
);
$$('[data-reveal]').forEach((el) => revealIO.observe(el));

// Count-ups in the numbers cards, once they are in view.
const fmt = (v: number, digits: number) => v.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits });
$$('.metric').forEach((card) => {
  if (!motion) return;
  const nums = $$('[data-count]', card);
  const target = nums.map((n) => Number(n.dataset.count));
  const digits = nums.map((n) => (n.dataset.count!.split('.')[1] ?? '').length);
  nums.forEach((n) => (n.textContent = (n.dataset.prefix ?? '') + fmt(0, digits[nums.indexOf(n)])));
  const io = new IntersectionObserver((entries) => {
    if (!entries[0].isIntersecting) return;
    io.disconnect();
    const t0 = performance.now();
    const step = (now: number) => {
      const p = Math.min(1, (now - t0) / 1300);
      const e = 1 - Math.pow(1 - p, 4);
      nums.forEach((n, i) => (n.textContent = (n.dataset.prefix ?? '') + fmt(target[i] * e, digits[i])));
      if (p < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }, { rootMargin: '0px 0px -12% 0px' });
  io.observe(card);
});

// The CTA mark hops once.
$$('[data-hop]').forEach((el) => watch(el, (v) => v && el.classList.add('is-in')));

// Pointer spotlight on cards.
if (matchMedia('(hover: hover)').matches) {
  $$('.spot').forEach((c) =>
    c.addEventListener('pointermove', (e) => {
      const r = c.getBoundingClientRect();
      c.style.setProperty('--mx', `${e.clientX - r.left}px`);
      c.style.setProperty('--my', `${e.clientY - r.top}px`);
    }),
  );
}

// Diagrams animate only while visible.
$$('.lk-diagram.is-animated').forEach((d) => watch(d, (v) => d.classList.toggle('is-paused', !v)));

// ---------- scroll-driven pieces: tagline words and the feature story ----------
type Scrubber = { el: HTMLElement; visible: boolean; update: () => void };
const scrubbers: Scrubber[] = [];
let ticking = false;
const onScroll = () => {
  if (ticking) return;
  ticking = true;
  requestAnimationFrame(() => { ticking = false; scrubbers.forEach((s) => s.visible && s.update()); });
};
addEventListener('scroll', onScroll, { passive: true });
addEventListener('resize', onScroll, { passive: true });
function scrubber(el: HTMLElement, update: () => void) {
  const s: Scrubber = { el, visible: false, update };
  scrubbers.push(s);
  watch(el, (v) => { s.visible = v; if (v) update(); });
}

const tagline = $('[data-scrub]');
if (tagline && motion) {
  root.classList.add('js-scrub');
  const words = $$('.w', tagline);
  scrubber(tagline, () => {
    const r = tagline.getBoundingClientRect();
    const vh = innerHeight;
    // 0 when the block's top is at 85% of the viewport, 1 when its bottom is at 55%.
    const p = Math.min(1, Math.max(0, (vh * 0.85 - r.top) / (r.height + vh * 0.3)));
    const lit = Math.round(p * words.length);
    words.forEach((w, i) => w.classList.toggle('lit', i < lit));
  });
}

const story = $('[data-story]');
if (story) {
  const items = $$('[data-story-item]', story);
  const panels = $$('[data-story-panel]', story);
  const bar = $('.story-progress i', story);
  const wide = matchMedia('(min-width: 1024px)');
  let current = 0;
  const activate = (i: number) => {
    if (i === current) return;
    items[current]?.classList.remove('active');
    panels[current]?.classList.remove('active');
    items[i]?.classList.add('active');
    panels[i]?.classList.add('active');
    current = i;
  };
  const progress = () => {
    const r = story.getBoundingClientRect();
    return Math.min(1, Math.max(0, -r.top / (r.height - innerHeight)));
  };
  scrubber(story, () => {
    if (!wide.matches) return;
    const p = progress();
    bar?.style.setProperty('--p', p.toFixed(4));
    activate(Math.min(items.length - 1, Math.floor(p * items.length)));
  });
  items.forEach((it, i) =>
    $('.story-btn', it)!.addEventListener('click', () => {
      if (!wide.matches) return;
      const top = story.getBoundingClientRect().top + scrollY;
      const span = story.offsetHeight - innerHeight;
      scrollTo({ top: top + span * ((i + 0.5) / items.length), behavior: motion ? 'smooth' : 'auto' });
    }),
  );
}

// ---------- the terminal demo ----------
const term = $('[data-term]');
if (term) {
  const scenes = $$('.term-scene', term);
  const tabs = $$<HTMLElement>('.term-tab', term);
  const clock = $('.term-clock', term)!;
  let run = 0; // bumps on every (re)start; a stale player stops at its next step
  let visible = false;
  let waiting: (() => void) | null = null;
  let index = 0;

  const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const line = (cls: string, html = '') => {
    const d = document.createElement('div');
    d.className = `ln ${cls}`;
    d.innerHTML = html;
    return d;
  };
  /** Waits `ms`, and longer while the terminal is off screen or the tab hidden. */
  const pause = async (ms: number, id: number) => {
    await sleep(ms);
    while ((!visible || document.hidden) && id === run) await new Promise<void>((r) => (waiting = r));
    return id === run;
  };
  const resume = () => { if (visible && !document.hidden && waiting) { const w = waiting; waiting = null; w(); } };
  watch(term, (v) => { visible = v; resume(); }, '0px 0px -10% 0px');
  document.addEventListener('visibilitychange', resume);

  async function play(i: number, id: number) {
    const scene = scenes[i];
    const steps: Array<[string, string | number]> = JSON.parse(scene.dataset.steps!);
    scene.replaceChildren();
    const prog = tabs[i].querySelector<HTMLElement>('.prog')!;
    tabs.forEach((t) => t.querySelector<HTMLElement>('.prog')!.style.transform = 'scaleX(0)');
    const total = steps.length;
    for (const [n, [kind, v]] of steps.entries()) {
      prog.style.transition = 'transform .4s';
      prog.style.transform = `scaleX(${(n + 1) / total})`;
      if (kind === '$') {
        const ln = line('cmdline', '<span class="p">$</span> <span class="t"></span><span class="caret is-typing"></span>');
        scene.append(ln);
        const t = ln.querySelector('.t')!;
        const text = String(v);
        if (!(await pause(380, id))) return false;
        for (let k = 1; k <= text.length; k++) {
          t.innerHTML = esc(text.slice(0, k));
          if (!(await pause(text.length > 60 ? 9 : 26 + Math.random() * 30, id))) return false;
        }
        if (!(await pause(260, id))) return false;
        ln.querySelector('.caret')!.remove();
      } else if (kind === '>') {
        if (!(await pause(70, id))) return false;
        scene.append(line('out', String(v)));
      } else if (kind === '~') {
        if (!(await pause(Number(v), id))) return false;
      } else if (kind === '*') {
        // `dev up` waits for the node to be Ready; the clock is the demo's,
        // running for the measured time (bench/RESULTS.md).
        const ms = Number(v);
        clock.classList.remove('is-done');
        clock.classList.add('is-on');
        const t0 = performance.now();
        await new Promise<void>((done) => {
          const tick = () => {
            const e = Math.min(ms, performance.now() - t0);
            clock.textContent = `${(e / 1000).toFixed(2)} s`;
            if (e < ms && id === run) requestAnimationFrame(tick);
            else done();
          };
          requestAnimationFrame(tick);
        });
        if (id !== run) return false;
        clock.classList.add('is-done');
      } else if (kind === 'ui') {
        const box = document.createElement('div');
        box.className = 'term-ui is-new';
        box.innerHTML = `<img src="${term!.dataset.tui}" alt="litek8s ui: the home view with clusters, nodes, CPU and memory meters" width="1028" height="700" decoding="async">`;
        scene.append(box);
      }
    }
    scene.append(line('', '<span class="p">$</span> <span class="caret"></span>'));
    return true;
  }

  const show = (i: number) => {
    index = i;
    tabs.forEach((t, j) => { t.setAttribute('aria-selected', String(i === j)); t.tabIndex = i === j ? 0 : -1; });
    scenes.forEach((s, j) => (s.hidden = i !== j));
    clock.classList.remove('is-on', 'is-done');
  };

  async function loop(start: number) {
    const id = ++run;
    waiting?.();
    let i = start;
    for (;;) {
      show(i);
      if (!(await play(i, id))) return;
      if (!(await pause(scenes[i].dataset.scene === 'ui' ? 5200 : 2600, id))) return;
      i = (i + 1) % scenes.length;
    }
  }

  const select = tablist($('.term-tabs', term)!, (_t, i) => { if (motion) loop(i); else show(i); });
  $('.term-replay', term)!.addEventListener('click', () => (motion ? loop(index) : select(index)));
  if (motion) {
    // Start once it is first on screen.
    // After the hero intro, when there is one.
    const io = new IntersectionObserver((e) => { if (e[0].isIntersecting) { io.disconnect(); setTimeout(() => loop(0), intro ? 2000 : 0); } });
    io.observe(term);
  }
}

export {};
