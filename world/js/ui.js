// HTML over the world: a quiet heading, the view switch, the interaction
// prompt, and one side panel for anything longer than a label. Long text
// never goes into 3D.

import { askConsul, fetchConsul, fetchOrder, source } from './api.js';
import { escapeHtml, numeral, since } from './director.js';
import { STATE_WORD } from './props.js';

const $ = (sel) => document.querySelector(sel);

export class UI {
  constructor() {
    this.state = null;
    this.panel = $('#panel');
    this.body = $('#panel-body');
    this.prompt = $('#prompt');
    this.open = null;
    this.orderRequest = 0;
    this.orderPending = null;
    this.talkRequest = 0;
    $('#panel-close').addEventListener('click', () => this.close());
    window.addEventListener('keydown', (e) => {
      if (e.code === 'Escape') this.close();
    });
    if (source === 'demo') $('#source').textContent = 'Demonstration campaign';
  }

  setState(state) {
    this.state = state;
    const c = state.campaign;
    $('#campaign-title').textContent = c ? c.title : 'No campaign';
    const chip = $('#campaign-chip');
    if (c) {
      const needs = c.needs_you > 0;
      chip.textContent = needs ? `${c.needs_you} need${c.needs_you === 1 ? 's' : ''} you` : c.active ? `${c.active} active` : c.state === 'completed' ? 'Completed' : 'Quiet';
      chip.className = `chip ${needs ? 'needs' : c.active ? 'active' : ''}`;
      $('#campaign-count').textContent = `${c.settled} / ${c.total} Orders settled`;
    } else {
      chip.textContent = 'At rest';
      chip.className = 'chip';
      $('#campaign-count').textContent = '';
    }
    if (this.open) this.refresh();
  }

  setPrompt(anchor) {
    if (!anchor) {
      this.prompt.hidden = true;
      return;
    }
    this.prompt.hidden = false;
    this.prompt.innerHTML = `<kbd>E</kbd> ${escapeHtml(anchor.prompt)}`;
  }

  show(kind, id, html) {
    this.open = { kind, id };
    this.body.innerHTML = html;
    this.panel.hidden = false;
    this.panel.dataset.kind = kind;
  }

  close() {
    this.open = null;
    this.orderRequest++;
    this.orderPending = null;
    this.talkRequest++;
    this.panel.hidden = true;
  }

  refresh() {
    const { kind, id } = this.open;
    if (kind === 'campaign') this.openCampaign();
    else if (kind === 'censor') this.openCensor();
    else if (kind === 'order') this.openOrder(id, true);
    // The conversation refreshes itself; re-rendering would eat the draft.
  }

  interact(anchor) {
    if (anchor.kind === 'consul') this.openConsul();
    else if (anchor.kind === 'censor') this.openCensor();
    else if (anchor.kind === 'board') this.openCampaign();
    else if (anchor.kind === 'order') this.openOrder(anchor.id);
  }

  openCampaign() {
    const s = this.state;
    const c = s?.campaign;
    if (!c) {
      this.show('campaign', null, `<header><small>CAMPAIGN</small><h2>No campaign</h2></header><p class="muted">Start one from the terminal: <code>senate mission create</code>.</p>`);
      return;
    }
    const rows = s.orders
      .map(
        (o) => `<li class="order ${o.state}" data-order="${escapeHtml(o.id)}">
          <span class="glyph">${glyph(o.state)}</span>
          <span class="name">${escapeHtml(o.title)}</span>
          <span class="st">${STATE_WORD[o.state] || o.state}</span></li>`,
      )
      .join('');
    const attention = s.attention.length
      ? `<h3>Needs your judgment</h3><ul class="attention">${s.attention.map((a) => `<li>${escapeHtml(title(s, a.order))} — ${escapeHtml(a.text)}</li>`).join('')}</ul>`
      : '<p class="muted">Nothing needs your judgment.</p>';
    this.show(
      'campaign',
      null,
      `<header><small>CAMPAIGN</small><h2>${escapeHtml(c.title)}</h2></header>
      <p>${escapeHtml(c.goal || '')}</p>
      <div class="stats"><div><b>${c.settled}/${c.total}</b><span>settled</span></div><div><b>${c.active}</b><span>active</span></div><div><b>${c.needs_you}</b><span>need you</span></div></div>
      ${attention}
      <h3>Orders</h3><ul class="orders">${rows}</ul>`,
    );
    this.body.querySelectorAll('[data-order]').forEach((li) => li.addEventListener('click', () => this.openOrder(li.dataset.order)));
  }

  async openOrder(id, quiet = false) {
    const s = this.state;
    const o = s?.orders.find((x) => x.id === id);
    if (!o) {
      if (this.open?.kind === 'order' && this.open.id === id) this.showOrderUnavailable(id);
      return;
    }
    if (quiet && this.orderPending === id) return;
    const request = ++this.orderRequest;
    this.orderPending = id;
    if (!quiet) this.show('order', id, `<header><small>ORDER</small><h2>${escapeHtml(o.title)}</h2></header><p class="muted">Loading…</p>`);
    let d = null;
    try {
      d = await fetchOrder(s, id);
    } catch (e) {
      d = { error: String(e.message || e) };
    }
    if (request !== this.orderRequest) return;
    this.orderPending = null;
    if (this.open?.kind !== 'order' || this.open.id !== id) return;
    const latest = this.state;
    const currentOrder = latest?.orders.find((x) => x.id === id);
    if (!currentOrder) {
      this.showOrderUnavailable(id);
      return;
    }
    const who = currentOrder.worker ? `${escapeHtml(currentOrder.worker.provider)}${currentOrder.worker.model ? ` · ${escapeHtml(currentOrder.worker.model)}` : ''}` : '—';
    const stages = (d?.stages || [])
      .map((st) => `<li class="stage ${st.status}"><span>${escapeHtml(st.label)}</span><span>${escapeHtml(st.status)}${st.provider ? ` · ${escapeHtml(st.provider)}` : ''}</span></li>`)
      .join('');
    const outcome = (name, x) => (x ? `<h3>${name}</h3><blockquote>${escapeHtml(x.bottom_line || x.status)}</blockquote>` : '');
    this.show(
      'order',
      id,
      `<header><small>COHORT ${numeral(currentOrder.cohort)} · ORDER</small><h2>${escapeHtml(currentOrder.title)}</h2></header>
      <div class="kv"><span>State</span><b class="st-${currentOrder.state}">${STATE_WORD[currentOrder.state] || currentOrder.state}</b>
      <span>Worker</span><b>${who}</b>
      ${currentOrder.since ? `<span>For</span><b>${since(currentOrder.since, Date.parse(latest.at))}</b>` : ''}</div>
      ${currentOrder.reason ? `<div class="reason">${escapeHtml(currentOrder.reason)}</div>` : ''}
      ${d?.goal ? `<h3>Goal</h3><p>${escapeHtml(d.goal)}</p>` : ''}
      ${d?.acceptance?.length ? `<h3>Accepted when</h3><ul>${d.acceptance.map((a) => `<li>${escapeHtml(a)}</li>`).join('')}</ul>` : ''}
      ${stages ? `<h3>Stages</h3><ul class="stages">${stages}</ul>` : ''}
      ${outcome('Verification', d?.verification)}
      ${outcome('Independent review', d?.review)}
      ${d?.error ? `<p class="muted">${escapeHtml(d.error)}</p>` : ''}
      <p class="hint">In the terminal: <code>senate mission show ${escapeHtml(latest.campaign?.id || '')}</code></p>`,
    );
  }

  showOrderUnavailable(id) {
    this.orderRequest++;
    this.orderPending = null;
    this.show(
      'order',
      id,
      '<header><small>ORDER</small><h2>Order unavailable</h2></header><p class="muted">This Order is no longer in the selected campaign.</p>',
    );
  }

  openCensor() {
    const s = this.state;
    const c = s?.censor;
    const o = s?.orders.find((x) => x.id === c?.order);
    const body =
      c?.state === 'reviewing' && o
        ? `<p>The Censor is examining <b>${escapeHtml(o.title)}</b>, submitted by Cohort ${numeral(o.cohort)}.</p>
           <div class="kv"><span>Reviewer</span><b>${escapeHtml(c.reviewer?.provider || '—')}${c.reviewer?.model ? ` · ${escapeHtml(c.reviewer.model)}` : ''}</b></div>
           <p class="muted">The verdict appears here, verbatim, once the review is written. Nothing is summarised in advance.</p>
           <button data-order="${escapeHtml(o.id)}">Open the Order</button>`
        : `<p>Nothing to review.</p><p class="muted">When a Cohort finishes, its work comes here for an independent reading before it can be integrated.</p>`;
    this.show('censor', null, `<header><small>CENSOR</small><h2>Independent review</h2></header>${body}`);
    this.body.querySelector('[data-order]')?.addEventListener('click', (e) => this.openOrder(e.target.dataset.order));
  }

  async openConsul() {
    const s = this.state;
    const said = (s?.consul?.summary || []).map((l) => escapeHtml(l)).join('<br>');
    this.show(
      'consul',
      null,
      `<header><small>CONSUL</small><h2>Engineering lead</h2></header>
      <blockquote class="said">${said || 'Nothing to report.'}</blockquote>
      <div id="talk" class="talk"><p class="muted">Loading the conversation…</p></div>
      <form id="ask"><textarea id="ask-text" rows="2" placeholder="Ask the Consul…"></textarea><button type="submit">Send</button></form>
      <p class="hint">Same conversation as <code>senate mission ask</code>. Plan changes the Consul proposes land only when you apply them.</p>`,
    );
    const form = this.body.querySelector('#ask');
    const text = this.body.querySelector('#ask-text');
    const button = form.querySelector('button');
    let sending = false;
    text.addEventListener('keydown', (e) => {
      e.stopPropagation();
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        form.requestSubmit();
      }
    });
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      if (sending) return;
      const msg = text.value.trim();
      if (!msg) return;
      sending = true;
      text.disabled = true;
      button.disabled = true;
      let r;
      try {
        r = await askConsul(msg);
      } catch (error) {
        r = { accepted: false, reason: String(error.message || error) };
      } finally {
        sending = false;
        text.disabled = false;
        button.disabled = false;
      }
      if (this.body.querySelector('#ask') !== form) return;
      const talk = this.body.querySelector('#talk');
      if (!r.accepted) talk?.insertAdjacentHTML('beforeend', `<p class="muted">${escapeHtml(r.reason || 'Not sent.')}</p>`);
      else {
        text.value = '';
        this.loadTalk();
      }
    });
    this.loadTalk();
  }

  async loadTalk() {
    const request = ++this.talkRequest;
    let log;
    try {
      log = await fetchConsul();
    } catch (e) {
      log = { turns: [], error: String(e.message || e) };
    }
    if (request !== this.talkRequest || this.open?.kind !== 'consul') return;
    const talk = this.body.querySelector('#talk');
    if (!talk) return;
    const turns = (log.turns || [])
      .map((t) => `<div class="turn ${t.role}"><small>${t.role === 'you' ? 'You' : 'Consul · latest answer'}</small>${prose(t.text)}</div>`)
      .join('');
    talk.innerHTML = turns || '<p class="muted">No conversation yet.</p>';
    if (log.busy) talk.insertAdjacentHTML('beforeend', '<p class="muted">The Consul is answering…</p>');
    if (log.error) talk.insertAdjacentHTML('beforeend', `<p class="muted">${escapeHtml(log.error)}</p>`);
    talk.scrollTop = talk.scrollHeight;
    if (log.busy) setTimeout(() => this.open?.kind === 'consul' && this.loadTalk(), 2500);
  }
}

function glyph(state) {
  return { working: '●', in_review: '◆', blocked: '!', delivered: '✓', settled: '✓', failed: '✕', cancelled: '–' }[state] || '○';
}

function title(s, id) {
  return s.orders.find((o) => o.id === id)?.title || id || '';
}

// Just enough Markdown for the lead's prose: headings, bullets, paragraphs,
// inline code. Everything is escaped first; no HTML from the answer survives.
function prose(text) {
  const inline = (s) => escapeHtml(s).replace(/`([^`]+)`/g, '<code>$1</code>');
  const out = [];
  let list = false;
  let para = [];
  const flush = () => {
    if (para.length) out.push(`<p>${inline(para.join(' '))}</p>`);
    para = [];
  };
  for (const raw of String(text).split('\n')) {
    const line = raw.trim();
    const bullet = /^[-*] (.*)/.exec(line);
    if (!bullet && list) {
      out.push('</ul>');
      list = false;
    }
    if (!line) flush();
    else if (/^#{1,6} /.test(line)) {
      flush();
      out.push(`<h4>${inline(line.replace(/^#+ /, ''))}</h4>`);
    } else if (bullet) {
      flush();
      if (!list) out.push('<ul>');
      list = true;
      out.push(`<li>${inline(bullet[1])}</li>`);
    } else para.push(line);
  }
  flush();
  if (list) out.push('</ul>');
  return out.join('');
}
