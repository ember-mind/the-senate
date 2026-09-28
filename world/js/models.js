// Every model the Senate can route work to wears its own colour, so who is
// doing what reads at a glance: a Cohort's tunics, the Censor's hood and the
// chip on their labels all carry the model, never a vendor logo.

export const MODELS = {
  claude: { name: 'Claude', color: '#c2643f' },
  codex: { name: 'Codex', color: '#ece4d2' },
  deepseek: { name: 'DeepSeek', color: '#2f55a4' },
  gemini: { name: 'Gemini', color: '#23918b' },
  qwen: { name: 'Qwen', color: '#7650b0' },
  kimi: { name: 'Kimi', color: '#7f9b6c' },
};

// The model behind a worker or reviewer (`{ provider, model }`), or null when
// nobody is assigned. A provider the world has no colour for keeps its name
// and falls back to the caller's colour.
export function modelOf(who) {
  const id = who?.provider?.toLowerCase();
  if (!id) return null;
  return MODELS[id] || { name: who.provider, color: null };
}

// A label chip naming the model in its colour, with ink that stays readable
// on both the pale and the dark colours.
export function modelChip(m, escape) {
  if (!m) return '';
  const bg = m.color || '#6f7f8c';
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(bg.slice(i, i + 2), 16) / 255);
  const light = 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.55;
  return `<i class="m${light ? ' on-light' : ''}" style="--m:${bg}">${escape(m.name.toUpperCase())}</i>`;
}
