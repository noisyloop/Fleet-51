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
    else if (k === 'autofill') continue;
    else n.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k != null) n.append(k instanceof Node ? k : document.createTextNode(String(k)));
  if ((tag === 'input' || tag === 'textarea') && !attrs.autofill) noAutofill(n);
  return n;
};
// No autofill or remembered entries anywhere: what you typed before is never offered again.
function noAutofill(n) {
  n.setAttribute('autocomplete', 'off');
  n.setAttribute('autocorrect', 'off');
  n.setAttribute('autocapitalize', 'off');
  n.setAttribute('data-form-type', 'other');
}
document.querySelectorAll('input, textarea').forEach(noAutofill);
const store = {
  get(k, d) { try { const v = localStorage.getItem('fleet51.' + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem('fleet51.' + k, JSON.stringify(v)); } catch { /* storage unavailable */ } },
};

// ---- avatars --------------------------------------------------------------
const COLORS = { white: '#f2f2f2', brown: '#a8743f', red: '#f0283a', orange: '#ff6a13', amber: '#f59e0b', green: '#3ddc68', teal: '#14b8a6', blue: '#1a84ff', purple: '#8b5cf6', pink: '#f23aa0', gray: '#8a8a8a' };
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
  // Alien eyes: big, dark and slanted, with a small highlight.
  const set = (node, attrs) => { Object.entries(attrs).forEach(([k, v]) => node.setAttribute(k, v)); svg.append(node); };
  for (const [x, rot] of [[38, -32], [62, 32]]) {
    set(document.createElementNS(NS, 'ellipse'), { cx: x, cy: 50, rx: 8.5, ry: 13, transform: `rotate(${rot} ${x} 50)`, fill: '#06120a' });
    set(document.createElementNS(NS, 'ellipse'), { cx: x - 2.5, cy: 45, rx: 2.2, ry: 3.2, transform: `rotate(${rot} ${x} 50)`, fill: 'rgba(255,255,255,.8)' });
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
// A window opened for one bot ("Open in new window") starts on that bot.
let current = new URLSearchParams(location.search).get('bot') || store.get('bot', null);
let installed = [];
let suggested = [];
const pulls = new Map();
let runtime = { state: 'checking' };
let defaultModel = 'qwen3:1.7b';
let lastSeq = 0;
let online = true;
let sandbox = false;
let autos = [];
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
  $('#bots').replaceChildren(...list.map((b) => el('button', { class: 'bot' + (b.id === current ? ' active' : ''), onclick: () => select(b.id), oncontextmenu: (e) => openCtx(e, b), 'aria-current': b.id === current ? 'true' : null },
    el('span', { class: 'avatar' }, avatar(b.shape, b.color, 48), b.status === 'working' || b.pending ? el('span', { class: 'busy', title: 'Working' }) : null),
    el('span', { class: 'name', text: b.name }),
    el('span', { class: 'preview', text: preview(b) }))));
  $('#empty').hidden = bots.length > 0;
}
function preview(b) {
  if (b.status === 'working') return 'Thinking…';
  return (b.preview || 'Say hello').replace(/\s+/g, ' ');
}

// Right-click (or long-press) on a bot.
let ctxBot = null;
function openCtx(e, b) {
  e.preventDefault();
  ctxBot = b;
  const m = $('#ctx');
  m.hidden = false;
  const x = Math.min(e.clientX, innerWidth - m.offsetWidth - 8);
  const y = Math.min(e.clientY, innerHeight - m.offsetHeight - 8);
  m.style.left = `${Math.max(8, x)}px`;
  m.style.top = `${Math.max(8, y)}px`;
  m.querySelector('button').focus();
}
async function clearChat(b) {
  if (!b || !confirm(`Clear your chat with ${b.name}? The messages are removed and ${b.name} starts fresh. Files and automations stay.`)) return;
  try { await api(`/api/bots/${b.id}/clear`, {}); } catch (e) { toast(e.message); }
}
async function deleteBot(b) {
  if (!b || !confirm(`Delete ${b.name}? Its chat and automations are removed. Its files stay in ${b.folder}.`)) return;
  await api(`/api/bots/${b.id}/delete`, {}).catch((e) => toast(e.message));
  if (current === b.id) { current = null; $('#msgs').replaceChildren(); }
  await refresh();
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
  // Files added for another bot went to that bot's folder.
  attaching = [];
  renderPending();
  const b = bot();
  if (!b) return;
  try {
    for (const e of await api(`/api/bots/${id}/messages?since=0`)) render(e);
  } catch (e) { toast(e.message); }
  if (!$('#msgs').children.length) hello(b);
  updateTyping();
  $('#input').focus();
}

function hello(b) {
  $('#msgs').append(el('div', { class: 'hello' }, el('div', { class: 'big-avatar' }, avatar(b.shape, b.color, 120)), el('div', { text: `Hi, I'm ${b.name}. Ask me anything.` }), el('div', { class: 'muted', text: `Running ${b.model || 'a local model'} on this computer` })));
}

function renderThink() {
  const b = bot();
  const t = $('#btn-think');
  t.hidden = !b;
  if (!b) return;
  t.setAttribute('aria-pressed', String(!!b.think));
  t.textContent = b.think ? 'Think: on' : 'Think: off';
  t.title = b.think ? 'The model reasons before answering (shown in the chat). Slower.' : 'Faster replies without reasoning.';
}

function renderCode() {
  const b = bot();
  const c = $('#btn-code');
  c.hidden = !b || !sandbox;
  if (c.hidden) return;
  c.setAttribute('aria-pressed', String(!!b.run_code));
  c.textContent = b.run_code ? 'Run code: on' : 'Run code: off';
  c.title = b.run_code ? 'This bot can run code in a sandbox: no internet, only its own folder, 60 seconds at most.' : 'This bot cannot run programs. Turn on to let it test the code it writes, inside a sandbox.';
}
async function toggleCode() {
  const b = bot();
  if (!b) return;
  if (!b.run_code && !confirm(`Let ${b.name} run code?\n\nIt runs in a sandbox: no internet, it can only change files in its own folder (${b.folder}), the rest of your home folder is hidden, and each run stops after 60 seconds. You can undo its file changes.`)) return;
  try { Object.assign(b, await api(`/api/bots/${b.id}/run-code`, { run_code: !b.run_code })); renderCode(); } catch (e) { toast(e.message); }
}

function renderHead() {
  renderThink();
  renderCode();
  const b = bot();
  $('#head-avatar').replaceChildren(b ? avatar(b.shape, b.color, 30) : '');
  $('#head-name').textContent = b ? b.name : '';
  $('#input').placeholder = b ? `Message ${b.name}` : 'Message';
  renderModelSelect();
}

const CLOUD_NOTE = '☁ Cloud model: runs on Ollama\'s servers, so your messages leave this computer and it needs the internet';
function isCloud(name) { return installed.some((m) => m.name === name && m.cloud); }
function renderCloud() {
  const b = bot();
  const c = $('#cloud-note');
  c.hidden = !b || !isCloud(b.model);
  c.title = CLOUD_NOTE;
}

function renderModelSelect() {
  renderCloud();
  const b = bot();
  const sel = $('#model-select');
  const names = [...new Set([...(installed.map((m) => m.name)), ...(b && b.model ? [b.model] : [])])];
  sel.replaceChildren(...names.map((n) => {
    const p = pulls.get(n);
    const have = installed.some((m) => m.name === n);
    const cloud = installed.some((m) => m.name === n && m.cloud);
    const label = have ? (cloud ? `${n} ☁ cloud` : n) : p && !p.done ? `${n} (downloading ${pct(p)}%)` : `${n} (not downloaded)`;
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
      const files = (e.attachments || []).length ? el('div', { class: 'files' }, e.attachments.map((n) => el('span', { class: 'chip', title: 'Show in Finder', onclick: () => reveal(n) }, el('span', { text: `📎 ${n}` })))) : null;
      add(el('div', { class: 'm-user' }, e.automation ? el('span', { class: 'auto-tag' }, '⚡ ' + e.automation) : null, files, e.text, st));
      prompts.set(e.id, st);
      scrollDown(true);
      break;
    }
    case 'reasoning': add(el('details', { class: 'm-reason' }, el('summary', { text: 'Reasoning' }), el('div', { class: 'm-reason-body', text: e.text }))); break;
    case 'text': add(el('div', { class: 'm-bot' }, markdown(e.text))); break;
    case 'checkpoint': {
      const btn = el('button', { type: 'button', 'data-checkpoint': e.id, onclick: () => undo(e.id, btn) }, 'Undo');
      add(el('div', { class: 'm-undo' }, el('span', { text: 'Files changed in this reply' }), btn));
      break;
    }
    case 'undone': {
      const b = document.querySelector(`[data-checkpoint="${CSS.escape(e.id)}"]`);
      if (b) { b.disabled = true; b.textContent = 'Undone'; }
      add(el('div', { class: 'm-sys', text: 'Files put back to how they were before that reply' }));
      break;
    }
    case 'cleared': $('#msgs').replaceChildren(); typingEl = null; { const b = bot(); if (b) hello(b); } break;
    case 'tool_call': {
      const a = e.input || {};
      if (e.name === 'run_command') { add(el('div', { class: 'm-tool' }, 'Ran ', el('code', { text: String(a.command || '').slice(0, 120) }))); break; }
      if ((e.name === 'write_file' || e.name === 'edit_file') && a.path) {
        add(el('div', { class: 'm-file' }, `${e.name === 'write_file' ? 'Wrote' : 'Edited'} `, el('code', { text: String(a.path).slice(0, 80) }), el('button', { type: 'button', onclick: () => reveal(String(a.path)) }, 'Show in Finder')));
        break;
      }
      const what = a.path || a.file_path || a.command || a.pattern || '';
      add(el('div', { class: 'm-tool', text: `Used ${e.name.replace(/_/g, ' ')}${what ? ' · ' + String(what).slice(0, 80) : ''}` }));
      break;
    }
    case 'turn_done': if (!e.ok && e.summary) add(el('div', { class: 'm-error', text: friendlyError(e.summary) })); break;
    case 'note': if (e.level === 'error' || e.level === 'warn') add(el('div', { class: 'm-error', text: e.message })); break;
    case 'model_changed': add(el('div', { class: 'm-sys', text: `Switched to ${e.model}` })); break;
    case 'run_code_changed': add(el('div', { class: 'm-sys', text: e.run_code ? 'Running code on: only inside a sandbox, in this bot\'s folder' : 'Running code off' })); break;
    case 'think_changed': add(el('div', { class: 'm-sys', text: e.think ? 'Thinking on: replies show their reasoning' : 'Thinking off: faster replies' })); break;
  }
}
async function undo(id, btn) {
  const b = bot();
  if (!b || !confirm(`Put ${b.name}'s folder back to how it was before this reply? Changes from later replies are undone too.`)) return;
  btn.disabled = true;
  try { await api(`/api/bots/${b.id}/undo`, { checkpoint: id }); } catch (e) { btn.disabled = false; toast(e.message); }
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
  if (attaching.some((a) => a.busy)) { toast('Wait for the files to finish adding'); return; }
  const attachments = attaching.filter((a) => a.name).map((a) => a.name);
  if ((!text && !attachments.length) || !b) return;
  $('#input').value = '';
  autosize();
  const kept = attaching;
  attaching = [];
  renderPending();
  try { await api(`/api/bots/${b.id}/messages`, { text, attachments }); } catch (e) { toast(e.message); $('#input').value = text; attaching = kept; renderPending(); }
}

// ---- adding files ------------------------------------------------------------------
// Files are copied into the bot's folder as soon as they are picked or dropped, then sent along
// with the next message.
let attaching = [];
const FILE_ICON = { image: '🖼', pdf: '📄', word: '📄', text: '📝', other: '📎' };
function renderPending() {
  const p = $('#pending');
  p.hidden = !attaching.length;
  p.replaceChildren(...attaching.map((a) => el('span', { class: 'chip' + (a.busy ? ' busy' : '') + (a.note ? ' warn' : ''), title: a.note || (a.text_file ? `The bot reads its text from ${a.text_file}` : a.name || a.label) },
    el('span', { text: `${FILE_ICON[a.kind] || '📎'} ${a.name || a.label}${a.busy ? ' · adding…' : ''}` }),
    el('button', { type: 'button', 'aria-label': `Remove ${a.name || a.label}`, onclick: () => { attaching = attaching.filter((x) => x !== a); renderPending(); } }, '×'))));
}
async function addFiles(list) {
  const b = bot();
  if (!b) return;
  for (const f of list) {
    if (f.size > 50 * 1024 * 1024) { toast(`${f.name} is too big (the limit is 50 MB)`); continue; }
    const a = { label: f.name, busy: true };
    attaching.push(a);
    renderPending();
    try {
      const res = await fetch(`/api/bots/${b.id}/files?name=${encodeURIComponent(f.name)}`, { method: 'POST', headers: { Authorization: 'Bearer ' + token }, body: f });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(data.error || res.statusText);
      Object.assign(a, data, { busy: false });
      if (data.kind === 'image' && !isVision(b.model)) toast(`${b.model} can't see images. Try a model that can, like gemma4 or qwen2.5vl (Models).`);
      if (data.note) toast(`${data.name}: ${data.note}`);
    } catch (e) {
      attaching = attaching.filter((x) => x !== a);
      toast(`${f.name}: ${e.message}`);
    }
    renderPending();
  }
}
function isVision(name) { return installed.some((m) => m.name === name && m.vision); }
function reveal(path) { const b = bot(); if (b) api(`/api/bots/${b.id}/reveal`, path ? { path } : {}).catch((e) => toast(e.message)); }
function autosize() { const t = $('#input'); t.style.height = 'auto'; t.style.height = Math.min(t.scrollHeight, 200) + 'px'; }

// ---- new bot ---------------------------------------------------------------------
let shapes = Object.keys(SHAPES);
let colorNames = Object.keys(COLORS);
const nb = { shape: 'cloud', color: 'green' };
function openNewBot() {
  nb.shape = shapes[Math.floor(Math.random() * shapes.length)];
  nb.color = 'green';
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
// Models dialog: a find box that filters both lists as you type, and downloads by exact name.
let modelQuery = '';
const modelLists = el('div', { class: 'model-lists' });
const modelFind = el('input', { type: 'text', placeholder: 'Find a model, or type a name like qwen3:8b', 'aria-label': 'Find a model', spellcheck: 'false', oninput: (e) => { modelQuery = e.target.value.trim(); renderModelsDialog(); }, onkeydown: (e) => { if (e.key === 'Enter') { e.preventDefault(); if (modelQuery) download(modelQuery); } } });
function renderModelsDialog() {
  if (!$('#models-dlg').open) return;
  const q = modelQuery.toLowerCase();
  const match = (s) => !q || s.toLowerCase().includes(q);
  const have = (n) => installed.some((m) => m.name === n || m.name === n + ':latest');
  const row = (name, sub, action) => {
    const p = pulls.get(name);
    return el('div', { class: 'mrow' }, el('b', { text: name }), action, el('div', { class: 'sub', text: p && !p.done ? `${p.status} · ${pct(p)}%` : p && p.error ? `Failed: ${p.error}` : sub }), p && !p.done ? el('div', { class: 'bar' }, el('div', { style: `width:${pct(p)}%` })) : null);
  };
  const shown = suggested.filter((s) => !have(s.name) && match(`${s.name} ${s.note}`));
  const exact = q && !installed.some((m) => m.name === modelQuery) && !suggested.some((s) => s.name === modelQuery);
  const valid = /^[a-z0-9][a-z0-9._\/-]*(:[a-z0-9._-]+)?$/i.test(modelQuery);
  // Offer "download this exact name" when it looks like a real name, not a search word.
  const byName = exact && valid && (!shown.length || modelQuery.includes(':')) ? el('div', { class: 'mrow' }, el('b', { text: modelQuery }),
    el('button', { type: 'button', class: 'pill', disabled: !online, onclick: () => download(modelQuery) }, 'Download'),
    el('div', { class: 'sub', text: /cloud/.test(modelQuery) ? '☁ A cloud model: it runs on Ollama\'s servers, so chats with it leave this computer.' : 'Downloads this exact name from Ollama. Check its size first: some models are hundreds of GB.' })) : null;
  const mine = installed.filter((m) => match(`${m.name}${m.vision ? ' sees images vision' : ''}${m.cloud ? ' cloud' : ''}`));
  // The find box stays put (re-rendering it would lose focus); only the lists below change.
  if (!modelFind.isConnected) $('#models-body').replaceChildren(el('div', { class: 'custom-model' }, modelFind), modelLists);
  modelLists.replaceChildren(...[
    online ? null : el('div', { class: 'offline-note', text: "You're offline. Your downloaded models still work; downloading new ones needs the internet." }),
    byName,
    el('div', { class: 'model-list' }, el('h3', { text: 'On this computer' }),
      mine.length ? mine.map((m) => row(m.name, m.cloud ? CLOUD_NOTE : `${(m.size / 1e9).toFixed(1)} GB${m.vision ? ' · sees images' : ''}${m.tools === false ? ' · limited tool use' : ''}`, el('span', { class: 'muted', text: bot() && bot().model === m.name ? 'in use' : '' }))) : el('div', { class: 'muted', text: q && installed.length ? 'No downloaded model matches.' : 'None yet.' })),
    el('div', { class: 'model-list' }, el('h3', { text: 'Free models to download' }),
      shown.length ? shown.map((s) => {
        const p = pulls.get(s.name);
        return row(s.name, `${s.size} · ${s.note}`, el('button', { type: 'button', class: 'pill', disabled: (p && !p.done) || !online, onclick: () => download(s.name) }, p && !p.done ? 'Downloading…' : 'Download'));
      }) : el('div', { class: 'muted', text: q ? 'No suggested model matches.' : 'You have them all.' })),
    el('a', { class: 'browse', href: `https://ollama.com/search${q ? '?q=' + encodeURIComponent(modelQuery) : ''}`, target: '_blank', rel: 'noopener' }, `Browse all models on ollama.com${q ? ` for “${modelQuery}”` : ''} ↗`)].filter(Boolean));
}

function setOnline(v) {
  online = v;
  $('#net').hidden = online;
  renderModelsDialog();
}

// ---- automations -------------------------------------------------------------------------
const TRIGGERS = ['once', 'hourly', 'daily', 'weekdays', 'weekly', 'monthly', 'annually', 'custom'];
const DAY = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
const MONTH = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];
const two = (n) => String(n).padStart(2, '0');
const ymd = (d) => `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())}`;
function ordinal(n) {
  if (n % 100 >= 11 && n % 100 <= 13) return n + 'th';
  return n + ({ 1: 'st', 2: 'nd', 3: 'rd' }[n % 10] || 'th');
}
const cap = (s) => s[0].toUpperCase() + s.slice(1);
function listWords(xs) { return xs.length < 2 ? xs.join('') : xs.slice(0, -1).join(', ') + ' and ' + xs[xs.length - 1]; }

// What a schedule means, in words ("Every weekday at 08:30").
function describe(s) {
  const t = s.times[0] || '';
  const [, mm, dd] = (s.date || '2000-01-01').split('-').map(Number);
  switch (s.repeat) {
    case 'once': {
      const d = s.date ? new Date(s.date + 'T00:00') : new Date();
      return `Only one time on ${MONTH[d.getMonth()].slice(0, 3)} ${d.getDate()}, ${d.getFullYear()} at ${t}`;
    }
    case 'hourly': return `Every hour from ${two(s.from_hour ?? 9)}:${t.slice(3) || '00'} to ${two(s.to_hour ?? 17)}:${t.slice(3) || '00'}`;
    case 'daily': return `Every day at ${t}`;
    case 'weekdays': return `Every weekday at ${t}`;
    case 'weekly': return `Every week on ${listWords(s.days.map((d) => DAY[d]))} at ${t}`;
    case 'monthly': return `On the ${ordinal(dd)} of each month at ${t}`;
    case 'annually': return `Every ${MONTH[mm - 1]} ${dd} at ${t}`;
    case 'custom': return s.days.length && s.times.length ? `${s.days.length === 7 ? 'Every day' : listWords(s.days.map((d) => DAY[d].slice(0, 3)))} at ${listWords([...new Set(s.times)].sort())}` : 'Pick days and times';
  }
  return '';
}

let draft = null; // the automation being edited
// A new time an hour after the last one, so "Add time" never repeats a time.
function laterTime(t = '08:00') {
  const [h, m] = t.split(':').map(Number);
  return `${two((h + 1) % 24)}:${two(m || 0)}`;
}
function defaultSchedule(repeat, from) {
  const soon = new Date(Date.now() + 5 * 60000);
  soon.setMinutes(Math.ceil(soon.getMinutes() / 5) * 5);
  const time = (from && from.times[0]) || `${two(soon.getHours())}:${two(soon.getMinutes())}`;
  const s = { repeat, times: [time], date: null, days: [], from_hour: 9, to_hour: 17 };
  if (repeat === 'once') s.date = ymd(soon);
  if (repeat === 'weekly' || repeat === 'custom') s.days = [new Date().getDay()];
  if (repeat === 'monthly') s.date = '2000-01-01';
  if (repeat === 'annually') s.date = '2000-01-01';
  if (repeat === 'custom' && from) s.times = [...from.times];
  return s;
}

function openAuto(a, botId) {
  if (!bots.length) { toast('Create a bot first'); return; }
  draft = a ? JSON.parse(JSON.stringify(a)) : { id: null, bot: botId || current || bots[0].id, name: '', instructions: '', schedule: defaultSchedule('daily'), notify: true, enabled: true };
  if (!bots.some((b) => b.id === draft.bot)) draft.bot = bots[0].id;
  $('#auto-title').textContent = a ? 'Edit Automation' : 'New Automation';
  $('#auto-save').textContent = a ? 'Save' : 'Create';
  $('#auto-name').value = draft.name;
  $('#auto-text').value = draft.instructions;
  $('#auto-notify').checked = draft.notify;
  $('#auto-actions').hidden = !a;
  drawHistory();
  $('#trig-menu').hidden = true;
  drawAuto();
  if ($('#autos-dlg').open) $('#autos-dlg').close();
  $('#auto-dlg').showModal();
  $('#auto-name').focus();
}

function drawAuto() {
  const s = draft.schedule;
  $('#auto-bots').replaceChildren(...bots.map((b) => el('button', { type: 'button', role: 'radio', 'aria-checked': String(b.id === draft.bot), title: `${b.name} (${b.model || 'no model'})`, onclick: () => { draft.bot = b.id; drawAuto(); } }, avatar(b.shape, b.color, 40), el('span', { text: b.name }))));
  $('#trig-name').textContent = cap(s.repeat);
  $('#trig-desc').textContent = describe(s);
  const set = (k, v) => { s[k] = v; drawAuto(); };
  const time = (i = 0) => el('input', { type: 'time', value: s.times[i] || '', 'aria-label': 'Time', onchange: (e) => { s.times[i] = e.target.value; drawAuto(); } });
  const dayPick = (multi) => el('div', { class: 'days', role: 'group', 'aria-label': 'Days' }, DAY.map((name, d) => el('button', { type: 'button', 'aria-pressed': String(s.days.includes(d)), 'aria-label': name, title: name, onclick: () => {
    if (multi) set('days', s.days.includes(d) ? s.days.filter((x) => x !== d) : [...s.days, d].sort());
    else set('days', [d]);
  } }, name[0])));
  const [yy, mm, dd] = (s.date || '2000-01-01').split('-');
  const hourSel = (k, label) => el('label', {}, label, el('select', { onchange: (e) => set(k, Number(e.target.value)) }, Array.from({ length: 24 }, (_, h) => el('option', { value: h, selected: (s[k] ?? (k === 'from_hour' ? 9 : 17)) === h }, `${two(h)}:00`))));
  const parts = {
    once: () => [el('label', {}, 'Date', el('input', { type: 'date', value: s.date || '', min: ymd(new Date()), onchange: (e) => set('date', e.target.value) })), el('label', {}, 'Time', time())],
    hourly: () => [hourSel('from_hour', 'From'), hourSel('to_hour', 'to'), el('label', {}, 'at minute', el('input', { type: 'number', min: 0, max: 59, value: Number((s.times[0] || '00:00').slice(3)), onchange: (e) => set('times', [`00:${two(Math.min(59, Math.max(0, Number(e.target.value) || 0)))}`]) }))],
    daily: () => [el('label', {}, 'Time', time())],
    weekdays: () => [el('label', {}, 'Time', time())],
    weekly: () => [dayPick(true), el('label', {}, 'Time', time())],
    monthly: () => [el('label', {}, 'Day of the month', el('input', { type: 'number', min: 1, max: 31, value: Number(dd), onchange: (e) => set('date', `2000-01-${two(Math.min(31, Math.max(1, Number(e.target.value) || 1)))}`) })), el('label', {}, 'Time', time())],
    annually: () => [el('select', { 'aria-label': 'Month', onchange: (e) => set('date', `2000-${two(Number(e.target.value) + 1)}-${dd}`) }, MONTH.map((m, i) => el('option', { value: i, selected: i === Number(mm) - 1 }, m))),
      el('input', { type: 'number', min: 1, max: 31, value: Number(dd), 'aria-label': 'Day', onchange: (e) => set('date', `${yy}-${mm}-${two(Math.min(31, Math.max(1, Number(e.target.value) || 1)))}`) }), el('label', {}, 'Time', time())],
    custom: () => [dayPick(true), el('div', { class: 'times' }, s.times.map((t, i) => el('span', { class: 't' }, time(i), s.times.length > 1 ? el('button', { type: 'button', 'aria-label': 'Remove time', onclick: () => set('times', s.times.filter((_, j) => j !== i)) }, '×') : null)),
      el('button', { type: 'button', class: 'pill small', onclick: () => set('times', [...s.times, laterTime(s.times[s.times.length - 1])]) }, '+ Add time'))],
  };
  $('#trig-detail').replaceChildren(...parts[s.repeat]());
  $('#trig-menu').replaceChildren(el('div', { class: 'group', text: 'General' }), ...TRIGGERS.map((r) => {
    const preview = r === s.repeat ? s : defaultSchedule(r, s);
    return el('button', { type: 'button', role: 'option', 'aria-selected': String(r === s.repeat), onclick: () => { draft.schedule = r === s.repeat ? s : defaultSchedule(r, s); $('#trig-menu').hidden = true; drawAuto(); } },
      clockIcon(), el('span', {}, el('b', { text: r === 'custom' ? 'Custom' : cap(r) }), el('span', { class: 'sub', text: r === 'hourly' && r !== s.repeat ? 'Every hour within chosen hours' : r === 'custom' && r !== s.repeat ? 'Pick days and several times' : describe(preview) })));
  }));
  validAuto();
}
function clockIcon() {
  const t = document.createElement('template');
  t.innerHTML = '<svg viewBox="0 0 24 24" width="22" height="22" aria-hidden="true"><circle cx="12" cy="12" r="8.5" fill="none" stroke="currentColor" stroke-width="2"/><path d="M12 7.5V12l3 2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"/></svg>';
  return t.content.firstChild;
}
function drawHistory() {
  const a = draft && draft.id && autos.find((x) => x.id === draft.id);
  const runs = (a && a.history) || [];
  $('#auto-history-wrap').hidden = !a;
  if (!a) return;
  $('#auto-clear-history').hidden = !runs.length;
  $('#auto-history').replaceChildren(...(runs.length ? runs.map((r) => el('div', { class: 'hrow' },
    el('span', { class: 'when' }, el('b', { class: r.ok ? '' : 'failed', text: r.ok ? 'Done' : 'Failed' }), ' · ' + whenText(r.at)),
    el('button', { type: 'button', 'aria-label': 'Delete this run', title: 'Delete this run', onclick: () => deleteRuns(a.id, r.id) }, '×'),
    el('details', {}, el('summary', { text: r.result.replace(/\s+/g, ' ').slice(0, 140) || '(no reply)' }), el('div', { class: 'full' }, markdown(r.result))))) : [el('div', { class: 'muted', text: 'No runs yet.' })]));
}
async function deleteRuns(id, run) {
  try { await api(`/api/automations/${id}/history/delete`, run ? { run } : {}); } catch (e) { toast(e.message); }
}
function validAuto() {
  $('#auto-save').disabled = !$('#auto-name').value.trim() || !$('#auto-text').value.trim();
}
async function saveAuto() {
  draft.name = $('#auto-name').value.trim();
  draft.instructions = $('#auto-text').value.trim();
  draft.notify = $('#auto-notify').checked;
  const body = { bot: draft.bot, name: draft.name, instructions: draft.instructions, schedule: draft.schedule, notify: draft.notify, enabled: draft.id ? draft.enabled : true };
  try {
    await api(draft.id ? `/api/automations/${draft.id}` : '/api/automations', body);
    $('#auto-dlg').close();
    autos = await api('/api/automations');
    openAutos();
  } catch (e) { toast(e.message); }
}
function whenText(ms) {
  const d = new Date(ms);
  const today = new Date();
  const tomorrow = new Date(Date.now() + 86400000);
  const hm = `${two(d.getHours())}:${two(d.getMinutes())}`;
  if (d.toDateString() === today.toDateString()) return `today at ${hm}`;
  if (d.toDateString() === tomorrow.toDateString()) return `tomorrow at ${hm}`;
  return `${DAY[d.getDay()].slice(0, 3)} ${d.getDate()} ${MONTH[d.getMonth()].slice(0, 3)} at ${hm}`;
}
function openAutos() {
  $('#autos-dlg').open || $('#autos-dlg').showModal();
  renderAutos();
}
function renderAutos() {
  if (!$('#autos-dlg').open) return;
  if (!autos.length) {
    $('#autos-body').replaceChildren(el('div', { class: 'autos-empty' }, el('div', { class: 'bolt' }, clockIcon()), el('div', { text: 'Have a bot do something on a schedule: a morning summary, a daily code check, a weekly report.' }), el('button', { type: 'button', class: 'pill primary', onclick: () => openAuto(null) }, 'New automation')));
    return;
  }
  $('#autos-body').replaceChildren(...autos.map((a) => {
    const b = bots.find((x) => x.id === a.bot);
    const toggle = el('input', { type: 'checkbox', class: 'switch', 'aria-label': `${a.name} on`, checked: a.enabled, onclick: (e) => e.stopPropagation(), onchange: async (e) => {
      try { await api(`/api/automations/${a.id}`, { ...a, enabled: e.target.checked }); } catch (err) { toast(err.message); e.target.checked = !e.target.checked; }
    } });
    const last = a.last_status === 'running' ? 'Running now…' : a.last_status ? `Last run ${a.last_status === 'done' ? 'done' : 'failed'}${a.last_result ? ': ' + a.last_result : ''}` : '';
    return el('div', { class: 'arow', role: 'button', tabindex: 0, onclick: () => openAuto(a), onkeydown: (e) => { if (e.key === 'Enter') openAuto(a); } },
      el('span', { class: 'avatar' }, b ? avatar(b.shape, b.color, 40) : null),
      el('b', { text: a.name }),
      toggle,
      el('span', { class: 'sub', text: `${b ? b.name + ' · ' : ''}${describe(a.schedule)}${a.enabled && a.next_run ? ' · next ' + whenText(a.next_run) : a.enabled ? '' : ' · off'}` }),
      last ? el('span', { class: 'last' + (a.last_status === 'failed' ? ' failed' : ''), text: last }) : null);
  }));
}

// ---- first-run setup -------------------------------------------------------------------
function renderSetup() {
  const ready = runtime.state === 'ready';
  const firstPull = pulls.get(defaultModel);
  const needModel = ready && !installed.length;
  const show = !ready || needModel;
  $('#setup').hidden = !show;
  if (!show) return;
  $('#setup-avatar').replaceChildren(el('img', { src: '/icon.svg', alt: '', width: 96, height: 96 }));
  const eng = $('#step-engine');
  eng.classList.toggle('done', ready);
  eng.classList.toggle('fail', runtime.state === 'failed');
  const sub = { checking: 'Looking for Ollama…', downloading: `Downloading Ollama ${runtime.total ? Math.round(runtime.completed / 1e6) + ' of ' + Math.round(runtime.total / 1e6) + ' MB' : ''}`, starting: 'Starting Ollama (the first start can take a minute or two)…', ready: runtime.source === 'system' ? 'Using the Ollama already on this computer' : 'Ready', failed: runtime.message }[runtime.state] || '';
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
  const next = { id: s.id, name: s.profile.name, shape: s.profile.shape, color: s.profile.color, model: s.model, think: s.think, run_code: s.run_code, folder: s.cwd, status: s.status, preview: s.last_text, pending: s.pending.length, updated: s.updated, created: s.created };
  if (b) Object.assign(b, next); else bots.push(next);
  renderBots();
  if (s.id === current) { updateTyping(); renderThink(); renderCode(); if (b && b.model !== next.model) renderModelSelect(); }
}

// Older versions installed a caching service worker that could show the old interface.
if ('serviceWorker' in navigator) navigator.serviceWorker.getRegistrations().then((rs) => rs.forEach((r) => r.unregister())).catch(() => {});
if (window.caches) caches.keys().then((ks) => ks.forEach((k) => caches.delete(k))).catch(() => {});

async function boot() {
  if (!token) return showGate();
  try {
    const r = await api('/api/runtime');
    runtime = r.runtime;
    defaultModel = r.default_model;
    setOnline(r.online !== false);
    sandbox = !!r.sandbox;
    autos = await api('/api/automations');
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
    if (msg.kind === 'state') { onState(msg); return; }
    if (msg.kind === 'automations') { autos = msg.automations; renderAutos(); if ($('#auto-dlg').open) drawHistory(); return; }
    if (msg.kind === 'notice') { toast(`${msg.title}: ${msg.body.slice(0, 120)}`); return; }
    if (msg.kind === 'network') setOnline(msg.online);
  };
  es.onopen = () => { refresh().catch(() => {}); if (current) api(`/api/bots/${current}/messages?since=${lastSeq}`).then((evs) => evs.forEach(render)).catch(() => {}); };
}

// ---- wiring -----------------------------------------------------------------------------------
$('#composer').addEventListener('submit', send);
$('#btn-attach').onclick = () => $('#file-input').click();
$('#file-input').onchange = (e) => { addFiles([...e.target.files]); e.target.value = ''; };
{
  let depth = 0;
  const chat = $('#chat');
  const hasFiles = (e) => [...(e.dataTransfer?.types || [])].includes('Files');
  chat.addEventListener('dragenter', (e) => { if (!hasFiles(e) || !bot()) return; e.preventDefault(); depth++; $('#drop').hidden = false; });
  chat.addEventListener('dragover', (e) => { if (hasFiles(e) && bot()) e.preventDefault(); });
  chat.addEventListener('dragleave', () => { if (--depth <= 0) { depth = 0; $('#drop').hidden = true; } });
  chat.addEventListener('drop', (e) => { if (!hasFiles(e)) return; e.preventDefault(); depth = 0; $('#drop').hidden = true; addFiles([...e.dataTransfer.files]); });
}
$('#input').addEventListener('paste', (e) => { const fs = [...(e.clipboardData?.files || [])]; if (fs.length) { e.preventDefault(); addFiles(fs); } });
$('#input').addEventListener('input', autosize);
$('#input').addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); $('#composer').requestSubmit(); } });
$('#btn-new').onclick = openNewBot;
$('#btn-new-empty').onclick = openNewBot;
$('#nb-name').addEventListener('keydown', (e) => { if (e.key === 'Enter') createBot(e); });
$('#nb-create').addEventListener('click', createBot);
$('#btn-search').onclick = () => { const w = $('#search-wrap'); w.hidden = !w.hidden; if (!w.hidden) $('#search').focus(); else { $('#search').value = ''; renderBots(); } };
$('#search').oninput = renderBots;
$('#btn-models').onclick = () => { modelQuery = ''; modelFind.value = ''; $('#models-dlg').showModal(); renderModelsDialog(); modelFind.focus(); loadModels().then(renderModelsDialog); };
$('#btn-autos').onclick = openAutos;
$('#autos-new').onclick = () => openAuto(null);
$('#menu-auto').onclick = () => { const b = bot(); if (b) openAuto(null, b.id); };
$('#auto-name').oninput = validAuto;
$('#auto-text').oninput = validAuto;
$('#auto-save').onclick = saveAuto;
$('#auto-trigger').onclick = () => { $('#trig-menu').hidden = !$('#trig-menu').hidden; };
$('#auto-dlg').addEventListener('click', (e) => { if (!e.target.closest('.trigger-wrap')) $('#trig-menu').hidden = true; });
$('#auto-run').onclick = async () => { try { await api(`/api/automations/${draft.id}/run`, {}); $('#auto-dlg').close(); toast(`${draft.name} is running`); select(draft.bot); } catch (e) { toast(e.message); } };
$('#auto-clear-history').onclick = () => { if (draft && draft.id && confirm('Delete every past run of this automation?')) deleteRuns(draft.id); };
$('#auto-delete').onclick = async () => {
  if (!confirm(`Delete the automation "${draft.name}"?`)) return;
  try { await api(`/api/automations/${draft.id}/delete`, {}); $('#auto-dlg').close(); autos = await api('/api/automations'); openAutos(); } catch (e) { toast(e.message); }
};
$('#btn-back').onclick = () => document.getElementById('app').classList.remove('in-chat');
$('#btn-menu').onclick = () => { $('#menu').hidden = !$('#menu').hidden; };
document.addEventListener('click', (e) => { if (!e.target.closest('#btn-menu') && (!e.target.closest('#menu') || e.target.closest('#menu button'))) $('#menu').hidden = true; });
$('#btn-code').onclick = toggleCode;
$('#btn-think').onclick = async () => {
  const b = bot();
  if (!b) return;
  try { Object.assign(b, await api(`/api/bots/${b.id}/think`, { think: !b.think })); renderThink(); } catch (e) { toast(e.message); }
};
$('#menu-window').onclick = () => { const b = bot(); if (b) window.open(`/?bot=${encodeURIComponent(b.id)}`, '_blank'); };
$('#menu-folder').onclick = () => reveal();
$('#menu-delete').onclick = () => deleteBot(bot());
$('#menu-clear').onclick = () => clearChat(bot());
$('#ctx-window').onclick = () => { if (ctxBot) window.open(`/?bot=${encodeURIComponent(ctxBot.id)}`, '_blank'); };
$('#ctx-auto').onclick = () => { if (ctxBot) openAuto(null, ctxBot.id); };
$('#ctx-folder').onclick = () => { if (ctxBot) api(`/api/bots/${ctxBot.id}/reveal`, {}).catch((e) => toast(e.message)); };
$('#ctx-clear').onclick = () => clearChat(ctxBot);
$('#ctx-delete').onclick = () => deleteBot(ctxBot);
document.addEventListener('click', (e) => { if (!e.target.closest('#ctx') || e.target.closest('#ctx button')) $('#ctx').hidden = true; });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') $('#ctx').hidden = true; });
window.addEventListener('blur', () => { $('#ctx').hidden = true; });
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
