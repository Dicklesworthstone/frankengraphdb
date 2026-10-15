import { LIMITS, colorIndex, graphQuery, queryBody, parseResponse, boundedText, extractGraph, filterGraph, cellText, csv } from './model.mjs';
import { demoResult } from './demo.mjs';
import { GraphCanvas } from './graph.mjs';

const $ = id => document.getElementById(id);
const paths = {
  graph: 'M4 5h0m0 0 14 7-14 7V5Zm-2 0a2 2 0 1 0 4 0 2 2 0 1 0-4 0m14 7a2 2 0 1 0 4 0 2 2 0 1 0-4 0M2 19a2 2 0 1 0 4 0 2 2 0 1 0-4 0',
  node: 'M8 12a4 4 0 1 0 8 0 4 4 0 1 0-8 0M12 2v3m0 14v3M2 12h3m14 0h3M5 5l2 2m10 10 2 2M5 19l2-2M17 7l2-2',
  link: 'm9 15 6-6M8 16l-1 1a4 4 0 0 1-6-6l4-4a4 4 0 0 1 6 0m2 10a4 4 0 0 0 6 0l4-4a4 4 0 0 0-6-6l-1 1',
  layers: 'm12 3 9 5-9 5-9-5 9-5Zm-9 9 9 5 9-5M3 16l9 5 9-5',
  clock: 'M3 12a9 9 0 1 0 18 0 9 9 0 1 0-18 0m9-5v5l3 2',
  code: 'm8 6-6 6 6 6m8-12 6 6-6 6m-3-14-2 16',
  spark: 'm12 3 2.5 6.5L21 12l-6.5 2.5L12 21l-2.5-6.5L3 12l6.5-2.5L12 3Z',
  shield: 'm12 3 8 3v6c0 5-8 9-8 9s-8-4-8-9V6l8-3Zm-4 9 3 3 5-6',
  sun: 'M8 12a4 4 0 1 0 8 0 4 4 0 1 0-8 0M12 1v2m0 18v2M1 12h2m18 0h2M4 4l2 2m12 12 2 2M4 20l2-2M18 6l2-2',
  moon: 'M20 14A8 8 0 0 1 10 4a8 8 0 1 0 10 10Z',
  help: 'M3 12a9 9 0 1 0 18 0 9 9 0 1 0-18 0m6-3a3 3 0 0 1 6 0c0 2-3 2-3 4m0 3h.01',
  workspace: 'M3 4h18v16H3V4Zm0 5h18M8 9v11',
  search: 'M3 10a7 7 0 1 0 14 0 7 7 0 1 0-14 0m12 5 6 6',
  chevron: 'm7 10 5 5 5-5',
  download: 'M12 3v12m-5-5 5 5 5-5M4 16v5h16v-5',
  tag: 'M3 3h8l10 10-8 8L3 11V3Zm4 4h.01',
  activity: 'M2 12h4l3-8 6 16 3-8h4',
  table: 'M3 4h18v16H3V4Zm0 5h18M3 14h18M9 4v16',
  type: 'M4 5h16M12 5v15m-4 0h8M4 5v3m16-3v3',
  panel: 'M3 4h18v16H3V4Zm12 0v16',
  plus: 'M12 5v14M5 12h14', minus: 'M5 12h14',
  fit: 'M8 3H3v5m13-5h5v5M3 16v5h5m13-5v5h-5M8 8h8v8H8V8Z',
  refresh: 'M20 7v5h-5M4 17v-5h5M6 6a8 8 0 0 1 13 3M5 15a8 8 0 0 0 13 3',
  pause: 'M8 5v14M16 5v14', stop: 'M6 6h12v12H6V6Z',
  play: 'm8 4 12 8-12 8V4Z', close: 'm6 6 12 12M6 18 18 6',
  terminal: 'm5 6 6 6-6 6m9 0h5', braces: 'M8 3H6v6l-3 3 3 3v6h2M16 3h2v6l3 3-3 3v6h-2',
  database: 'M3 6c0-4 18-4 18 0s-18 4-18 0Zm0 0v12c0 4 18 4 18 0V6M3 12c0 4 18 4 18 0',
  arrow: 'M4 12h16m-6-6 6 6-6 6', copy: 'M8 8h13v13H8V8ZM4 16H3V3h13v1',
  image: 'M3 3h18v18H3V3Zm0 14 6-6 4 4 3-3 5 5M7 7h.01',
  person: 'M8 7a4 4 0 1 0 8 0 4 4 0 1 0-8 0M4 21v-3a8 8 0 0 1 16 0v3',
};
function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24'); svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('fill', 'none'); svg.setAttribute('stroke', 'currentColor'); svg.setAttribute('stroke-width', '1.5');
  svg.setAttribute('stroke-linecap', 'round'); svg.setAttribute('stroke-linejoin', 'round');
  const path = document.createElementNS(svg.namespaceURI, 'path'); path.setAttribute('d', paths[name] || paths.node); svg.append(path); return svg;
}
for (const element of document.querySelectorAll('[data-icon]')) element.replaceChildren(icon(element.dataset.icon));
function el(tag, text = '', className = '') { const element = document.createElement(tag); element.textContent = text; element.className = className; return element; }
function action(text, callback, className = '', glyph = '') { const button = el('button', '', className); button.type = 'button'; if (glyph) button.append(icon(glyph)); button.append(el('span', text)); button.addEventListener('click', callback); return button; }
function swatch(group) { return el('span', '', `swatch swatch-${colorIndex(group)}`); }
let toastTimer;
function toast(message) { $('toast').textContent = message; $('toast').hidden = false; clearTimeout(toastTimer); toastTimer = setTimeout(() => { $('toast').hidden = true; }, 5000); }
let session = null, sample = true, result = null, raw = '', graph = { nodes: [], edges: [], warnings: [] };
let schema = { labels: [], relations: [], properties: [] }, history = [], selection = null, requestId = 0, controller = null;
let currentPage = 'explore', currentView = 'graph', hiddenLabels = new Set(), tablePage = 0, tableSort = null, sortAscending = true;
let queryDuration = null, templateKind = 'edges', connecting = false;
const pageSize = 50;
const renderer = new GraphCanvas($('graph'), $('minimap'), {
  onSelect: value => { selection = value; renderInspector(); renderNodeList(); },
  onBusy: busy => { $('layout-status').hidden = !busy; $('layout-stop').disabled = !busy; },
  onError: toast,
});
function setTheme(theme) {
  document.documentElement.dataset.theme = theme;
  $('theme-toggle').replaceChildren(icon(theme === 'dark' ? 'sun' : 'moon'));
  $('theme-toggle').setAttribute('aria-label', `Switch to ${theme === 'dark' ? 'light' : 'dark'} theme`);
  try { localStorage.setItem('fgdb-studio-theme', theme); } catch { /* Optional preference only. */ }
  renderer.drawSoon();
}
try { if (localStorage.getItem('fgdb-studio-theme') === 'light') setTheme('light'); } catch { /* Private browser storage may be disabled. */ }
$('theme-toggle').onclick = () => setTheme(document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark');
function dialog(id) { if (!$(id).open) $(id).showModal(); }
for (const button of document.querySelectorAll('[data-close]')) button.addEventListener('click', () => $(button.dataset.close).close());
for (const modal of document.querySelectorAll('dialog')) modal.addEventListener('click', event => { if (event.target === modal) { const rect = modal.getBoundingClientRect(); if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) modal.close(); } });
function showPage(page) {
  currentPage = page;
  for (const id of ['explore', 'schema', 'history']) {
    $(id + '-page').hidden = page !== id; $('nav-' + id).classList.toggle('active', page === id);
    if (page === id) $('nav-' + id).setAttribute('aria-current', 'page'); else $('nav-' + id).removeAttribute('aria-current');
  }
  const titles = { explore: ['Graph explorer', 'A different perspective on your data. Follow an idea, find a connection.', 'Explorer'], schema: ['Schema browser', 'The names available to your capability. A map of what you can query.', 'Schema'], history: ['Query history', 'Pick up where you left off, without leaving a trace on disk.', 'History'] }[page];
  $('page-title').replaceChildren(document.createTextNode(titles[0]), el('span', '.', 'heading-dot'));
  $('page-subtitle').textContent = titles[1]; $('breadcrumb-page').textContent = titles[2];
  if (page === 'schema') renderSchema(); if (page === 'history') renderHistory();
  if (page === 'explore') requestAnimationFrame(() => renderer.resize());
}
for (const button of document.querySelectorAll('[data-page]')) button.addEventListener('click', () => showPage(button.dataset.page));
function showView(view) {
  currentView = view;
  for (const name of ['graph', 'table', 'json']) {
    $('view-' + name).hidden = view !== name;
    $('tab-' + name).classList.toggle('active', view === name);
    $('tab-' + name).setAttribute('aria-selected', String(view === name)); $('tab-' + name).tabIndex = view === name ? 0 : -1;
  }
  for (const id of ['layout-mode', 'toggle-labels']) $(id).disabled = view !== 'graph';
  if (view === 'graph') requestAnimationFrame(() => renderer.resize());
  if (view === 'table') renderTable(); if (view === 'json') renderJSON();
}
for (const tab of document.querySelectorAll('[data-view]')) {
  tab.onclick = () => showView(tab.dataset.view);
  tab.onkeydown = event => {
    const names = ['graph', 'table', 'json'], at = names.indexOf(tab.dataset.view);
    const next = event.key === 'ArrowRight' ? (at + 1) % 3 : event.key === 'ArrowLeft' ? (at + 2) % 3 : event.key === 'Home' ? 0 : event.key === 'End' ? 2 : -1;
    if (next >= 0) { event.preventDefault(); showView(names[next]); $('tab-' + names[next]).focus(); }
  };
}
function showConnection() {
  $('connection-origin').textContent = location.origin;
  $('connection-error').hidden = true; $('disconnect-button').hidden = !session;
  if (session) $('database-name').value = session.name;
  dialog('connect-dialog');
}
$('connect-button').onclick = showConnection;
function closeConnection() {
  $('capability-token').value = '';
  if (connecting) { cancelRequest(false); connecting = false; $('connect-submit').disabled = false; }
}
$('connect-dialog').addEventListener('cancel', closeConnection);
$('connect-dialog').addEventListener('close', closeConnection);
function setBusy(busy) {
  $('run-query').disabled = busy;
  $('cancel-query').hidden = !busy;
  $('run-label').textContent = busy ? 'Running…' : session ? 'Run query' : 'Connect to run';
}
function cancelRequest(notify = true) {
  requestId++; controller?.abort(); controller = null; setBusy(false);
  if (notify) { $('query-status').textContent = 'Request cancelled in this browser. Server-side cancellation is not guaranteed.'; toast('Browser request cancelled.'); }
}
function clearResults() {
  result = null; raw = ''; graph = { nodes: [], edges: [], warnings: [] }; selection = null;
  queryDuration = null; hiddenLabels = new Set(); tablePage = 0; tableSort = null; sortAscending = true;
  renderer.focus = false; $('node-search').value = ''; $('table-filter').value = '';
  renderer.setData(graph); renderResult();
}
function clearSession() {
  cancelRequest(false); session = null; sample = false; schema = { labels: [], relations: [], properties: [] }; history = [];
  $('capability-token').value = ''; $('query-parameters').value = '{}'; $('history-count').textContent = '0';
  clearResults(); updateConnection(); renderSchema(); renderHistory();
}
function updateConnection() {
  $('workspace-name').textContent = sample ? 'Sample workspace' : session?.name || 'Local workspace';
  $('connection-label').textContent = session ? session.name : 'Connect database';
  $('connection-dot').classList.toggle('live', Boolean(session));
  $('dataset-badge').textContent = sample ? 'SAMPLE DATA' : session ? 'QUERY RESULT' : 'NOT CONNECTED';
  $('dataset-badge').className = `badge ${session ? 'live' : 'sample'}`;
  $('footer-status').textContent = sample ? 'Sample workspace · not connected' : session ? 'Connected · same-origin API' : 'Not connected';
  $('run-label').textContent = session ? 'Run query' : 'Connect to run';
  $('schema-note').textContent = sample ? 'Sample schema. Connect to see the label, relationship, and property names your capability permits.' : session ? 'Names supplied by the capability-scoped schema endpoint. These are bindings, not population counts.' : 'Connect to your local instance to inspect the names your capability allows.';
}
async function api(path, token, body, signal) {
  const response = await fetch(path, { method: body === undefined ? 'GET' : 'POST', headers: { Accept: 'application/json', Authorization: `Bearer ${token}`, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) }, body, signal, credentials: 'omit', redirect: 'error', cache: 'no-store', referrerPolicy: 'no-referrer' });
  try {
    const text = await boundedText(response);
    let data;
    try { data = parseResponse(text); } catch (error) { throw new Error(`Cannot read the server response: ${error.message}`); }
    if (!response.ok || data?.error) throw new Error(`${data?.error?.code || response.status}: ${data?.error?.message || 'Request refused'}`);
    if (!data || data.v !== 1) throw new Error('Studio requires the v1 HTTP JSON API.');
    return { data, text };
  } catch (error) {
    // Status remains authoritative even if an intermediary supplies HTML,
    // invalid UTF-8 or an oversized body instead of the native JSON refusal.
    if (!response.ok) error.status = response.status;
    throw error;
  }
}
$('connect-form').addEventListener('submit', async event => {
  event.preventDefault();
  const name = $('database-name').value.trim(), token = $('capability-token').value.trim();
  if (!/^[A-Za-z0-9_.-]{1,128}$/.test(name)) { $('connection-error').textContent = 'Use a database name containing letters, digits, dots, underscores, or hyphens.'; $('connection-error').hidden = false; return; }
  if (!/^(?:[0-9a-fA-F]{2}){1,8192}$/.test(token)) { $('connection-error').textContent = 'Paste an even-length hexadecimal capability token.'; $('connection-error').hidden = false; return; }
  if (!['http:', 'https:'].includes(location.protocol)) { $('connection-error').textContent = 'Open /studio/ from the fgdbd HTTP listener, not as a file.'; $('connection-error').hidden = false; return; }
  const draft = $('query-editor').value, parameters = $('query-parameters').value;
  clearSession(); $('query-editor').value = draft; $('query-parameters').value = parameters;
  connecting = true; $('connect-submit').disabled = true; $('connection-error').hidden = true;
  controller = new AbortController(); const localController = controller, signal = localController.signal, ticket = ++requestId;
  const timeout = setTimeout(() => localController.abort(), 30000);
  try {
    const { data } = await api(`/v1/databases/${encodeURIComponent(name)}/schema`, token, undefined, signal);
    if (ticket !== requestId) return;
    for (const key of ['labels', 'relations', 'properties']) if (!Array.isArray(data[key]) || data[key].length > 10000 || !data[key].every(value => typeof value === 'string')) throw new Error('Invalid or oversized schema response.');
    session = { name, token }; schema = data; connecting = false; controller = null; $('connect-submit').disabled = false;
    $('capability-token').value = ''; $('connect-dialog').close(); updateConnection(); renderSchema();
    clearTimeout(timeout); await runQuery();
  } catch (error) {
    if (ticket !== requestId) return;
    $('connection-error').textContent = error.name === 'AbortError' ? 'Connection timed out or was cancelled.' : error.message;
    $('connection-error').hidden = false;
  } finally {
    clearTimeout(timeout);
    if (ticket === requestId) { connecting = false; controller = null; $('connect-submit').disabled = false; }
  }
});
$('disconnect-button').onclick = () => {
  clearSession(); $('query-editor').value = graphQuery(); highlight(); $('connect-dialog').close();
  $('query-status').textContent = 'Disconnected. Credentials, results, and session history cleared.'; toast('Disconnected. This tab’s graph data and token were cleared.');
};
async function runQuery() {
  if (!session) { showConnection(); return; }
  let body;
  const statement = $('query-editor').value, parameters = $('query-parameters').value;
  try { body = queryBody(statement, parameters); } catch (error) { $('query-status').textContent = error.message; $('query-status').classList.add('error'); return; }
  cancelRequest(false); const ticket = ++requestId; controller = new AbortController(); const localController = controller;
  const activeSession = session; clearResults(); showPage('explore'); setBusy(true);
  $('query-status').classList.remove('error'); $('query-status').textContent = 'Running through the read-only query endpoint…';
  const started = performance.now(), timeout = setTimeout(() => localController.abort(), 30000);
  try {
    const response = await api(`/v1/databases/${encodeURIComponent(activeSession.name)}/query`, activeSession.token, body, localController.signal);
    if (ticket !== requestId || activeSession !== session) return;
    const parsedGraph = extractGraph(response.data);
    result = response.data; raw = response.text; graph = parsedGraph;
    queryDuration = performance.now() - started; tablePage = 0; tableSort = null;
    renderer.setData(graph); renderResult();
    const entry = { statement, parameters, duration: queryDuration, rows: result.rows.length, seq: String(result.seq ?? '—'), at: new Date().toLocaleTimeString(), success: true };
    history.unshift(entry); history = history.slice(0, 30); $('history-count').textContent = String(history.length);
    $('query-status').textContent = `${result.rows.length.toLocaleString()} rows · ${Math.round(queryDuration)} ms round trip · snapshot ${entry.seq}`;
    if (!graph.nodes.length) showView('table');
  } catch (error) {
    if (ticket !== requestId) return;
    const message = error.name === 'AbortError' ? 'Browser request timed out after 30 seconds. Server-side cancellation is not guaranteed.' : error.message;
    history.unshift({ statement, parameters, duration: performance.now() - started, at: new Date().toLocaleTimeString(), success: false, error: message }); history = history.slice(0, 30);
    if ([401, 403, 404].includes(error.status)) clearSession();
    $('history-count').textContent = String(history.length); $('query-status').textContent = message; $('query-status').classList.add('error');
    toast(message);
  } finally { clearTimeout(timeout); if (ticket === requestId) { controller = null; setBusy(false); } }
}
$('run-query').onclick = runQuery; $('cancel-query').onclick = () => cancelRequest();
function renderResult() {
  $('metric-nodes').textContent = result ? graph.nodes.length.toLocaleString() : '—';
  $('metric-edges').textContent = result ? graph.edges.length.toLocaleString() : '—';
  $('metric-labels').textContent = result ? new Set(graph.nodes.flatMap(node => node.labels)).size.toLocaleString() : '—';
  $('metric-time').textContent = queryDuration == null ? '—' : `${Math.round(queryDuration)} ms`;
  $('metric-time-caption').textContent = sample ? 'sample data' : queryDuration == null ? 'no query yet' : 'this query';
  $('graph-context').textContent = sample ? 'Research atlas' : result ? `${session?.name || 'Graph'} · snapshot ${String(result.seq ?? '—')}` : 'Connect to explore';
  $('snapshot-status').textContent = sample || !result ? 'No database snapshot' : `SNAPSHOT ${String(result.seq ?? '—')}`;
  $('empty-state').hidden = graph.nodes.length > 0;
  $('graph-notice').hidden = !graph.warnings.length; $('graph-notice').textContent = graph.warnings.join(' ');
  $('export-button').disabled = !result;
  renderLabels(); applyFilters(); renderInspector();
  if (currentView === 'table') renderTable(); if (currentView === 'json') renderJSON();
}
function renderLabels() {
  const counts = new Map();
  for (const node of graph.nodes) counts.set(node.group, (counts.get(node.group) || 0) + 1);
  $('label-list').replaceChildren();
  for (const [group, count] of [...counts].sort(([a], [b]) => a.localeCompare(b))) {
    const button = el('button', '', 'label-filter'); button.type = 'button'; button.setAttribute('aria-pressed', String(!hiddenLabels.has(group)));
    button.setAttribute('aria-label', `${group}: ${count} nodes. Toggle visibility.`);
    button.append(swatch(group), el('span', group), el('span', String(count), 'count'));
    button.onclick = () => { if (hiddenLabels.has(group)) hiddenLabels.delete(group); else hiddenLabels.add(group); renderLabels(); applyFilters(); };
    $('label-list').append(button);
  }
  if (!counts.size) $('label-list').append(el('p', 'No labels to display.', 'sidebar-note'));
}
function applyFilters() {
  const visible = filterGraph(graph, hiddenLabels, $('node-search').value);
  renderer.setVisible(visible);
  if (result) {
    $('metric-nodes').textContent = visible.nodes.length.toLocaleString();
    $('metric-edges').textContent = visible.edges.length.toLocaleString();
    $('metric-labels').textContent = new Set(visible.nodes.flatMap(node => node.labels)).size.toLocaleString();
  }
  if (selection?.kind === 'node' && !visible.nodes.some(node => node.id === selection.id)) renderer.select(null);
  $('empty-state').hidden = visible.nodes.length > 0;
  renderNodeList();
}
$('reset-filters').onclick = () => { hiddenLabels.clear(); $('node-search').value = ''; renderLabels(); applyFilters(); };
$('node-search').oninput = applyFilters;
function selectNode(id) {
  const node = graph.nodes.find(node => node.id === id);
  if (!node) { toast('This node is not mapped on the bounded canvas. Its exact identity remains in the table.'); return; }
  hiddenLabels.delete(node.group); renderLabels(); applyFilters(); showPage('explore'); showView('graph');
  if (matchMedia('(max-width: 800px)').matches) { document.body.classList.add('inspector-open'); $('toggle-inspector').setAttribute('aria-expanded', 'true'); }
  document.body.classList.remove('inspector-hidden'); renderer.select({ kind: 'node', id }, true);
  syncInspectorState();
}
function syncInspectorState() {
  const visible = matchMedia('(max-width: 800px)').matches ? document.body.classList.contains('inspector-open') : !document.body.classList.contains('inspector-hidden');
  $('toggle-inspector').setAttribute('aria-expanded', String(visible));
}
matchMedia('(max-width: 800px)').addEventListener('change', syncInspectorState);
syncInspectorState();
function renderNodeList() {
  const visible = filterGraph(graph, hiddenLabels, $('node-search').value);
  const found = visible.nodes.filter(node => visible.matches.has(node.id));
  $('directory-count').textContent = String(found.length); $('node-list').replaceChildren();
  for (const node of found.slice(0, 100)) {
    const button = el('button', '', `node-option${selection?.kind === 'node' && selection.id === node.id ? ' active' : ''}`);
    button.type = 'button'; button.append(swatch(node.group), el('span', node.title), el('small', String(node.degree)));
    button.title = `${node.title} · vertex ${node.id}`; button.onclick = () => selectNode(node.id); $('node-list').append(button);
  }
  if (found.length > 100) $('node-list').append(el('p', 'Showing 100 matches. Refine your search.', 'sidebar-note'));
  if (!found.length) $('node-list').append(el('p', 'No matching nodes.', 'sidebar-note'));
}
function renderInspector() {
  const host = $('inspector-detail'); host.replaceChildren();
  const item = selection?.kind === 'node' ? graph.nodes.find(node => node.id === selection.id) : selection?.kind === 'edge' ? graph.edges.find(edge => edge.id === selection.id) : null;
  if (!item) {
    const placeholder = el('div', '', 'inspector-placeholder');
    const symbol = el('span'); symbol.append(icon('graph')); placeholder.append(symbol, el('h3', 'Follow a connection.'), el('p', 'Select a node or relationship to inspect its identity, properties, and connections in this result.'));
    host.append(placeholder); return;
  }
  const isNode = selection.kind === 'node', group = isNode ? item.group : item.label || 'Relationship';
  const badge = el('span', group, `badge element-kind swatch-${colorIndex(group)}`);
  const avatar = el('div', '', `element-avatar swatch-${colorIndex(group)}`); avatar.append(icon(isNode ? (group === 'Person' ? 'person' : 'node') : 'link'));
  const identity = el('div', '', 'identity'); identity.append(el('span', isNode ? 'VERTEX' : 'EDGE'), el('code', item.id));
  const copy = action('', () => copyText(item.id), 'icon-button', 'copy'); copy.setAttribute('aria-label', 'Copy exact element identity'); identity.append(copy);
  host.append(badge, avatar, el('h3', isNode ? item.title : item.label || `Relationship ${item.id}`, 'element-title'), identity);
  if (!isNode) host.append(el('p', item.directed ? 'Stored direction: source → target' : 'Path adjacency only. Stored direction is not supplied.', 'sidebar-note'));
  const properties = el('dl', '', 'property-list');
  for (const [key, value] of Object.entries(item.properties).slice(0, 100)) {
    const text = typeof value === 'object' ? JSON.stringify(value) : String(value);
    const pair = el('div', '', 'property-pair'); pair.append(el('dt', key), el('dd', text.length > 2000 ? text.slice(0, 2000) + '… (full value in JSON)' : text)); properties.append(pair);
  }
  if (!Object.keys(item.properties).length) properties.append(el('p', 'No property metadata was returned for this element.', 'sidebar-note'));
  if (Object.keys(item.properties).length > 100) properties.append(el('p', 'First 100 properties shown. JSON retains the full result.', 'sidebar-note'));
  host.append(properties);
  const neighbors = isNode ? graph.edges.filter(edge => edge.source === item.id || edge.target === item.id).map(edge => ({ edge, id: edge.source === item.id ? edge.target : edge.source })) : [{ id: item.source, edge: item }, { id: item.target, edge: item }];
  host.append(el('div', `${neighbors.length} CONNECTIONS IN RESULT`, 'neighbor-title'));
  for (const { id, edge } of neighbors.slice(0, 16)) {
    const node = graph.nodes.find(node => node.id === id); if (!node) continue;
    const button = el('button', '', 'neighbor-link'); button.type = 'button'; button.append(swatch(node.group), el('span', node.title)); button.title = edge.label || 'Relationship'; button.onclick = () => selectNode(id); host.append(button);
  }
  if (neighbors.length > 16) host.append(el('p', `${neighbors.length - 16} more connections on the canvas.`, 'sidebar-note'));
  if (isNode) {
    const focus = action(renderer.focus ? 'Show full context' : 'Focus neighborhood', () => { renderer.focus = !renderer.focus; renderer.drawSoon(); renderInspector(); }, 'secondary-button focus-button', 'fit');
    focus.setAttribute('aria-pressed', String(renderer.focus)); host.append(focus);
  }
}
$('clear-selection').onclick = () => { renderer.select(null); if (matchMedia('(max-width: 800px)').matches) document.body.classList.remove('inspector-open'); syncInspectorState(); };
$('toggle-inspector').onclick = () => {
  if (matchMedia('(max-width: 800px)').matches) { document.body.classList.toggle('inspector-open'); $('toggle-inspector').setAttribute('aria-expanded', String(document.body.classList.contains('inspector-open'))); }
  else { document.body.classList.toggle('inspector-hidden'); $('toggle-inspector').setAttribute('aria-expanded', String(!document.body.classList.contains('inspector-hidden'))); }
};
$('fit').onclick = () => renderer.fit(); $('zoom-in').onclick = () => renderer.zoom(1.2); $('zoom-out').onclick = () => renderer.zoom(1 / 1.2);
$('arrange').onclick = () => renderer.arrange(); $('layout-stop').onclick = () => renderer.stop(); $('layout-mode').onchange = () => renderer.arrange($('layout-mode').value);
$('toggle-labels').onclick = () => { renderer.labels = !renderer.labels; $('toggle-labels').classList.toggle('pressed', renderer.labels); $('toggle-labels').setAttribute('aria-pressed', String(renderer.labels)); renderer.drawSoon(); };
function compareCells(a, b) {
  if (a.type === 'null' || b.type === 'null') return a.type === b.type ? 0 : a.type === 'null' ? 1 : -1;
  if (['int', 'wideint', 'count'].includes(a.type) && ['int', 'wideint', 'count'].includes(b.type) && /^-?\d+$/.test(a.value) && /^-?\d+$/.test(b.value)) { const x = BigInt(a.value), y = BigInt(b.value); return x < y ? -1 : x > y ? 1 : 0; }
  return cellText(a).localeCompare(cellText(b), undefined, { numeric: true });
}
function renderTable() {
  const head = $('result-table').querySelector('thead'), body = $('result-table').querySelector('tbody'); head.replaceChildren(); body.replaceChildren();
  const filter = $('table-filter').value.toLowerCase(), rows = result ? result.rows.map((row, ordinal) => ({ row, ordinal })).filter(({ row }) => !filter || row.some(cell => cellText(cell).toLowerCase().includes(filter))) : [];
  if (tableSort !== null) rows.sort((a, b) => (sortAscending ? 1 : -1) * compareCells(a.row[tableSort], b.row[tableSort]) || a.ordinal - b.ordinal);
  const pages = Math.max(1, Math.ceil(rows.length / pageSize)); tablePage = Math.max(0, Math.min(tablePage, pages - 1));
  const header = el('tr'); header.append(el('th', '#'));
  for (const [i, column] of (result?.columns || []).entries()) {
    const th = el('th'); th.setAttribute('scope', 'col'); th.setAttribute('aria-sort', tableSort === i ? (sortAscending ? 'ascending' : 'descending') : 'none');
    const button = action(column + (tableSort === i ? (sortAscending ? ' ↑' : ' ↓') : ''), () => { sortAscending = tableSort === i ? !sortAscending : true; tableSort = i; tablePage = 0; renderTable(); });
    button.title = 'Sort received rows in this browser'; th.append(button); header.append(th);
  }
  head.append(header);
  for (const { row, ordinal } of rows.slice(tablePage * pageSize, (tablePage + 1) * pageSize)) {
    const tr = el('tr'); tr.append(el('td', String(ordinal + 1), 'row-number'));
    for (const cell of row) {
      const td = el('td'); const text = cellText(cell); td.title = `${cell.type}: ${text.slice(0, 2000)}`;
      if (cell.type === 'vertex') td.append(action(`◉ ${cell.value}`, () => selectNode(cell.value), 'cell-entity'));
      else if (cell.type === 'edge' && graph.edges.some(edge => edge.id === cell.value)) td.append(action(`↗ ${cell.value}`, () => { showView('graph'); document.body.classList.remove('inspector-hidden'); if (matchMedia('(max-width: 800px)').matches) document.body.classList.add('inspector-open'); syncInspectorState(); renderer.select({ kind: 'edge', id: cell.value }); }, 'cell-entity'));
      else { td.textContent = text; if (cell.type === 'null') td.className = 'cell-null'; }
      tr.append(td);
    }
    body.append(tr);
  }
  $('table-count').textContent = `${rows.length.toLocaleString()} rows`; $('table-empty').hidden = rows.length > 0;
  $('page-info').textContent = rows.length ? `${tablePage * pageSize + 1}–${Math.min(rows.length, (tablePage + 1) * pageSize)} of ${rows.length.toLocaleString()}` : 'No rows';
  $('previous-page').disabled = tablePage === 0; $('next-page').disabled = tablePage >= pages - 1;
}
$('table-filter').oninput = () => { tablePage = 0; renderTable(); };
$('previous-page').onclick = () => { tablePage--; renderTable(); }; $('next-page').onclick = () => { tablePage++; renderTable(); };
function formattedJSON(text) {
  let output = '', indent = 0, quoted = false, escaped = false;
  for (const character of text.slice(0, 200000)) {
    if (quoted) { output += character; if (escaped) escaped = false; else if (character === '\\') escaped = true; else if (character === '"') quoted = false; }
    else if (character === '"') { quoted = true; output += character; }
    else if (character === '{' || character === '[') { indent++; output += character + '\n' + '  '.repeat(Math.min(indent, 40)); }
    else if (character === '}' || character === ']') { indent = Math.max(0, indent - 1); output += '\n' + '  '.repeat(Math.min(indent, 40)) + character; }
    else if (character === ',') output += ',\n' + '  '.repeat(Math.min(indent, 40));
    else if (character === ':') output += ': ';
    else if (!/\s/.test(character)) output += character;
  }
  return output + (text.length > 200000 ? '\n\n… Preview limited to 200,000 characters. Export Original JSON for the complete response.' : '');
}
function renderJSON() { $('json-output').textContent = raw ? formattedJSON(raw) : 'Run a query to inspect the original response.'; }
async function copyText(text) { try { await navigator.clipboard.writeText(text); toast('Copied to clipboard.'); } catch { toast('Clipboard unavailable. Use Export, or select and copy the text.'); } }
$('copy-json').onclick = () => raw && copyText(raw);
function download(blob, name) {
  if (!blob) { toast('There is nothing to export yet.'); return; }
  const url = URL.createObjectURL(blob), a = document.createElement('a'); a.href = url; a.download = name; document.body.append(a); a.click(); a.remove(); setTimeout(() => URL.revokeObjectURL(url), 1000);
}
$('export-button').onclick = () => dialog('export-dialog');
$('export-json').onclick = () => { if (result) download(new Blob([raw], { type: 'application/json' }), sample ? 'frankengraph-sample.json' : 'frankengraph-result.json'); $('export-dialog').close(); };
$('export-csv').onclick = () => { if (result) download(new Blob([csv(result)], { type: 'text/csv;charset=utf-8' }), sample ? 'frankengraph-sample.csv' : 'frankengraph-result.csv'); $('export-dialog').close(); };
$('export-png').onclick = async () => { $('export-dialog').close(); showView('graph'); await new Promise(resolve => requestAnimationFrame(resolve)); renderer.draw(); download(await renderer.image(), sample ? 'frankengraph-sample.png' : 'frankengraph-graph.png'); };
function loadTemplate(kind, label = '') {
  templateKind = kind;
  $('query-editor').value = kind === 'counts' ? 'MATCH (n)\nRETURN count(*) AS nodes' : graphQuery({ kind, label, limit: Number($('sample-limit').value) });
  $('query-parameters').value = '{}'; highlight(); showPage('explore'); $('query-editor').focus();
  $('query-status').textContent = sample ? 'Sample shown above. Connect to execute this query against your database.' : 'Query loaded. Run to replace the current result.';
}
for (const button of document.querySelectorAll('[data-template]')) button.onclick = () => loadTemplate(button.dataset.template);
$('sample-limit').onchange = () => loadTemplate(templateKind);
function highlight() {
  const source = $('query-editor').value; $('query-highlight').replaceChildren();
  const regex = /(\/\/[^\n]*|'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*"|`(?:``|[^`])*`|\$[A-Za-z_]\w*|\b(?:MATCH|RETURN|WHERE|WITH|AS|LIMIT|ORDER|BY|ASC|DESC|SKIP|UNWIND|CALL|YIELD|DISTINCT|OPTIONAL|AND|OR|NOT|IN|IS|NULL|TRUE|FALSE|FOR|SYSTEM_TIME|OF|SEQ)\b|\b\d+(?:\.\d+)?\b)/gi;
  let position = 0;
  for (const match of source.matchAll(regex)) {
    $('query-highlight').append(document.createTextNode(source.slice(position, match.index)));
    const token = match[0], style = token.startsWith('//') ? 'token-comment' : /^['"`]/.test(token) ? 'token-string' : token.startsWith('$') ? 'token-parameter' : /^\d/.test(token) ? 'token-number' : 'token-keyword';
    $('query-highlight').append(el('span', token, style)); position = match.index + token.length;
  }
  $('query-highlight').append(document.createTextNode(source.slice(position) + '\n'));
  $('line-numbers').textContent = Array.from({ length: source.split('\n').length }, (_, i) => String(i + 1)).join('\n');
  syncEditor();
}
function syncEditor() { $('query-highlight').scrollTop = $('query-editor').scrollTop; $('query-highlight').scrollLeft = $('query-editor').scrollLeft; $('line-numbers').scrollTop = $('query-editor').scrollTop; }
$('query-editor').oninput = () => {
  if ($('query-editor').value.length > 65536) { $('query-editor').value = $('query-editor').value.slice(0, 65536); toast('Editor limited to 64 KiB.'); }
  highlight();
};
$('query-editor').onscroll = syncEditor;
// Tab keeps its normal focus-navigation behavior; the editor is not a keyboard trap.
$('parameters-toggle').onclick = () => { $('parameters-panel').hidden = !$('parameters-panel').hidden; $('parameters-toggle').setAttribute('aria-expanded', String(!$('parameters-panel').hidden)); if (!$('parameters-panel').hidden) $('query-parameters').focus(); };
$('query-parameters').maxLength = 65536;
function renderSchema() {
  $('schema-content').replaceChildren();
  for (const [key, title, glyph] of [['labels', 'Node labels', 'tag'], ['relations', 'Relationship types', 'link'], ['properties', 'Property keys', 'braces']]) {
    const card = el('section', '', 'schema-card'), heading = el('h2'); heading.append(icon(glyph), el('span', title), el('span', String(schema[key].length))); card.append(heading);
    const list = el('ul');
    for (const name of schema[key].slice(0, 1000)) {
      const item = el('li');
      if (key === 'labels') { const button = action(name, () => loadTemplate('nodes', name)); button.prepend(swatch(name)); button.title = `Load a read query for ${name}`; item.append(button); }
      else item.textContent = name;
      list.append(item);
    }
    if (!schema[key].length) list.append(el('li', 'No names available.'));
    if (schema[key].length > 1000) list.append(el('li', 'First 1,000 names shown.'));
    card.append(list); $('schema-content').append(card);
  }
}
function renderHistory() {
  $('history-list').replaceChildren();
  if (!history.length) {
    const empty = el('div', '', 'history-empty'); empty.append(icon('clock'), el('h2', 'Your next discovery starts here.'), el('p', 'Run a query against a connected database to start this session’s history.')); $('history-list').append(empty); return;
  }
  for (const entry of history) {
    const button = el('button', '', 'history-entry'); button.type = 'button';
    const symbol = el('span'); symbol.append(icon(entry.success ? 'code' : 'help'));
    const body = el('div', '', 'entry-body'); body.append(el('code', entry.statement));
    body.append(el('small', `${entry.at} · ${Math.round(entry.duration)} ms · ${entry.success ? `${entry.rows} rows · snapshot ${entry.seq}` : entry.error}`));
    button.append(symbol, body, icon('arrow')); button.onclick = () => { $('query-editor').value = entry.statement; $('query-parameters').value = entry.parameters; highlight(); showPage('explore'); $('query-status').textContent = 'History query loaded. Run to execute it again.'; }; $('history-list').append(button);
  }
}
$('clear-history').onclick = () => { history = []; $('history-count').textContent = '0'; renderHistory(); };
function loadSample() {
  clearSession(); sample = true; result = demoResult(); raw = JSON.stringify(result); graph = extractGraph(result);
  schema = { labels: [...new Set(graph.nodes.flatMap(node => node.labels))].sort(), relations: [...new Set(graph.edges.map(edge => edge.label))].sort(), properties: ['description', 'name', 'role', 'source', 'workspace'] };
  $('query-editor').value = graphQuery(); $('query-parameters').value = '{}'; templateKind = 'edges'; highlight();
  queryDuration = null; updateConnection(); renderer.setData(graph); renderResult(); renderer.select({ kind: 'node', id: '25' });
  $('query-status').classList.remove('error'); $('query-status').textContent = 'Sample workspace · connect a database to execute queries.';
  showPage('explore'); showView('graph');
}
$('load-sample').onclick = () => { loadSample(); toast('Sample workspace opened. Any previous connection was cleared.'); };
$('help-button').onclick = () => dialog('help-dialog');
const commands = [
  ['Connect database', 'database', showConnection], ['Graph explorer', 'graph', () => showPage('explore')], ['Schema browser', 'layers', () => showPage('schema')], ['Query history', 'clock', () => showPage('history')], ['Fit graph to screen', 'fit', () => { showPage('explore'); showView('graph'); renderer.fit(); }], ['Load relationship query', 'code', () => loadTemplate('edges')], ['Load node query', 'node', () => loadTemplate('nodes')], ['Switch color theme', 'sun', () => $('theme-toggle').click()], ['Help and shortcuts', 'help', () => dialog('help-dialog')],
];
function renderCommands() {
  const term = $('command-search').value.toLowerCase(); $('command-results').replaceChildren();
  const add = (title, glyph, callback, suffix) => { const button = action(title, () => { $('command-dialog').close(); callback(); }, 'command-result', glyph); button.append(el('small', suffix)); $('command-results').append(button); };
  for (const [title, glyph, callback] of commands.filter(([title]) => title.toLowerCase().includes(term))) add(title, glyph, callback, 'COMMAND');
  for (const node of graph.nodes.filter(node => `${node.title} ${node.id}`.toLowerCase().includes(term)).slice(0, 12)) add(node.title, 'node', () => selectNode(node.id), node.group);
  if (!$('command-results').children.length) $('command-results').append(el('p', 'No matching commands or nodes.', 'table-empty'));
}
function openCommands() { $('command-search').value = ''; renderCommands(); dialog('command-dialog'); $('command-search').focus(); }
$('command-button').onclick = openCommands; $('command-search').oninput = renderCommands;
$('command-search').onkeydown = event => { if (event.key === 'Enter') { event.preventDefault(); $('command-results').querySelector('button')?.click(); } if (event.key === 'ArrowDown') { event.preventDefault(); $('command-results').querySelector('button')?.focus(); } };
document.addEventListener('keydown', event => {
  const editing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName || '');
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); if (!document.querySelector('dialog[open]')) openCommands(); return; }
  if (document.querySelector('dialog[open]')) return;
  if ((event.metaKey || event.ctrlKey) && event.key === '1') { event.preventDefault(); showPage('explore'); return; }
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') { event.preventDefault(); if (!controller) runQuery(); return; }
  if (!editing && event.key === '/') { event.preventDefault(); showPage('explore'); document.body.classList.remove('inspector-hidden'); if (matchMedia('(max-width: 800px)').matches) document.body.classList.add('inspector-open'); syncInspectorState(); $('node-search').focus(); }
  if (!editing && event.key.toLowerCase() === 'f' && currentPage === 'explore' && currentView === 'graph') renderer.fit();
  if (!editing && event.key === 'Escape') renderer.select(null);
});
document.addEventListener('visibilitychange', () => { if (document.hidden) renderer.stop(); });
window.addEventListener('pagehide', () => { clearSession(); $('query-editor').value = ''; highlight(); });
loadSample();
