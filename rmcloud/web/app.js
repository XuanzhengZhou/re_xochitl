/* rmcloud Web UI（C16）——原生 JS，无构建、无依赖。
 * 令牌只在本页 localStorage（rmcloud.auth）和 Authorization 头里：不进 HTML、不进
 * URL（避免历史/Referer/日志泄露），401 就清掉重粘。页图用 fetch + blob URL；
 * 聊天用 fetch + ReadableStream 读 SSE（EventSource 加不了头）。服务端文本一律
 * textContent，不用 innerHTML。 */
'use strict';

const KEY = 'rmcloud.auth';
const $ = (sel) => document.querySelector(sel);

const state = {
  token: '',
  docs: new Map(),      // uuid -> {uuid,title,type,parent,deleted,pages,thumbs,contentSha}
  roots: [],
  open: null,
  pageList: [],         // 当前笔记的页（缩略图文件名；缺失项是 null）
  page: 0,
  urls: new Map(),      // "uuid/name" -> blob URL
  io: null,             // 页列表的 IntersectionObserver
  stream: null,         // 进行中的 fetch AbortController
  last: null,           // 最近一次问答 {body, ans}
  seq: 0,               // 打开笔记的序号，防快速点击时旧请求覆盖新页面
};

// ---- 工具

function el(tag, props = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'class') n.className = v;
    else if (k === 'text') n.textContent = v;
    else if (k.startsWith('on')) n.addEventListener(k.slice(2), v);
    else if (v === true) n.setAttribute(k, '');
    else if (v !== false && v != null) n.setAttribute(k, v);
  }
  for (const kid of kids.flat()) if (kid != null) n.append(kid);
  return n;
}

function status(text, kind = '') {
  const s = $('#status');
  s.textContent = text;
  s.className = 'status' + (kind ? ' ' + kind : '');
  s.title = text;
}

function readToken() {
  try { return localStorage.getItem(KEY) || ''; } catch { return ''; }
}

function writeToken(t) {
  state.token = t;
  try {
    if (t) localStorage.setItem(KEY, t);
    else localStorage.removeItem(KEY);
  } catch { /* 隐私模式：只在内存里用 */ }
}

/** 清空当前视图（令牌失效或「忘记」共用）：树、页、聊天、缓存 URL 全清。 */
function resetView() {
  state.docs.clear();
  state.roots = [];
  state.open = null;
  state.pageList = [];
  state.last = null;
  revokeUrls();
  state.seq++;
  if (state.io) { state.io.disconnect(); state.io = null; }
  renderTree();
  $('#pages').textContent = '';
  $('#pages').hidden = true;
  $('#viewer').hidden = true;
  $('#centerEmpty').hidden = false;
  $('#doctitle').textContent = '页面';
  $('#docmeta').textContent = '';
  $('#log').textContent = '';
  $('#log').append(el('p', { class: 'empty', text: '连上之后，可以就当前笔记提问。' }));
  lock(false);
}

function unauthorized() {
  writeToken('');
  resetView();
  const tree = $('#tree');
  tree.textContent = '';
  tree.append(el('p', { class: 'empty', text: '令牌无效，请重新粘贴' }));
  status('未授权：令牌无效或已重新配对', 'bad');
  $('#cred').value = '';
  $('#cred').placeholder = '粘贴访问令牌';
  $('#forget').hidden = true;
  $('#cred').focus();
}

// ---- API

async function api(path, opts = {}) {
  const headers = Object.assign({}, opts.headers || {}, {
    Authorization: 'Bearer ' + state.token,
  });
  const res = await fetch(path, Object.assign({}, opts, { headers }));
  if (res.status === 401) {
    unauthorized();
    throw new Error('unauthorized');
  }
  return res;
}

async function readJson(res) {
  try { return await res.json(); } catch { return {}; }
}

// ---- 载入库

async function loadLibrary() {
  status('载入中…');
  const [treeRes, manRes] = await Promise.all([
    api('/api/v1/tree').then(readJson),
    api('/api/v1/manifest').then(readJson),
  ]);

  const treeById = new Map((treeRes.items || []).map((i) => [i.uuid, i]));
  const meta = [];                 // [{uuid, sha}]
  const thumbs = new Map();        // uuid -> [文件名]
  const rms = new Map();           // uuid -> [文件名]
  const contentSha = new Map();    // uuid -> sha（.content，用来排页序）

  for (const e of manRes.entries || []) {
    const m = /^([^/]+)\.metadata$/.exec(e.path);
    if (m) { meta.push({ uuid: m[1], sha: e.sha256 }); continue; }
    const t = /^([^/]+)\.thumbnails\/(.+\.png)$/.exec(e.path);
    if (t) { push(thumbs, t[1], t[2]); continue; }
    const r = /^([^/]+)\/(.+\.rm)$/.exec(e.path);
    if (r) { push(rms, r[1], r[2]); continue; }
    const c = /^([^/]+)\.content$/.exec(e.path);
    if (c) contentSha.set(c[1], e.sha256);
  }

  state.docs.clear();
  // .metadata 里有 parent/type/deleted（/tree 不返回 parent，所以结构要自己拼）。
  for (let i = 0; i < meta.length; i += 8) {
    await Promise.all(meta.slice(i, i + 8).map(async (m) => {
      const t = treeById.get(m.uuid) || {};
      let j = {};
      try {
        j = await readJson(await api('/api/v1/blob/' + m.sha));
      } catch (e) {
        if (e.message === 'unauthorized') throw e;
      }
      state.docs.set(m.uuid, {
        uuid: m.uuid,
        title: String(j.visibleName || t.title || m.uuid).trim() || m.uuid,
        type: j.type || t.type || 'DocumentType',
        parent: j.parent || '',
        deleted: j.deleted === true,
        pages: t.pages || (rms.get(m.uuid) || []).length,
        thumbs: sortNames(thumbs.get(m.uuid) || []),
        contentSha: contentSha.get(m.uuid) || '',
      });
    }));
  }

  state.roots = buildTree();
  renderTree();
  const live = [...state.docs.values()].filter((d) => !d.deleted);
  const folders = live.filter((d) => d.type === 'CollectionType').length;
  status(`已连接 · ${live.length} 项（${folders} 个文件夹）`, 'ok');
  $('#forget').hidden = false;
  $('#cred').placeholder = '已保存令牌，可粘贴新的覆盖';
}

function push(map, k, v) {
  const list = map.get(k);
  if (list) list.push(v);
  else map.set(k, [v]);
}

function sortNames(list) {
  return list.slice().sort((a, b) => a.localeCompare(b, 'en', { numeric: true }));
}

function buildTree() {
  const byId = new Map();
  for (const d of state.docs.values()) {
    if (!d.deleted) byId.set(d.uuid, Object.assign({}, d, { kids: [] }));
  }
  const roots = [];
  for (const n of byId.values()) {
    const p = byId.get(n.parent);
    if (p && p !== n && p.type === 'CollectionType') p.kids.push(n);
    else roots.push(n);
  }
  const rank = (n) => (n.type === 'CollectionType' ? 0 : 1);
  const walk = (list) => {
    list.sort((a, b) => rank(a) - rank(b) || a.title.localeCompare(b.title, 'zh'));
    for (const n of list) walk(n.kids);
  };
  walk(roots);
  return roots;
}

function renderTree() {
  const nav = $('#tree');
  nav.textContent = '';
  if (!state.roots.length) {
    nav.append(el('p', { class: 'empty', text: state.token ? '库里还没有笔记' : '未连接' }));
    return;
  }
  const frag = document.createDocumentFragment();
  for (const n of state.roots) frag.append(treeNode(n));
  nav.append(frag);
}

function treeNode(n) {
  if (n.type === 'CollectionType') {
    const kids = el('div', { class: 'kids' });
    for (const k of n.kids) kids.append(treeNode(k));
    return el('details', { class: 'folder', open: true },
      el('summary', { text: `${n.title}（${n.kids.length}）` }), kids);
  }
  return el('button', {
    class: 'doc' + (state.open && state.open.uuid === n.uuid ? ' on' : ''),
    type: 'button',
    'data-uuid': n.uuid,
    onclick: () => openDoc(n.uuid),
  },
  el('span', { class: 't', text: n.title }),
  el('span', { class: 's', text: `${n.pages} 页` }));
}

// ---- 页列表

async function openDoc(uuid) {
  const d = state.docs.get(uuid);
  if (!d) return;
  const seq = ++state.seq;
  state.open = d;
  state.page = 0;
  state.pageList = [];
  $('#doctitle').textContent = d.title;
  $('#docmeta').textContent = d.pages ? `${d.pages} 页` : '';
  $('#centerEmpty').hidden = true;
  $('#viewer').hidden = true;
  $('#pages').hidden = false;
  $('#pages').textContent = '';
  $('#pages').append(el('p', { class: 'empty', text: '载入页面…' }));
  markActive();

  let thumbs = d.thumbs;
  if (thumbs.length && d.contentSha) {
    const order = await pageOrder(d).catch(() => null);
    if (seq !== state.seq) return;
    if (order) thumbs = order;
  }
  state.pageList = thumbs.length ? thumbs : Array.from({ length: d.pages }, () => null);
  renderGrid();
}

/** 用 <uuid>.content 的 cPages 顺序排缩略图（设备 render 的 page 参数就是页序号）。 */
async function pageOrder(d) {
  const res = await api('/api/v1/blob/' + d.contentSha);
  const j = await readJson(res);
  const ids = (j.cPages && j.cPages.pages || j.pages || [])
    .map((p) => (typeof p === 'string' ? p : p && p.id))
    .filter(Boolean);
  if (!ids.length) return null;
  const rank = new Map(ids.map((id, i) => [id, i]));
  const known = [];
  const rest = [];
  for (const name of d.thumbs) {
    const id = name.replace(/\.png$/, '');
    if (rank.has(id)) known.push([rank.get(id), name]);
    else rest.push(name);
  }
  known.sort((a, b) => a[0] - b[0]);
  return known.map((x) => x[1]).concat(rest);
}

function renderGrid() {
  if (state.io) { state.io.disconnect(); state.io = null; }
  const box = $('#pages');
  box.textContent = '';
  if (!state.pageList.length) {
    box.append(el('p', { class: 'empty', text: '这篇没有页数据（设备可能还没上传缩略图）' }));
    return;
  }
  const grid = el('div', { class: 'grid' });
  const io = new IntersectionObserver((entries) => {
    for (const en of entries) {
      if (!en.isIntersecting) continue;
      io.unobserve(en.target);
      loadThumb(en.target);
    }
  }, { root: box, rootMargin: '300px' });
  state.pageList.forEach((name, i) => {
    const cell = el('button', {
      class: 'page', type: 'button', 'data-i': i,
      'aria-label': `第 ${i + 1} 页`,
      onclick: () => showPage(i),
    }, el('span', { class: 'ph', text: String(i + 1) }));
    grid.append(cell);
    io.observe(cell);
  });
  box.append(grid);
  state.io = io;
}

async function loadThumb(cell) {
  const d = state.open;
  const name = state.pageList[Number(cell.dataset.i)];
  if (!d || !name) { cell.classList.add('missing'); return; }
  try {
    const url = await thumbUrl(d.uuid, name);
    const img = el('img', { src: url, alt: cell.getAttribute('aria-label') });
    cell.textContent = '';
    cell.append(img);
    cell.classList.add('ready');
  } catch (e) {
    if (e.message === 'unauthorized') return;
    cell.textContent = '';
    cell.append(el('span', { class: 'ph', text: '缩略图缺失' }));
    cell.classList.add('missing');
  }
}

async function thumbUrl(uuid, name) {
  const key = uuid + '/' + name;
  const hit = state.urls.get(key);
  if (hit) return hit;
  const res = await api('/api/v1/thumb/' + encodeURIComponent(uuid) + '/' + encodeURIComponent(name));
  if (!res.ok) throw new Error('HTTP ' + res.status);
  const url = URL.createObjectURL(await res.blob());
  state.urls.set(key, url);
  return url;
}

function revokeUrls() {
  for (const url of state.urls.values()) URL.revokeObjectURL(url);
  state.urls.clear();
}

function showPage(i) {
  if (i < 0 || i >= state.pageList.length) return;
  state.page = i;
  $('#pages').hidden = true;
  $('#viewer').hidden = false;
  $('#pageinfo').textContent = `第 ${i + 1} / ${state.pageList.length} 页`;
  const box = $('#pv');
  box.textContent = '';
  const d = state.open;
  const name = state.pageList[i];
  if (!d || !name) {
    box.append(el('p', { class: 'empty', text: '这一页没有缩略图（设备尚未上传）' }));
    return;
  }
  box.append(el('p', { class: 'empty', text: '载入中…' }));
  thumbUrl(d.uuid, name).then((url) => {
    if (state.page !== i || !state.open || state.open.uuid !== d.uuid) return;
    box.textContent = '';
    box.append(el('img', { src: url, alt: `第 ${i + 1} 页` }));
  }).catch((e) => {
    if (e.message === 'unauthorized') return;
    box.textContent = '';
    box.append(el('p', { class: 'empty', text: '页图加载失败：' + e.message }));
  });
}

/** 高清页：复用设备的 render 命令（POST /api/v1/tool/render，已有接口，鉴权不变）。 */
async function renderHd() {
  const d = state.open;
  if (!d) return;
  const idx = state.page;
  const box = $('#pv');
  status('正在让设备渲染第 ' + (idx + 1) + ' 页…');
  try {
    const res = await api('/api/v1/tool/render', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ uuid: d.uuid, page: idx }),
    });
    if (!res.ok) {
      const j = await readJson(res);
      throw new Error(j.error || ('HTTP ' + res.status));
    }
    const url = URL.createObjectURL(await res.blob());
    if (state.open !== d || state.page !== idx) return;
    box.textContent = '';
    box.append(el('img', { src: url, alt: `第 ${idx + 1} 页（设备渲染）` }));
    status('已换成设备渲染的高清页', 'ok');
  } catch (e) {
    if (e.message !== 'unauthorized') status('设备不在线或渲染失败：' + e.message, 'bad');
  }
}

function markActive() {
  for (const b of document.querySelectorAll('.doc')) {
    b.classList.toggle('on', !!state.open && b.dataset.uuid === state.open.uuid);
  }
}

// ---- 聊天（SSE over fetch）

function scrollLog() {
  const log = $('#log');
  log.scrollTop = log.scrollHeight;
}

function addMsg(who, text) {
  const log = $('#log');
  const empty = log.querySelector('.empty');
  if (empty) empty.remove();
  const m = el('div', { class: 'msg ' + who },
    el('div', { class: 'who' }, el('span', { text: who === 'user' ? '我' : 'AI' })),
    el('div', { class: 'body', text }));
  log.append(m);
  scrollLog();
  return m;
}

function addAnswer() {
  const log = $('#log');
  const empty = log.querySelector('.empty');
  if (empty) empty.remove();
  const text = el('div', { class: 'body' });
  const meta = el('div', { class: 'meta', text: '思考中…' });
  const msg = el('div', { class: 'msg ai' },
    el('div', { class: 'who' }, el('span', { text: 'AI' })),
    text, meta);
  log.append(msg);
  scrollLog();
  return { msg, text, meta };
}

function lock(busy) {
  $('#stop').disabled = !busy;
  $('#send').disabled = busy;
  $('#retry').disabled = busy || !state.last;
}

async function send() {
  const text = $('#q').value.trim();
  if (!text) return;
  if (!state.token) { status('先粘贴访问令牌', 'bad'); $('#cred').focus(); return; }
  const body = {
    mode: $('#mode').value,
    text,
    doc: state.open ? state.open.uuid : '',
    page: state.page,
  };
  $('#q').value = '';
  addMsg('user', text);
  const ans = addAnswer();
  state.last = { body, ans };
  await stream(body, ans);
}

async function stream(body, ans) {
  const ctrl = new AbortController();
  state.stream = ctrl;
  ans.text.textContent = '';
  ans.meta.textContent = '思考中…';
  lock(true);
  try {
    const res = await api('/api/v1/agent/ask', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: ctrl.signal,
    });
    if (!res.ok) {
      const j = await readJson(res);
      if (res.status === 503 && String(j.error || '').includes('busy')) {
        throw new Error('agent 忙，稍等再点重试');
      }
      throw new Error(j.error || ('HTTP ' + res.status));
    }
    const reader = res.body.getReader();
    const dec = new TextDecoder();
    let buf = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf('\n\n')) >= 0) {
        parseSse(buf.slice(0, i), ans);
        buf = buf.slice(i + 2);
      }
    }
    if (buf.trim()) parseSse(buf, ans);
    if (!ans.text.textContent.trim() && !ans.meta.textContent.includes('出错')) {
      ans.meta.textContent = ans.meta.textContent === '思考中…' ? '（没有返回内容）' : ans.meta.textContent;
    }
  } catch (e) {
    if (e.name === 'AbortError') ans.meta.textContent = '已停止';
    else if (e.message !== 'unauthorized') ans.meta.textContent = '出错：' + e.message;
  } finally {
    state.stream = null;
    lock(false);
    scrollLog();
  }
}

function parseSse(chunk, ans) {
  let event = 'message';
  let data = '';
  for (const raw of chunk.split('\n')) {
    const line = raw.trimEnd();
    if (line.startsWith('event:')) event = line.slice(6).trim();
    else if (line.startsWith('data:')) data += line.slice(5).trim();
  }
  if (!data) return;
  let j = {};
  try { j = JSON.parse(data); } catch { return; }
  if (event === 'token') {
    ans.text.textContent += j.t || '';   // 纯文本追加：$...$ 原样显示，不解析 markdown
    scrollLog();
  } else if (event === 'error') {
    ans.meta.textContent = '出错：' + (j.message || '未知错误');
  } else if (event === 'done') {
    const u = j.usage || {};
    ans.meta.textContent = `完成 · 输入 ${u.in || 0} / 输出 ${u.out || 0} tokens`;
  }
}

// ---- 事件绑定

function onKey(e) {
  const tag = (e.target.tagName || '').toLowerCase();
  if (tag === 'input' || tag === 'textarea' || tag === 'select') return;
  if ($('#viewer').hidden) return;
  if (e.key === 'ArrowLeft') { e.preventDefault(); showPage(state.page - 1); }
  else if (e.key === 'ArrowRight') { e.preventDefault(); showPage(state.page + 1); }
  else if (e.key === 'Escape') { $('#viewer').hidden = true; $('#pages').hidden = false; }
}

async function connect() {
  const t = $('#cred').value.trim();
  if (!t) return;
  writeToken(t);
  $('#cred').value = '';
  try {
    await loadLibrary();
  } catch (e) {
    if (e.message !== 'unauthorized') status('连接失败：' + e.message, 'bad');
  }
}

function forget() {
  writeToken('');
  resetView();
  $('#cred').placeholder = '粘贴访问令牌';
  $('#forget').hidden = true;
  status('已清除本机令牌');
}

function init() {
  $('#auth').addEventListener('submit', (e) => { e.preventDefault(); connect(); });
  $('#forget').addEventListener('click', forget);
  $('#reload').addEventListener('click', () => {
    if (!state.token) { $('#cred').focus(); return; }
    loadLibrary().catch((e) => {
      if (e.message !== 'unauthorized') status('刷新失败：' + e.message, 'bad');
    });
  });
  $('#ask').addEventListener('submit', (e) => { e.preventDefault(); send(); });
  $('#q').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  });
  $('#stop').addEventListener('click', () => { if (state.stream) state.stream.abort(); });
  $('#retry').addEventListener('click', () => {
    if (!state.last || state.stream) return;
    stream(state.last.body, state.last.ans);
  });
  $('#prev').addEventListener('click', () => showPage(state.page - 1));
  $('#next').addEventListener('click', () => showPage(state.page + 1));
  $('#back').addEventListener('click', () => { $('#viewer').hidden = true; $('#pages').hidden = false; });
  $('#hd').addEventListener('click', renderHd);
  document.addEventListener('keydown', onKey);
  lock(false);

  const saved = readToken();
  if (saved) {
    state.token = saved;
    $('#cred').placeholder = '已保存令牌，可粘贴新的覆盖';
    status('正在用本机保存的令牌连接…');
    loadLibrary().catch((e) => {
      if (e.message !== 'unauthorized') status('连接失败：' + e.message, 'bad');
    });
  } else {
    status('先粘贴访问令牌');
    $('#cred').focus();
  }
}

init();

/* ---- 错题档案（C13）------------------------------------------------------
 * 左栏第二个视图。数据 = GET /api/v1/profile（AI 扫页时按指纹去重登记）。
 * 「学会」= POST .../learned（幂等）；「删除」= DELETE。
 * 只读 + 两个写操作，全部走 api()（401 会统一清令牌）。 */
const quiz = { on: false, items: [] };

async function loadQuiz() {
  const box = $('#quiz');
  box.replaceChildren(el('p', { class: 'qempty', text: '读取中…' }));
  let list = [];
  try {
    const res = await api('/api/v1/profile?status=all&limit=100');
    const data = await res.json();
    list = data.problems || [];
  } catch (e) {
    if (String(e.message) !== 'unauthorized') {
      box.replaceChildren(el('p', { class: 'qempty', text: '读取失败：' + e.message }));
    }
    return;
  }
  quiz.items = list;
  if (!list.length) {
    box.replaceChildren(
      el('p', {
        class: 'qempty',
        text: '还没有登记错题。在设备上打开一页作业，用「设置 → Grade page」让它批一遍，就会记在这里。',
      }),
    );
    return;
  }
  box.replaceChildren(...list.map(quizCard));
}

function quizCard(p) {
  const learned = p.status === 'learned';
  const badge = (txt, cls) => el('span', { class: 'badge' + (cls ? ' ' + cls : ''), text: txt });
  const act = (label, fn) => el('button', { class: 'ghost', type: 'button', text: label, onclick: fn });
  return el(
    'div',
    { class: 'qcard' + (learned ? ' done' : '') },
    el(
      'div',
      { class: 'qhead' },
      badge(p.priority === 'high' ? '高优先' : p.priority === 'low' ? '低优先' : '普通'),
      badge(p.status === 'learned' ? '已学会' : '未掌握', p.status === 'learned' ? '' : 'open'),
      p.repeat_count > 1 ? badge('错 ' + p.repeat_count + ' 次') : null,
      p.difficulty ? badge('难度 ' + p.difficulty) : null,
      p.subject ? badge(p.subject) : null,
    ),
    el('p', { class: 'qtext', text: p.text || '' }),
    el(
      'div',
      { class: 'qacts' },
      learned ? null : act('标记学会', () => quizLearn(p.id)),
      act('删除', () => quizDelete(p.id)),
    ),
  );
}

async function quizLearn(id) {
  const res = await api('/api/v1/profile/' + id + '/learned', { method: 'POST' });
  if (res.ok) loadQuiz();
}

async function quizDelete(id) {
  const res = await api('/api/v1/profile/' + id, { method: 'DELETE' });
  if (res.ok) loadQuiz();
}

function showTab(notes) {
  quiz.on = !notes;
  $('#tabNotes').classList.toggle('on', notes);
  $('#tabQuiz').classList.toggle('on', !notes);
  $('#tabNotes').setAttribute('aria-selected', String(notes));
  $('#tabQuiz').setAttribute('aria-selected', String(!notes));
  $('#tree').hidden = !notes;
  $('#quiz').hidden = notes;
  if (!notes) loadQuiz();
}

document.addEventListener('DOMContentLoaded', () => {
  $('#tabNotes').addEventListener('click', () => showTab(true));
  $('#tabQuiz').addEventListener('click', () => showTab(false));
});
