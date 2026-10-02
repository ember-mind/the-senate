import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
class Element {
  innerHTML = ''; value = ''; disabled = false; hidden = false;
  dataset = {}; listeners = {}; children = new Map();
  addEventListener(name, handler) { this.listeners[name] = handler; }
  querySelector(name) { return this.children.get(name) || null; }
  querySelectorAll() { return []; }
  insertAdjacentHTML(_, html) { this.innerHTML += html; }
}
const nodes = new Map(['#panel', '#panel-body', '#prompt', '#panel-close', '#campaign-title', '#campaign-chip', '#campaign-count'].map(id => [id, new Element()]));
const body = nodes.get('#panel-body');
const orders = [], talks = [], sends = [];
const escapeHtml = s => String(s).replaceAll('&','&amp;').replaceAll('<','&lt;').replaceAll('>','&gt;').replaceAll('"','&quot;');
const apiExports = {
  source: 'live',
  fetchOrder: () => { const d=deferred(); orders.push(d); return d.promise; },
  fetchConsul: () => { const d=deferred(); talks.push(d); return d.promise; },
  askConsul: () => { const d=deferred(); sends.push(d); return d.promise; },
};
const context = vm.createContext({ document: { querySelector: s => nodes.get(s) }, window: { addEventListener() {} }, setTimeout() {}, console });
function synthetic(exports) {
  return new vm.SyntheticModule(Object.keys(exports), function() {
    for (const [key,value] of Object.entries(exports)) this.setExport(key,value);
  }, { context });
}
const module = new vm.SourceTextModule(await readFile(new URL('../world/js/ui.js', import.meta.url),'utf8'), { context });
await module.link(name => {
  if (name === './api.js') return synthetic(apiExports);
  if (name === './director.js') return synthetic({ escapeHtml, numeral: String, since: (start, at) => `${start}@${at}` });
  if (name === './props.js') return synthetic({ STATE_WORD: {} });
  throw new Error(name);
});
await module.evaluate();
const ui = new module.namespace.UI();
ui.state = { orders: [{ id:'A',title:'Order A',cohort:1,state:'ready' }], consul: {}, campaign: {} };
const oldOrder = ui.openOrder('A');
ui.close();
const newOrder = ui.openOrder('A');
await ui.openOrder('A',true);
assert.equal(orders.length,2,'pending refresh must coalesce');
orders[1].resolve({ goal:'fresh goal' }); await newOrder;
orders[0].resolve({ goal:'stale goal' }); await oldOrder;
assert.match(body.innerHTML,/fresh goal/);
assert.doesNotMatch(body.innerHTML,/stale goal/);

const latestOrder = ui.openOrder('A');
ui.setState({
  at: '2026-10-02T00:00:01Z',
  campaign: { id: 'latest-mission', active: 1, needs_you: 0, settled: 0, total: 1 },
  orders: [{ id: 'A', title: 'Latest title', cohort: 2, state: 'working', worker: { provider: 'codex', model: 'latest-model' }, since: 'latest-start' }],
  attention: [],
});
assert.equal(orders.length, 3, 'snapshot refresh must coalesce with the pending request');
orders[2].resolve({ goal: 'detail response' }); await latestOrder;
assert.match(body.innerHTML, /Latest title/);
assert.match(body.innerHTML, /COHORT 2/);
assert.match(body.innerHTML, /st-working/);
assert.doesNotMatch(body.innerHTML, /st-ready/);
assert.match(body.innerHTML, /codex · latest-model/);
assert.match(body.innerHTML, new RegExp(`latest-start@${Date.parse(ui.state.at)}`));
assert.match(body.innerHTML, /senate mission show latest-mission/);

const removedOrder = ui.openOrder('A');
ui.setState({ ...ui.state, orders: [] });
assert.match(body.innerHTML, /Order unavailable/);
assert.equal(ui.orderPending, null);
orders[3].resolve({ goal: 'removed stale goal' }); await removedOrder;
assert.match(body.innerHTML, /Order unavailable/);
assert.doesNotMatch(body.innerHTML, /removed stale goal/);
function conversation() {
  const form = new Element(), text = new Element(), button = new Element(), talk = new Element();
  form.children.set('button',button);
  body.children = new Map([['#ask',form],['#ask-text',text],['#talk',talk]]);
  return { form,text,button,talk };
}
let panel = conversation(); await ui.openConsul();
ui.close(); panel = conversation(); await ui.openConsul();
talks[1].resolve({ turns:[{ role:'consul',text:'new answer' }],busy:false });
await new Promise(setImmediate);
talks[0].resolve({ turns:[{ role:'consul',text:'old answer' }],busy:false });
await new Promise(setImmediate);
assert.match(panel.talk.innerHTML,/new answer/);
assert.doesNotMatch(panel.talk.innerHTML,/old answer/);
panel.text.value='keep this draft';
const submit = panel.form.listeners.submit({ preventDefault() {} });
assert.equal(panel.text.disabled,true);
assert.equal(panel.button.disabled,true);
await panel.form.listeners.submit({ preventDefault() {} });
assert.equal(sends.length,1,'duplicate submission must be suppressed');
sends[0].reject(new Error('server <unavailable>')); await submit;
assert.equal(panel.text.value,'keep this draft');
assert.equal(panel.text.disabled,false);
assert.equal(panel.button.disabled,false);
assert.match(panel.talk.innerHTML,/server &lt;unavailable&gt;/);
const accepted = panel.form.listeners.submit({ preventDefault() {} });
sends[1].resolve({ accepted:true }); await accepted;
assert.equal(panel.text.value,'');
console.log('UI regression scenarios passed: coalescing, latest metadata, removed Orders, stale order/conversation responses, duplicate send, retained draft, escaped failure, accepted draft cleanup.');

let response;
const apiContext = vm.createContext({ URLSearchParams, location: { search:'?mission=selected', hash:'#token=secret', pathname:'/' }, history: { replaceState() {} }, sessionStorage: { getItem() { return null; }, setItem() {} }, window: {}, performance: { now() { return 0; } }, fetch: async (path, options) => { assert.equal(path,'/api/consul?mission=selected'); assert.equal(options.headers['X-Senate-Token'],'secret'); return response; } });
const api = new vm.SourceTextModule(await readFile(new URL('../world/js/api.js', import.meta.url),'utf8'),{ context:apiContext });
await api.link(() => new vm.SyntheticModule(['SCENARIOS','demoOrderDetail','storyAt'],function() { this.setExport('SCENARIOS',{}); this.setExport('demoOrderDetail',()=>{}); this.setExport('storyAt',()=>{}); },{context:apiContext}));
await api.evaluate();
response = { ok:false,status:500,json:async()=>({error:'backend failed'}) };
await assert.rejects(api.namespace.askConsul('hi'),/backend failed/);
response = { ok:true,status:202,json:async()=>({accepted:true}) };
assert.equal((await api.namespace.askConsul('hi')).accepted,true);
console.log('API regression scenarios passed: mission/token forwarding, HTTP error propagation, successful acceptance.');
