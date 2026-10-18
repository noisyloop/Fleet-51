// Fleet-51 web UI. No build step; talks to the daemon's HTTP API and event streams.
'use strict';

const $ = (sel, root = document) => root.querySelector(sel);
const el = (tag, attrs = {}, ...kids) => {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k.startsWith('on')) n.addEventListener(k.slice(2), v);
    else if (k === 'text') n.textContent = v;
    else n.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k != null) n.append(k instanceof Node ? k : document.createTextNode(String(k)));
  return n;
};
const store = {
  get(k, d) { try { const v = localStorage.getItem('fleet51.' + k); return v == null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem('fleet51.' + k, JSON.stringify(v)); } catch { /* storage unavailable */ } },
};
const stripAnsi = (s) => s.replace(/\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[@-Z\\-_]/g, '').replace(/\r/g, '');
const money = (n) => '$' + (n || 0).toFixed(4);

// ---- auth ---------------------------------------------------------------
let token = new URLSearchParams(location.search).get('token') || store.get('token', '');
if (new URLSearchParams(location.search).has('token')) {
  store.set('token', token);
  history.replaceState(null, '', location.pathname);
}

async function api(path, opts = {}) {
  const res = await fetch(path, {
    method: opts.method || (opts.body ? 'POST' : 'GET'),
    headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' },
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (res.status === 401) { showGate(); throw new Error('token required'); }
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}
const post = (path, body = {}) => api(path, { method: 'POST', body });

function showGate() {
  $('#token-gate').hidden = false;
  $('#token-save').onclick = () => { token = $('#token-input').value.trim(); store.set('token', token); location.reload(); };
}

// ---- state --------------------------------------------------------------
const sessions = new Map(); // id -> state
let open = store.get('open', []);
const panes = new Map(); // id -> { root, es, ... }
let agents = [];

function toast(msg, kind = '') {
  const t = el('div', { class: 'msg sys ' + kind, text: msg, style: 'position:fixed;bottom:16px;right:16px;background:var(--panel);border:1px solid var(--border);padding:8px 12px;border-radius:8px;z-index:20;max-width:420px' });
  document.body.append(t);
  setTimeout(() => t.remove(), 4000);
}

// ---- sidebar ------------------------------------------------------------
function renderList() {
  const list = $('#session-list');
  const q = $('#filter').value.toLowerCase();
  list.replaceChildren();
  const all = [...sessions.values()].filter((s) => !q || (s.title + s.agent + s.id).toLowerCase().includes(q)).sort((a, b) => b.created - a.created);
  const groups = new Map();
  for (const s of all) {
    const key = s.group || '';
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(s);
  }
  for (const [g, items] of groups) {
    const head = el('div', { class: 'group-head' }, g ? `Fan-out ${g}` : 'Sessions');
    if (g) {
      head.append(el('span', {},
        el('button', { type: 'button', onclick: () => { for (const s of items) openPane(s.id); } }, 'Open all'), ' ',
        el('button', { type: 'button', onclick: () => compareDialog(g) }, 'Compare')));
    }
    list.append(head);
    for (const s of items) {
      list.append(el('div', { class: 'srow' + (open.includes(s.id) ? ' open' : ''), onclick: () => togglePane(s.id) },
        el('span', { class: 'dot ' + s.status, title: s.status }),
        el('span', { class: 't', text: s.title || s.id }),
        el('span', { class: 'muted', text: money(s.cost_usd) }),
        el('span', { class: 'm', text: `${s.agent} · ${s.status.replace('_', ' ')}${s.account ? ' · ' + s.account : ''} · ${s.id}` })));
    }
  }
  if (!all.length) list.append(el('div', { class: 'group-head', text: 'No sessions yet' }));
}

// ---- panes --------------------------------------------------------------
function togglePane(id) { open.includes(id) ? closePane(id) : openPane(id); if (innerWidth < 760) $('#side').classList.remove('show'); }
function persistOpen() { store.set('open', open); renderList(); $('#empty').hidden = open.length > 0; }

function closePane(id) {
  const p = panes.get(id);
  if (p) { p.root.remove(); panes.delete(id); }
  open = open.filter((x) => x !== id);
  persistOpen();
}

function openPane(id) {
  if (panes.has(id) || !sessions.has(id)) return;
  if (!open.includes(id)) open.push(id);
  const s = sessions.get(id);
  const feed = el('div', { class: 'feed' });
  const status = el('span', { class: 'pill' });
  const meta = el('div', { class: 'pane-meta' });
  const input = el('textarea', { rows: 1, placeholder: 'Prompt the agent (⌘/Ctrl+Enter to send)' });
  const send = async () => {
    const text = input.value.trim();
    if (!text) return;
    input.value = '';
    try { await post(`/api/sessions/${id}/prompt`, { text }); } catch (e) { toast(e.message, 'error'); input.value = text; }
  };
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); send(); } });
  const stopBtn = el('button', { type: 'button', onclick: () => post(`/api/sessions/${id}/stop`).catch((e) => toast(e.message, 'error')) }, 'Stop');
  const resumeBtn = el('button', { type: 'button', onclick: () => post(`/api/sessions/${id}/resume`).catch((e) => toast(e.message, 'error')) }, 'Resume');
  const root = el('section', { class: 'pane' },
    el('div', { class: 'pane-head' },
      el('span', { class: 'title', text: s.title || id, title: s.title }),
      status,
      el('div', { class: 'pane-tools' },
        el('button', { type: 'button', onclick: () => diffDialog(id) }, 'Diff'),
        stopBtn, resumeBtn,
        el('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Close pane', onclick: () => closePane(id) }, '✕'))),
    meta, feed,
    el('div', { class: 'compose' }, input, el('button', { type: 'button', class: 'primary', onclick: send }, 'Send')));
  $('#grid').append(root);
  const pane = { root, feed, status, meta, stopBtn, resumeBtn, prompts: new Map(), term: null, lastSeq: 0, loaded: false, buffer: [] };
  panes.set(id, pane);
  updatePane(id);
  loadHistory(id);
  persistOpen();
}

// Live events arrive on the one shared stream; history is a plain request per pane.
async function loadHistory(id) {
  const p = panes.get(id);
  if (!p) return;
  try {
    for (const e of await api(`/api/sessions/${id}/history?since=${p.lastSeq}`)) renderEvent(p, e);
  } catch (e) { toast(e.message, 'error'); }
  p.loaded = true;
  for (const e of p.buffer.splice(0)) renderEvent(p, e);
}

function updatePane(id) {
  const p = panes.get(id); const s = sessions.get(id);
  if (!p || !s) return;
  p.status.className = 'pill ' + s.status;
  p.status.textContent = s.status.replace('_', ' ');
  const live = ['starting', 'idle', 'working', 'rate_limited'].includes(s.status);
  p.stopBtn.hidden = !live && !['suspended', 'orphaned'].includes(s.status);
  p.resumeBtn.hidden = live;
  p.meta.replaceChildren(...[
    el('span', { text: s.agent }),
    el('span', { text: 'sandbox ' + s.sandbox }),
    el('span', { text: 'perm ' + s.permission }),
    s.account ? el('span', { text: 'account ' + s.account }) : null,
    el('span', { text: `${s.turns} turns · ${s.input_tokens + s.output_tokens} tok · ${money(s.cost_usd)}` }),
    s.pending.length ? el('span', { text: `${s.pending.length} queued` }) : null,
    el('span', { class: 'muted', text: s.id })].filter(Boolean));
}

function line(pane, cls, text) {
  const atBottom = pane.feed.scrollHeight - pane.feed.scrollTop - pane.feed.clientHeight < 40;
  const n = el('div', { class: 'msg ' + cls, text });
  pane.feed.append(n);
  if (atBottom) pane.feed.scrollTop = pane.feed.scrollHeight;
  return n;
}

function renderEvent(pane, e) {
  if (e.seq <= pane.lastSeq) return;
  pane.lastSeq = e.seq;
  switch (e.type) {
    case 'created': line(pane, 'sys', `${e.agent} in ${e.cwd}`); break;
    case 'spawned': line(pane, 'sys', `started via ${e.protocol}${e.pid ? ' · pid ' + e.pid : ''}${e.account ? ' · account ' + e.account : ''}${e.resume ? ' · resuming ' + e.resume.slice(0, 12) : ''}`); break;
    case 'prompt_queued': {
      const n = line(pane, 'user', e.text);
      const st = el('span', { class: 'st', text: 'queued' });
      n.append(st);
      pane.prompts.set(e.id, st);
      break;
    }
    case 'prompt_delivered': { const st = pane.prompts.get(e.id); if (st) st.textContent = 'delivered'; break; }
    case 'prompt_acked': { const st = pane.prompts.get(e.id); if (st) st.textContent = 'acknowledged ✓'; break; }
    case 'text': line(pane, 'agent', e.text); break;
    case 'tool_call': line(pane, 'tool', `⚙ ${e.name} ${JSON.stringify(e.input || {}).slice(0, 140)}`); break;
    case 'permission': line(pane, 'tool', `⚖ ${e.tool} → ${e.decision}`); break;
    case 'output': {
      if (!pane.term) {
        pane.term = el('pre', { class: 'term' });
        pane.feed.append(pane.term);
      }
      pane.term.textContent += stripAnsi(e.data);
      pane.term.scrollTop = pane.term.scrollHeight;
      break;
    }
    case 'turn_done': line(pane, 'turn', `${e.ok ? 'turn done' : 'turn failed'} · ${e.input_tokens} in / ${e.output_tokens} out · ${money(e.cost_usd)}`); pane.term = null; break;
    case 'rate_limited': line(pane, 'sys warn', `rate limited (${e.account || 'default'}): ${e.message}`); break;
    case 'account_switched': line(pane, 'sys warn', `switched account ${e.from || 'default'} → ${e.to}`); break;
    case 'hook': line(pane, 'sys', `hook: ${e.name}`); break;
    case 'annotation': line(pane, 'sys', `✎ ${e.file}:${e.line} ${e.comment}`); break;
    case 'feedback_sent': line(pane, 'sys', `sent ${e.annotations.length} review note(s) as feedback`); break;
    case 'suspended': line(pane, 'sys warn', `suspended for daemon handoff (generation ${e.generation})`); break;
    case 'orphaned': line(pane, 'sys warn', `daemon restarted mid-session (now generation ${e.generation})`); break;
    case 'exited': line(pane, 'sys' + (e.clean ? '' : ' error'), `exited ${e.code ?? ''} (${e.clean ? 'clean' : 'unclean'})`); break;
    case 'merged': line(pane, 'sys', `merged ${e.branch} into ${e.into} (${e.commit.slice(0, 10)})`); break;
    case 'note': if (e.level !== 'stderr' || e.message.length < 300) line(pane, 'sys ' + e.level, e.message); break;
  }
}

// ---- dialogs ------------------------------------------------------------
function dialog(title, body) {
  $('#dlg-title').textContent = title;
  $('#dlg-body').replaceChildren(body);
  if (!$('#dlg').open) $('#dlg').showModal();
}
const field = (label, input, wide) => el('label', { class: 'f' + (wide ? ' wide' : '') }, label, input);

function agentOptions(selected) {
  return agents.map((a) => el('option', { value: a.name, selected: a.name === selected }, `${a.name}${a.available ? '' : ' (not installed)'} · ${a.protocol}`));
}

function sessionForm(fanout) {
  const repos = el('textarea', { rows: 2, placeholder: '/path/to/repo\n/path/to/other-repo/packages/web' });
  repos.value = store.get('lastRepos', '');
  const prompt = el('textarea', { rows: 4, placeholder: 'What should the agent do?' });
  const sandbox = el('select', {}, ['none', 'docker', 'docker-offline'].map((v) => el('option', { value: v }, v)));
  const perm = el('select', {}, ['auto', 'default', 'plan', 'yolo'].map((v) => el('option', { value: v }, v)));
  const account = el('input', { placeholder: 'default login, an account name, or auto' });
  const model = el('input', { placeholder: 'agent default' });
  const pick = fanout
    ? el('div', { class: 'checks' }, agents.map((a) => el('label', {}, el('input', { type: 'checkbox', value: a.name, checked: a.name.startsWith('mock-claude') || a.name === 'mock-codex' }), a.name)))
    : el('select', {}, agentOptions(agents.find((a) => a.available && !a.name.startsWith('mock'))?.name || 'mock-claude'));
  const n = el('input', { type: 'number', min: 1, max: 8, value: 1 });
  const submit = async () => {
    const repoList = repos.value.split('\n').map((s) => s.trim()).filter(Boolean).map((path) => ({ path }));
    store.set('lastRepos', repos.value);
    try {
      if (fanout) {
        const chosen = [...pick.querySelectorAll('input:checked')].map((i) => i.value);
        const r = await post('/api/fanout', { agents: chosen, repos: repoList, prompt: prompt.value, n: +n.value || 1, sandbox: sandbox.value, permission: perm.value, account: account.value || null });
        await refresh();
        for (const s of r.sessions) openPane(s.id);
      } else {
        const s = await post('/api/sessions', { agent: pick.value, repos: repoList, prompt: prompt.value || null, sandbox: sandbox.value, permission: perm.value, account: account.value || null, model: model.value || null });
        await refresh();
        openPane(s.id);
      }
      $('#dlg').close();
    } catch (e) { toast(e.message, 'error'); }
  };
  return el('div', {},
    el('div', { class: 'fields' },
      field(fanout ? 'Agents' : 'Agent', pick, true),
      field('Repos or folders (one per line; several = multi-repo workspace; a monorepo subfolder is fine)', repos, true),
      field('Prompt', prompt, true),
      field('Sandbox', sandbox), field('Permission', perm),
      field('Account', account), fanout ? field('Attempts per agent', n) : field('Model', model)),
    el('div', { class: 'row-actions' }, el('button', { type: 'button', class: 'primary', onclick: submit }, fanout ? 'Fan out' : 'Start')));
}

function parsePatch(patch) {
  const files = [];
  let cur = null; let newLine = 0;
  for (const raw of patch.split('\n')) {
    if (raw.startsWith('diff --git')) { cur = { path: raw.split(' b/').pop(), lines: [] }; files.push(cur); continue; }
    if (!cur) continue;
    if (raw.startsWith('+++ ')) { const p = raw.slice(4); if (p !== '/dev/null') cur.path = p.replace(/^b\//, ''); continue; }
    if (raw.startsWith('--- ') || raw.startsWith('index ') || raw.startsWith('new file') || raw.startsWith('deleted file') || raw.startsWith('similarity') || raw.startsWith('rename ')) continue;
    if (raw.startsWith('@@')) { const m = /\+(\d+)/.exec(raw); newLine = m ? +m[1] : 0; cur.lines.push({ kind: 'hunk', text: raw }); continue; }
    if (raw.startsWith('+')) cur.lines.push({ kind: 'add', text: raw, n: newLine++ });
    else if (raw.startsWith('-')) cur.lines.push({ kind: 'del', text: raw });
    else if (raw.startsWith(' ')) cur.lines.push({ kind: 'ctx', text: raw, n: newLine++ });
  }
  return files;
}

async function diffDialog(id) {
  let data;
  try { data = await api(`/api/sessions/${id}/diff`); } catch (e) { return toast(e.message, 'error'); }
  const s = sessions.get(id);
  const multi = data.checkouts.length > 1;
  const notes = (data.annotations || []);
  const unsent = notes.filter((a) => !a.sent).length;
  const body = el('div', {});
  body.append(el('div', { class: 'row-actions', style: 'margin:0 0 12px;justify-content:flex-start' },
    el('button', { type: 'button', class: 'primary', disabled: !unsent, onclick: async () => { try { await post(`/api/sessions/${id}/feedback`); toast('Feedback sent to the agent'); $('#dlg').close(); } catch (e) { toast(e.message, 'error'); } } }, `Send feedback (${unsent})`),
    el('button', { type: 'button', onclick: async () => { if (!confirm('Commit this work and merge its branch into the repo?')) return; try { const r = await post(`/api/sessions/${id}/merge`); toast(`Merged ${r.map((x) => x.branch).join(', ')}`); diffDialog(id); } catch (e) { toast(e.message, 'error'); } } }, 'Merge into repo'),
    el('span', { class: 'muted', text: 'Click a line number to comment.' })));
  for (const co of data.checkouts) {
    const coBox = el('div', { class: 'diff-co' }, el('h3', { text: `${co.checkout} · ${co.kind} · ${co.files.length} file(s)` }));
    if (co.error) coBox.append(el('div', { class: 'msg sys error', text: co.error }));
    if (!co.patch && !co.error) coBox.append(el('div', { class: 'muted', text: 'No changes yet.' }));
    for (const f of parsePatch(co.patch)) {
      const rel = multi ? `${co.checkout}/${f.path}` : f.path;
      const st = co.files.find((x) => x.path === f.path);
      const box = el('div', { class: 'diff-file' }, el('div', { class: 'fh' }, el('span', { text: f.path }), el('span', { text: st ? `+${st.added} −${st.deleted}` : '' })));
      for (const l of f.lines) {
        const row = el('div', { class: 'dl ' + l.kind + (l.n ? ' commentable' : '') }, el('span', { class: 'ln', text: l.n || '' }), el('span', { text: l.text }));
        box.append(row);
        for (const a of notes.filter((a) => a.file === rel && a.line === l.n)) box.append(el('div', { class: 'note' + (a.sent ? ' sent' : ''), text: `${a.comment}${a.sent ? ' (sent)' : ''}` }));
        if (l.n) row.addEventListener('click', () => {
          if (row.nextSibling?.classList?.contains('annot-box')) return;
          const inp = el('input', { placeholder: `Comment on ${rel}:${l.n}` });
          const save = async () => {
            if (!inp.value.trim()) return;
            try { await post(`/api/sessions/${id}/annotations`, { file: rel, line: l.n, comment: inp.value.trim() }); diffDialog(id); } catch (e) { toast(e.message, 'error'); }
          };
          inp.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); save(); } });
          row.after(el('div', { class: 'annot-box' }, inp, el('button', { type: 'button', onclick: save }, 'Add')));
          inp.focus();
        });
      }
      coBox.append(box);
    }
    body.append(coBox);
  }
  dialog(`Review · ${s ? s.title : id}`, body);
}

async function compareDialog(group) {
  let g;
  try { g = await api(`/api/groups/${group}`); } catch (e) { return toast(e.message, 'error'); }
  const rows = g.attempts.map((a) => {
    const s = a.session;
    return el('tr', {},
      el('td', {}, el('span', { class: 'dot ' + s.status, style: 'display:inline-block;margin-right:6px' }), s.agent, el('div', { class: 'muted', text: s.id })),
      el('td', { text: s.status.replace('_', ' ') }),
      el('td', { class: 'num', text: a.files_changed }),
      el('td', { class: 'num', text: `+${a.added} −${a.deleted}` }),
      el('td', { class: 'num', text: s.turns }),
      el('td', { class: 'num', text: money(s.cost_usd) }),
      el('td', { text: (s.last_text || '').split('\n')[0].slice(0, 120) }),
      el('td', {}, el('button', { type: 'button', onclick: () => diffDialog(s.id) }, 'Review')));
  });
  dialog(`Compare attempts · ${group}`, el('table', {},
    el('thead', {}, el('tr', {}, ['Agent', 'Status', 'Files', 'Lines', 'Turns', 'Cost', 'Last message', ''].map((h, i) => el('th', { class: [2, 3, 4, 5].includes(i) ? 'num' : '', text: h })))),
    el('tbody', {}, rows)));
}

async function usageDialog() {
  const u = await api('/api/usage');
  const table = (title, obj) => el('div', { style: 'margin-bottom:16px' }, el('h3', { text: title, style: 'font-size:13px' }), el('table', {},
    el('thead', {}, el('tr', {}, ['', 'Sessions', 'Turns', 'Input tok', 'Output tok', 'Cost'].map((h, i) => el('th', { class: i ? 'num' : '', text: h })))),
    el('tbody', {}, Object.entries(obj).map(([k, v]) => el('tr', {}, el('td', { text: k }), ...['sessions', 'turns', 'input_tokens', 'output_tokens'].map((f) => el('td', { class: 'num', text: v[f] })), el('td', { class: 'num', text: money(v.cost_usd) }))))));
  dialog('Usage', el('div', {}, table('Total', { all: u.total }), table('By agent', u.by_agent), table('By account', u.by_account),
    el('p', { class: 'muted', text: 'Usage is read from each session’s event log, so plain folders and remote hosts are counted too.' })));
}

async function accountsDialog() {
  const list = await api('/api/accounts');
  const provider = el('input', { placeholder: 'anthropic, openai, mock…' });
  const name = el('input', { placeholder: 'work' });
  dialog('Accounts', el('div', {},
    el('table', {}, el('thead', {}, el('tr', {}, ['Provider', 'Account', 'State', 'Config dir'].map((h) => el('th', { text: h })))),
      el('tbody', {}, list.map((a) => el('tr', {}, el('td', { text: a.provider }), el('td', { text: a.name }), el('td', { text: a.cooldown_until ? 'cooling down until ' + new Date(a.cooldown_until).toLocaleTimeString() : 'ready' }), el('td', {}, el('code', { text: a.dir })))))),
    el('p', { class: 'muted', text: 'Each account is an isolated config dir. Log in once with it (e.g. CLAUDE_CONFIG_DIR=<dir> claude). Sessions with account "auto" pick a ready account and fail over on rate limits.' }),
    el('div', { class: 'fields' }, field('Provider', provider), field('Name', name)),
    el('div', { class: 'row-actions' }, el('button', { type: 'button', class: 'primary', onclick: async () => { try { await post('/api/accounts', { provider: provider.value, name: name.value }); accountsDialog(); } catch (e) { toast(e.message, 'error'); } } }, 'Add account'))));
}

// ---- live updates & notifications ---------------------------------------
function notifyChange(prev, s) {
  if (!prev || document.visibilityState === 'visible' || Notification?.permission !== 'granted') return;
  if (prev.status === 'working' && s.status === 'idle') new Notification('Fleet-51: turn finished', { body: `${s.title}\n${(s.last_text || '').slice(0, 140)}`, tag: s.id });
  if (s.status !== prev.status && ['rate_limited', 'failed'].includes(s.status)) new Notification(`Fleet-51: ${s.status.replace('_', ' ')}`, { body: s.title, tag: s.id });
}

async function refresh() {
  const list = await api('/api/sessions');
  for (const s of list) sessions.set(s.id, s);
  renderList();
}

async function boot() {
  if (!token) return showGate();
  try {
    const h = await api('/api/health');
    $('#daemon').textContent = `generation ${h.generation} · ${h.url}`;
    agents = await api('/api/agents');
    await refresh();
  } catch (e) { return; }
  open = open.filter((id) => sessions.has(id));
  for (const id of [...open]) openPane(id);
  persistOpen();
  const es = new EventSource(`/api/stream?token=${encodeURIComponent(token)}`);
  es.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.kind === 'event') {
      const p = panes.get(msg.session);
      if (p) p.loaded ? renderEvent(p, msg.event) : p.buffer.push(msg.event);
      return;
    }
    const s = msg;
    const prev = sessions.get(s.id);
    sessions.set(s.id, s);
    notifyChange(prev, s);
    updatePane(s.id);
    renderList();
  };
  es.onerror = () => { $('#daemon').textContent = 'reconnecting…'; };
  // After a reconnect (for example a daemon handoff), catch up on anything missed.
  es.onopen = () => api('/api/health').then((h) => {
    $('#daemon').textContent = `generation ${h.generation} · ${h.url}`;
    refresh();
    for (const id of panes.keys()) loadHistory(id);
  }).catch(() => {});
}

$('#btn-new').onclick = () => dialog('New session', sessionForm(false));
$('#btn-fanout').onclick = () => dialog('Fan out one prompt to several agents', sessionForm(true));
$('#btn-usage').onclick = () => usageDialog().catch((e) => toast(e.message, 'error'));
$('#btn-accounts').onclick = () => accountsDialog().catch((e) => toast(e.message, 'error'));
$('#btn-notify').onclick = () => { if ('Notification' in window) Notification.requestPermission().then((p) => toast('Notifications ' + p)); };
$('#toggle-side').onclick = () => $('#side').classList.toggle('show');
$('#filter').oninput = renderList;
if ('serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js').catch(() => {});
boot();
