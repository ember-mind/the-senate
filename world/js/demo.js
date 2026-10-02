// Deterministic demonstration campaigns. They use the exact shape the live
// projection serves (`GET /api/world`, schema 1) so the world cannot tell
// them apart; they exist to test whether the spatial language reads before
// real state is wired, and to capture repeatable screenshots.

const T0 = Date.parse('2026-09-26T10:00:00Z');
const minutesAgo = (m) => new Date(T0 - m * 60000).toISOString();

function order(id, cohort, title, state, extra = {}) {
  return {
    id,
    cohort,
    title,
    state,
    activity: null,
    worker: null,
    since: null,
    reason: null,
    ...extra,
  };
}

const deepseek = { provider: 'deepseek', model: 'deepseek-v4' };
const codex = { provider: 'codex', model: 'gpt-5.5' };
const claude = { provider: 'claude', model: 'fable' };

function campaign(orders, extra = {}) {
  const settled = orders.filter((o) => o.state === 'settled').length;
  const active = orders.filter((o) => ['working', 'in_review', 'blocked'].includes(o.state)).length;
  const needs = orders.filter((o) => ['blocked', 'failed', 'delivered'].includes(o.state)).length;
  return {
    id: 'demo-barbarians',
    title: 'Drive out the barbarians',
    goal: 'Fix the three bugs in checkout.',
    state: 'active',
    settled,
    total: orders.length,
    active,
    needs_you: needs,
    ...extra,
  };
}

function world(orders, { consul, censor, attention = [], campaignExtra } = {}) {
  return {
    schema: 1,
    source: 'demo',
    at: new Date(T0).toISOString(),
    campaign: campaign(orders, campaignExtra),
    consul,
    orders,
    censor: censor || { state: 'idle', order: null, reviewer: null },
    attention,
    curia: { state: 'closed' },
  };
}

const settledPair = [
  order('o-map', 3, 'Map the checkout', 'settled'),
  order('o-sentries', 4, 'Post the sentries', 'settled'),
];

export const SCENARIOS = {
  // Full field: six Orders, including a defensive rank running checks.
  battle: () => world([
    'Fix the free coupon', 'Fix the double charge', 'Keep the cart on reload',
    'Check tax rounding', 'Verify the refund', 'Harden checkout',
  ].map((title, i) => order(`o-battle-${i}`, i + 1, title, 'working', {
    activity: i === 5 ? 'testing' : 'coding', worker: [deepseek, codex, claude][i % 3], since: minutesAgo(4 + i),
  })), {
    consul: { state: 'quiet', summary: ['Six Cohorts are on the field.', 'Cohort VI is running checks.'], lead_turns: 3 },
    campaignExtra: { goal: 'Secure checkout and verify its behavior.' },
  }),
  // Nothing is running. The Senate is quiet, and that is the point.
  quiet: () =>
    world(
      [
        ...settledPair,
        order('o-coupon', 1, 'Fix the free coupon', 'settled'),
        order('o-double', 2, 'Fix the double charge', 'settled'),
        order('o-cart', 5, 'Keep the cart on reload', 'planned'),
      ],
      {
        consul: {
          state: 'quiet',
          summary: ['Nothing is running.', '4 of 5 Orders are settled.', 'Keep the cart on reload is planned and waits for your word to start.'],
          lead_turns: 3,
        },
      },
    ),

  // One Cohort at work.
  one: () =>
    world(
      [
        ...settledPair,
        order('o-coupon', 1, 'Fix the free coupon', 'working', { activity: 'coding', worker: deepseek, since: minutesAgo(6) }),
        order('o-double', 2, 'Fix the double charge', 'ready'),
        order('o-cart', 5, 'Keep the cart on reload', 'planned'),
      ],
      {
        consul: {
          state: 'quiet',
          summary: ['Cohort I is working on Fix the free coupon.', 'Fix the double charge is ready to start.', 'Nothing needs your judgment.'],
          lead_turns: 3,
        },
      },
    ),

  // The brief's reference state: A working, B under independent review.
  review: () =>
    world(
      [
        ...settledPair,
        order('o-coupon', 1, 'Fix the free coupon', 'working', { activity: 'coding', worker: deepseek, since: minutesAgo(6) }),
        order('o-double', 2, 'Fix the double charge', 'in_review', { activity: 'reviewing', worker: codex, since: minutesAgo(21) }),
        order('o-cart', 5, 'Keep the cart on reload', 'planned'),
      ],
      {
        consul: {
          state: 'quiet',
          summary: ['Cohort I is working on Fix the free coupon.', 'The Censor is reviewing Fix the double charge.', 'Nothing needs your judgment.'],
          lead_turns: 3,
        },
        censor: { state: 'reviewing', order: 'o-double', reviewer: claude },
      },
    ),

  // Tests running on one desk, a stop on another.
  blocked: () =>
    world(
      [
        ...settledPair,
        order('o-coupon', 1, 'Fix the free coupon', 'working', { activity: 'testing', worker: deepseek, since: minutesAgo(14) }),
        order('o-double', 2, 'Fix the double charge', 'blocked', {
          activity: 'coding',
          worker: codex,
          since: minutesAgo(9),
          reason: 'Asks whether to refund the customers charged twice.',
        }),
        order('o-cart', 5, 'Keep the cart on reload', 'planned'),
      ],
      {
        consul: {
          state: 'awaiting_you',
          summary: ['Fix the double charge has stopped: it asks whether to refund the customers charged twice.', 'Cohort I is running the tests for Fix the free coupon.'],
          lead_turns: 3,
        },
        attention: [{ kind: 'blocked', order: 'o-double', text: 'Asks whether to refund the customers charged twice.' }],
      },
    ),

  // B has delivered and waits to be integrated; A still working.
  complete: () =>
    world(
      [
        ...settledPair,
        order('o-coupon', 1, 'Fix the free coupon', 'working', { activity: 'reading', worker: deepseek, since: minutesAgo(2) }),
        order('o-double', 2, 'Fix the double charge', 'delivered', { worker: codex, since: minutesAgo(1) }),
        order('o-cart', 5, 'Keep the cart on reload', 'planned'),
      ],
      {
        consul: {
          state: 'awaiting_you',
          summary: ['Fix the double charge passed review and waits to be integrated.', 'Cohort I is reading the code for Fix the free coupon.'],
          lead_turns: 3,
        },
        attention: [{ kind: 'awaiting_integration', order: 'o-double', text: 'Delivered; integrate to settle it.' }],
      },
    ),

  // No campaign at all.
  empty: () => ({
    schema: 1,
    source: 'demo',
    at: new Date(T0).toISOString(),
    campaign: null,
    consul: { state: 'quiet', summary: ['There is no campaign yet.', 'Start one from the terminal with `senate mission create`.'], lead_turns: 0 },
    orders: [],
    censor: { state: 'idle', order: null, reviewer: null },
    attention: [],
    curia: { state: 'closed' },
  }),
};

// "story" walks through the states in order, so transitions can be watched:
// a Cohort arrives, works, tests, hands its tablet to the Censor, the tablet
// is sealed and waits for integration, then it is settled.
const STORY = ['quiet', 'one', 'review', 'blocked', 'complete', 'quiet'];
export function storyAt(seconds, stepSeconds = 12) {
  const i = Math.floor(seconds / stepSeconds) % STORY.length;
  return SCENARIOS[STORY[i]]();
}

// Details a panel shows for an Order, in the same shape as `/api/order/:id`.
export function demoOrderDetail(state, id) {
  const o = state.orders.find((x) => x.id === id);
  if (!o) return null;
  return {
    id: o.id,
    title: o.title,
    state: o.state,
    goal: {
      'o-coupon': 'A coupon code can no longer take 100% off an order.',
      'o-double': 'A card is charged once per order, even when the buyer clicks Pay twice.',
    }[o.id] || 'Demonstration Order.',
    acceptance: ['Behaviour covered by tests', 'No change to the payment provider'],
    stages: [
      { kind: 'implementation', label: 'Implementation', status: o.state === 'working' ? 'running' : 'completed', provider: o.worker?.provider ?? null },
      { kind: 'verify', label: 'Verification', status: o.activity === 'testing' ? 'running' : o.state === 'working' ? 'pending' : 'completed', provider: null },
      { kind: 'independent_review', label: 'Independent review', status: o.state === 'in_review' ? 'running' : o.state === 'delivered' ? 'completed' : 'pending', provider: 'claude' },
    ],
    review: o.state === 'delivered' ? { status: 'completed', bottom_line: 'Approve. A second click on Pay reuses the first payment; one naming nit, not blocking.' } : null,
    verification: o.state === 'delivered' ? { status: 'completed', bottom_line: 'npm test: 412 passed.' } : null,
    reason: o.reason,
    demo: true,
  };
}
