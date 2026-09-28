// The browser's only door to Senate state. Live mode reads the local
// server's projection with the per-session token; demo mode reads the
// deterministic fixtures. Nothing here writes except `askConsul`, which asks
// the same service the terminal uses.

import { SCENARIOS, demoOrderDetail, storyAt } from './demo.js';

const params = new URLSearchParams(location.search);
const demo = params.get('demo');
// Which campaign this tab shows; the server falls back to its own default.
const mission = params.get('mission');
const scope = (path) => (mission ? `${path}${path.includes('?') ? '&' : '?'}mission=${encodeURIComponent(mission)}` : path);
const token = new URLSearchParams(location.hash.slice(1)).get('token') || sessionStorageGet('senate-token');
if (token) sessionStorageSet('senate-token', token);
// Keep the token out of the visible URL once read.
if (location.hash.includes('token=')) history.replaceState(null, '', location.pathname + location.search);

function sessionStorageGet(k) {
  try {
    return sessionStorage.getItem(k);
  } catch {
    return null;
  }
}
function sessionStorageSet(k, v) {
  try {
    sessionStorage.setItem(k, v);
  } catch {
    /* private mode: the token lives only in memory */
  }
}

// A capture run drives its own clock (window.__senateClock, in ms) and starts at 0.
const started = window.__senateClock !== undefined ? 0 : performance.now();

async function get(path) {
  const r = await fetch(scope(path), { headers: { 'X-Senate-Token': token || '' }, cache: 'no-store' });
  if (!r.ok) throw new Error(`${path}: ${r.status}`);
  return r.json();
}

export const source = demo ? 'demo' : 'live';
export const demoName = demo;

export async function fetchWorld() {
  if (demo) {
    if (demo === 'story') return storyAt(((window.__senateClock ?? performance.now()) - started) / 1000);
    return (SCENARIOS[demo] || SCENARIOS.review)();
  }
  return get('/api/world');
}

export async function fetchOrder(state, id) {
  if (demo) return demoOrderDetail(state, id);
  return get(`/api/order/${encodeURIComponent(id)}`);
}

export async function fetchConsul() {
  if (demo) {
    return {
      turns: [
        { role: 'you', text: 'How goes the war on the checkout bugs?' },
        {
          role: 'consul',
          text: 'Cohort I is working on Fix the free coupon. Fix the double charge is with the Censor for independent review. Nothing needs your judgment yet; I will tell you when it can be integrated.',
        },
      ],
      busy: false,
      demo: true,
    };
  }
  return get('/api/consul');
}

export async function askConsul(message) {
  if (demo) return { accepted: false, reason: 'This is a demonstration campaign; nothing is sent.' };
  const r = await fetch(scope('/api/consul'), {
    method: 'POST',
    headers: { 'X-Senate-Token': token || '', 'Content-Type': 'application/json' },
    body: JSON.stringify({ message }),
  });
  return r.json();
}
