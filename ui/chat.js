// Fleet-51 chat UI: bots in a sidebar, one conversation at a time, all bots working at once.
'use strict';

const $ = (s, r = document) => r.querySelector(s);
const el = (tag, attrs = {}, ...kids) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k === 'text') n.textContent = v;
    else if (k.startsWith('on')) n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k != null) n.append(k instanceof Node ? k : document.createTextNode(String(k)));
  return n;
};
const store = {
  get(k, d) { try { const v = localStorage.getItem('fleet51.' + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem('fleet51.' + k, JSON.stringify(v)); } catch { /* storage unavailable */ } },
};

// ---- avatars --------------------------------------------------------------
const COLORS = { white: '#f2f2f2', brown: '#a8743f', red: '#f0283a', orange: '#ff6a13', amber: '#f59e0b', green: '#16c172', teal: '#14b8a6', blue: '#1a84ff', purple: '#8b5cf6', pink: '#f23aa0', gray: '#8a8a8a' };
// Each shape is a polar outline r(θ) on a 100×100 box, or a superellipse.
const SHAPES = {
  circle: (t) => 40,
  cloud: (t) => 36 + 6 * Math.cos(3 * t + 0.5),
  squircle: null,
  sparkle: (t) => 30 + 14 * Math.cos(4 * t),
  flower: (t) => 36 + 5 * Math.cos(6 * t),
  clover: (t) => 34 + 8 * Math.cos(4 * t + Math.PI / 4 * 4),
  pebble: (t) => 34 / Math.sqrt(Math.pow(Math.cos(t + 0.3) / 1.25, 2) + Math.pow(Math.sin(t + 0.3), 2)),
  star: (t) => 32 + 9 * Math.cos(5 * t - Math.PI / 2),
  drop: (t) => 33 * (1 + 0.28 * Math.cos(t + Math.PI * 0.75)),
  gem: null,
  blossom: (t) => 34 + 7 * Math.cos(5 * t + Math.PI / 2),
  burst: (t) => 35 + 5 * Math.cos(8 * t),
  pillow: null,
};
function superellipse(a, b, n, rot = 0) {
  const pts = [];
  for (let i = 0; i < 120; i++) {
    const t = (i / 120) * Math.PI * 2;
    const c = Math.cos(t), s = Math.sin(t);
    let x = a * Math.sign(c) * Math.pow(Math.abs(c), 2 / n);
    let y = b * Math.sign(s) * Math.pow(Math.abs(s), 2 / n);
    [x, y] = [x * Math.cos(rot) - y * Math.sin(rot), x * Math.sin(rot) + y * Math.cos(rot)];
    pts.push([50 + x, 50 + y]);
  }
  return pts;
}
function outline(shape) {
  if (shape === 'squircle') return superellipse(38, 38, 4.5);
  if (shape === 'gem') return superellipse(36, 36, 3.2, Math.PI / 4 + 0.15);
  if (shape === 'pillow') return superellipse(42, 32, 4, -0.12);
  const f = SHAPES[shape] || SHAPES.circle;
  const pts = [];
  for (let i = 0; i < 160; i++) {
    const t = (i / 160) * Math.PI * 2;
    const r = f(t);
    pts.push([50 + r * Math.cos(t), 50 + r * Math.sin(t)]);
  }
  return pts;
}
function avatar(shape, color, size = 48) {
  const NS = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 100 100');
  svg.setAttribute('width', size);
  svg.setAttribute('height', size);
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(NS, 'path');
  path.setAttribute('d', 'M' + outline(shape).map((p) => p.map((v) => v.toFixed(1)).join(' ')).join('L') + 'Z');
  path.setAttribute('fill', COLORS[color] || COLORS.blue);
  svg.append(path);
  for (const [x, rot] of [[42, -12], [56, -8]]) {
    const eye = document.createElementNS(NS, 'rect');
    Object.entries({ x: x - 3.5, y: 36, width: 7, height: 15, rx: 3.5, fill: 'rgba(0,0,0,.78)', transform: `rotate(${rot} ${x} 43)` }).forEach(([k, v]) => eye.setAttribute(k, v));
    svg.append(eye);
  }
  return svg;
}

// ---- api --------------------------------------------------------------------
let token = new URLSearchParams(location.search).get('token') || store.get('token', '');
if (new URLSearchParams(location.search).has('token')) { store.set('token', token); history.replaceState(null, '', location.pathname); }
async function api(path, body) {
  const res = await fetch(path, { method: body ? 'POST' : 'GET', headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' }, body: body ? JSON.stringify(body) : undefined });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) { showGate(); throw new Error('token required'); }
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}
function showGate() {
  $('#token-gate').hidden = false;
  $('#token-save').onclick = () => { token = $('#token-input').value.trim(); store.set('token', token); location.reload(); };
}

// ---- state -------------------------------------------------------------------
let bots = [];
let current = store.get('bot', null);
let installed = [];
let suggested = [];
const pulls = new Map();
let runtime = { state: 'checking' };
let defaultModel = 'qwen3:1.7b';
let lastSeq = 0;
let prompts = new Map();
let typingEl = null;

function toast(msg) {
  const t = el('div', { class: 'pill', text: msg, style: 'position:fixed;left:50%;bottom:90px;transform:translateX(-50%);z-index:40' });
  document.body.append(t);
  setTimeout(() => t.remove(), 3500);
}

// ---- sidebar -----------------------------------------------------------------
function renderBots() {
  const q = $('#search').value.toLowerCase();
  const list = bots.filter((b) => !q || b.name.toLowerCase().includes(q)).sort((a, b) => b.updated - a.updated);
  $('#bots').replaceChildren(...list.map((b) => el('button', { class: 'bot' + (b.id === current ? ' active' : ''), onclick: () => select(b.id), 'aria-current': b.id === current ? 'true' : null },
    el('span', { class: 'avatar' }, avatar(b.shape, b.color, 48), b.status === 'working' || b.pending ? el('span', { class: 'busy', title: 'Working' }) : null),
    el('span', { class: 'name', text: b.name }),
    el('span', { class: 'preview', text: preview(b) }))));
  $('#empty').hidden = bots.length > 0;
}
function preview(b) {
  if (b.status === 'working') return 'Thinking…';
  return (b.preview || 'Say hello').replace(/\s+/g, ' ');
}

// ---- conversation --------------------------------------------------------------
function bot() { return bots.find((b) => b.id === current); }

// `open` is true when the user picked the bot; on a phone that switches from the list to the chat.
async function select(id, open = true) {
  current = id;
  store.set('bot', id);
  if (open) document.getElementById('app').classList.add('in-chat');
  renderBots();
  renderHead();
  $('#msgs').replaceChildren();
  lastSeq = 0;
  prompts = new Map();
  typingEl = null;
  const b = bot();
  if (!b) return;
  try {
    for (const e of await api(`/api/sessions/${id}/history?since=0`)) render(e);
  } catch (e) { toast(e.message); }
  if (!$('#msgs').children.length) hello(b);
  updateTyping();
  $('#input').focus();
}

function hello(b) {
  $('#msgs').append(el('div', { class: 'hello' }, el('div', { class: 'big-avatar' }, avatar(b.shape, b.color, 120)), el('div', { text: `Hi, I'm ${b.name}. Ask me anything.` }), el('div', { class: 'muted', text: `Running ${b.model || 'a local model'} on this computer` })));
}

function renderHead() {
  const b = bot();
  $('#head-avatar').replaceChildren(b ? avatar(b.shape, b.color, 30) : '');
  $('#head-name').textContent = b ? b.name : '';
  $('#input').placeholder = b ? `Message ${b.name}` : 'Message';
  renderModelSelect();
}

function renderModelSelect() {
  const b = bot();
  const sel = $('#model-select');
  const names = [...new Set([...(installed.map((m) => m.name)), ...(b && b.model ? [b.model] : [])])];
  sel.replaceChildren(...names.map((n) => {
    const p = pulls.get(n);
    const have = installed.some((m) => m.name === n);
    const label = have ? n : p && !p.done ? `${n} (downloading ${pct(p)}%)` : `${n} (not downloaded)`;
    return el('option', { value: n, selected: b && b.model === n }, label);
  }), el('option', { value: '__more' }, 'Get more models…'));
  sel.disabled = !b;
}

function scrollDown(force) {
  const m = $('#msgs');
  if (force || m.scrollHeight - m.scrollTop - m.clientHeight < 120) m.scrollTop = m.scrollHeight;
}

// A small, safe Markdown subset: paragraphs, lists, **bold**, `code` and ``` blocks.
function markdown(text) {
  const frag = document.createDocumentFragment();
  const parts = text.split(/```[^\n]*\n?/);
  parts.forEach((part, i) => {
    if (i % 2 === 1) { frag.append(el('pre', {}, el('code', { text: part.replace(/\n$/, '') }))); return; }
    let list = null;
    for (const block of part.split(/\n{2,}/)) {
      const lines = block.split('\n').filter((l) => l.trim());
      if (!lines.length) continue;
      if (lines.every((l) => /^\s*([-*]|\d+\.)\s+/.test(l))) {
        list = el('ul');
        for (const l of lines) list.append(el('li', {}, inline(l.replace(/^\s*([-*]|\d+\.)\s+/, ''))));
        frag.append(list);
      } else {
        frag.append(el('p', {}, inline(lines.join('\n'))));
      }
    }
  });
  return frag;
}
function inline(s) {
  const out = [];
  const re = /(\*\*[^*]+\*\*|`[^`]+`)/g;
  let last = 0, m;
  while ((m = re.exec(s))) {
    if (m.index > last) out.push(s.slice(last, m.index));
    out.push(m[0].startsWith('**') ? el('b', { text: m[0].slice(2, -2) }) : el('code', { text: m[0].slice(1, -1) }));
    last = m.index + m[0].length;
  }
  out.push(s.slice(last));
  return out;
}

function add(node) {
  const m = $('#msgs');
  m.querySelector('.hello')?.remove();
  if (typingEl) m.insertBefore(node, typingEl); else m.append(node);
  scrollDown();
}

function render(e) {
  if (e.seq <= lastSeq) return;
  lastSeq = e.seq;
  switch (e.type) {
    case 'prompt_queued': {
      const st = el('span', { class: 'st' });
      add(el('div', { class: 'm-user' }, e.text, st));
      prompts.set(e.id, st);
      scrollDown(true);
      break;
    }
    case 'text': add(el('div', { class: 'm-bot' }, markdown(e.text))); break;
    case 'tool_call': {
      const a = e.input || {};
      const what = a.path || a.file_path || a.command || a.pattern || '';
      add(el('div', { class: 'm-tool', text: `Used ${e.name.replace(/_/g, ' ')}${what ? ' · ' + String(what).slice(0, 80) : ''}` }));
      break;
    }
    case 'turn_done': if (!e.ok && e.summary) add(el('div', { class: 'm-error', text: friendlyError(e.summary) })); break;
    case 'note': if (e.level === 'error' || e.level === 'warn') add(el('div', { class: 'm-error', text: e.message })); break;
    case 'model_changed': add(el('div', { class: 'm-sys', text: `Switched to ${e.model}` })); break;
  }
}
function friendlyError(msg) {
  if (/not found|pull/i.test(msg)) return `This model isn't downloaded yet. Pick another from the menu above, or download it in Models. (${msg})`;
  if (/cannot reach|connection/i.test(msg)) return 'The model engine is not running yet. It should start on its own in a moment.';
  return msg;
}

function updateTyping() {
  const b = bot();
  const busy = b && (b.status === 'working' || b.pending > 0);
  if (busy && !typingEl) {
    typingEl = el('div', { class: 'typing', 'aria-label': 'Thinking' }, el('span'), el('span'), el('span'));
    $('#msgs').append(typingEl);
    scrollDown();
  } else if (!busy && typingEl) {
    typingEl.remove();
    typingEl = null;
  }
}

async function send(ev) {
  ev.preventDefault();
  const text = $('#input').value.trim();
  const b = bot();
  if (!text || !b) return;
  $('#input').value = '';
  autosize();
  try { await api(`/api/sessions/${b.id}/prompt`, { text }); } catch (e) { toast(e.message); $('#input').value = text; }
}
function autosize() { const t = $('#input'); t.style.height = 'auto'; t.style.height = Math.min(t.scrollHeight, 200) + 'px'; }

// ---- new bot ---------------------------------------------------------------------
let shapes = Object.keys(SHAPES);
let colorNames = Object.keys(COLORS);
const nb = { shape: 'cloud', color: 'blue' };
function openNewBot() {
  nb.shape = shapes[Math.floor(Math.random() * shapes.length)];
  nb.color = colorNames.filter((c) => c !== 'white' && c !== 'gray')[Math.floor(Math.random() * 9)];
  $('#nb-name').value = '';
  drawNewBot();
  $('#new-bot').showModal();
  $('#nb-name').focus();
}
function drawNewBot() {
  $('#nb-preview').replaceChildren(avatar(nb.shape, nb.color, 120));
  $('#nb-shapes').replaceChildren(...shapes.map((s) => el('button', { type: 'button', role: 'radio', 'aria-checked': String(s === nb.shape), 'aria-label': s, onclick: () => { nb.shape = s; drawNewBot(); } }, avatar(s, nb.color, 48))));
  $('#nb-colors').replaceChildren(...colorNames.map((c) => el('button', { type: 'button', role: 'radio', 'aria-checked': String(c === nb.color), 'aria-label': c, style: `background:${COLORS[c]}`, onclick: () => { nb.color = c; drawNewBot(); } })));
}
async function createBot(ev) {
  ev.preventDefault();
  const name = $('#nb-name').value.trim();
  if (!name) { $('#nb-name').focus(); return; }
  try {
    const b = await api('/api/bots', { name, shape: nb.shape, color: nb.color });
    $('#new-bot').close();
    await refresh();
    select(b.id);
  } catch (e) { toast(e.message); }
}

// ---- models ------------------------------------------------------------------------
function pct(p) { return p.total ? Math.floor((p.completed * 100) / p.total) : 0; }
async function loadModels() {
  try {
    const m = await api('/api/models');
    installed = m.installed || [];
    suggested = m.suggested || [];
    for (const p of m.pulls || []) pulls.set(p.name, p);
  } catch { /* engine not ready yet */ }
  renderModelSelect();
  renderSetup();
}
async function download(name) {
  name = name.trim();
  if (!name) return;
  try { await api('/api/models/pull', { name }); pulls.set(name, { name, status: 'starting', completed: 0, total: 0, done: false }); renderModelsDialog(); } catch (e) { toast(e.message); }
}
function renderModelsDialog() {
  if (!$('#models-dlg').open) return;
  const have = (n) => installed.some((m) => m.name === n || m.name === n + ':latest');
  const row = (name, sub, action) => {
    const p = pulls.get(name);
    return el('div', { class: 'mrow' }, el('b', { text: name }), action, el('div', { class: 'sub', text: p && !p.done ? `${p.status} · ${pct(p)}%` : p && p.error ? `Failed: ${p.error}` : sub }), p && !p.done ? el('div', { class: 'bar' }, el('div', { style: `width:${pct(p)}%` })) : null);
  };
  const custom = el('input', { placeholder: 'Any model from ollama.com/library', 'aria-label': 'Model name' });
  $('#models-body').replaceChildren(
    el('div', { class: 'model-list' }, el('h3', { text: 'On this computer' }),
      installed.length ? installed.map((m) => row(m.name, `${(m.size / 1e9).toFixed(1)} GB${m.tools === false ? ' · limited tool use' : ''}`, el('span', { class: 'muted', text: bot() && bot().model === m.name ? 'in use' : '' }))) : el('div', { class: 'muted', text: 'None yet.' })),
    el('div', { class: 'model-list' }, el('h3', { text: 'Free models to download' }),
      suggested.filter((s) => !have(s.name)).map((s) => {
        const p = pulls.get(s.name);
        return row(s.name, `${s.size} · ${s.note}`, el('button', { type: 'button', class: 'pill', disabled: p && !p.done, onclick: () => download(s.name) }, p && !p.done ? 'Downloading…' : 'Download'));
      })),
    el('div', { class: 'custom-model' }, custom, el('button', { type: 'button', class: 'pill', onclick: () => download(custom.value) }, 'Download')));
}

// ---- first-run setup -------------------------------------------------------------------
function renderSetup() {
  const ready = runtime.state === 'ready';
  const firstPull = pulls.get(defaultModel);
  const needModel = ready && !installed.length;
  const show = !ready || needModel;
  $('#setup').hidden = !show;
  if (!show) return;
  $('#setup-avatar').replaceChildren(avatar('cloud', 'blue', 96));
  const eng = $('#step-engine');
  eng.classList.toggle('done', ready);
  eng.classList.toggle('fail', runtime.state === 'failed');
  const sub = { checking: 'Looking for Ollama…', downloading: `Downloading Ollama ${runtime.total ? Math.round(runtime.completed / 1e6) + ' of ' + Math.round(runtime.total / 1e6) + ' MB' : ''}`, starting: 'Starting…', ready: runtime.source === 'system' ? 'Using the Ollama already on this computer' : 'Ready', failed: runtime.message }[runtime.state] || '';
  $('#engine-sub').textContent = sub;
  $('#engine-bar').style.width = runtime.state === 'downloading' && runtime.total ? `${(runtime.completed * 100) / runtime.total}%` : ready ? '100%' : '0%';
  $('#setup-retry').hidden = runtime.state !== 'failed' && !(firstPull && firstPull.error);
  const mod = $('#step-model');
  mod.classList.toggle('done', installed.length > 0);
  mod.classList.toggle('fail', !!(firstPull && firstPull.error));
  $('#model-sub').textContent = !ready ? `Waiting for the engine (then ${defaultModel}, about 1.4 GB)` : firstPull ? (firstPull.error ? firstPull.error : `${defaultModel} · ${firstPull.status}${firstPull.total ? ' · ' + pct(firstPull) + '%' : ''}`) : `Preparing ${defaultModel}…`;
  $('#model-bar').style.width = firstPull ? `${pct(firstPull)}%` : '0%';
}

// ---- live updates ----------------------------------------------------------------------
async function refresh() {
  bots = await api('/api/bots');
  renderBots();
  if (!bot() && bots.length) select(bots[0].id, false);
  else renderHead();
}
function onState(s) {
  const b = bots.find((x) => x.id === s.id);
  if (!s.profile || s.archived) { if (b && s.archived) refresh(); return; }
  const next = { id: s.id, name: s.profile.name, shape: s.profile.shape, color: s.profile.color, model: s.model, folder: s.cwd, status: s.status, preview: s.last_text, pending: s.pending.length, updated: s.updated, created: s.created };
  if (b) Object.assign(b, next); else bots.push(next);
  renderBots();
  if (s.id === current) { updateTyping(); if (b && b.model !== next.model) renderModelSelect(); }
}

async function boot() {
  if (!token) return showGate();
  try {
    const r = await api('/api/runtime');
    runtime = r.runtime;
    defaultModel = r.default_model;
    await loadModels();
    await refresh();
  } catch { return; }
  if (current && bot()) select(current, false);
  const es = new EventSource(`/api/stream?token=${encodeURIComponent(token)}`);
  es.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.kind === 'event') { if (msg.session === current) { render(msg.event); } return; }
    if (msg.kind === 'runtime') { runtime = msg; renderSetup(); if (msg.state === 'ready') loadModels(); return; }
    if (msg.kind === 'pull') {
      pulls.set(msg.name, msg);
      if (msg.done) { loadModels(); if (msg.error) toast(`${msg.name}: ${msg.error}`); }
      renderSetup(); renderModelSelect(); renderModelsDialog();
      return;
    }
    if (msg.kind === 'state') onState(msg);
  };
  es.onopen = () => { refresh().catch(() => {}); if (current) api(`/api/sessions/${current}/history?since=${lastSeq}`).then((evs) => evs.forEach(render)).catch(() => {}); };
}

// ---- wiring -----------------------------------------------------------------------------------
$('#composer').addEventListener('submit', send);
$('#input').addEventListener('input', autosize);
$('#input').addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); $('#composer').requestSubmit(); } });
$('#btn-new').onclick = openNewBot;
$('#btn-new-empty').onclick = openNewBot;
$('#nb-name').addEventListener('keydown', (e) => { if (e.key === 'Enter') createBot(e); });
$('#nb-create').addEventListener('click', createBot);
$('#btn-search').onclick = () => { const w = $('#search-wrap'); w.hidden = !w.hidden; if (!w.hidden) $('#search').focus(); else { $('#search').value = ''; renderBots(); } };
$('#search').oninput = renderBots;
$('#btn-models').onclick = () => { $('#models-dlg').showModal(); loadModels().then(renderModelsDialog); renderModelsDialog(); };
$('#btn-back').onclick = () => document.getElementById('app').classList.remove('in-chat');
$('#btn-menu').onclick = () => { $('#menu').hidden = !$('#menu').hidden; };
document.addEventListener('click', (e) => { if (!e.target.closest('#menu, #btn-menu')) $('#menu').hidden = true; });
$('#menu-folder').onclick = () => { const b = bot(); if (b) api(`/api/bots/${b.id}/reveal`, {}).catch((e) => toast(e.message)); };
$('#menu-delete').onclick = async () => {
  const b = bot();
  if (!b || !confirm(`Delete ${b.name}? Its files stay in ${b.folder}.`)) return;
  await api(`/api/bots/${b.id}/delete`, {}).catch((e) => toast(e.message));
  current = null;
  $('#msgs').replaceChildren();
  await refresh();
};
$('#model-select').onchange = async (e) => {
  const b = bot();
  const v = e.target.value;
  if (v === '__more') { renderModelSelect(); $('#btn-models').click(); return; }
  if (!b || v === b.model) return;
  try { Object.assign(b, await api(`/api/bots/${b.id}/model`, { model: v })); renderModelSelect(); } catch (err) { toast(err.message); renderModelSelect(); }
};
$('#setup-retry').onclick = () => { const p = pulls.get(defaultModel); if (p && p.error) download(defaultModel); else api('/api/runtime/retry', {}).catch(() => {}); };
document.addEventListener('keydown', (e) => { if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'n' && !e.shiftKey) { e.preventDefault(); openNewBot(); } });
boot();
