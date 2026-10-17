// Browser-side presentation of the existing robot JSON contract. No query engine.
export const LIMITS = Object.freeze({ bytes: 8 * 1024 * 1024, rows: 10000, nodes: 750, edges: 2000, cells: 200000 });
export const PALETTE = ['#64d8b0', '#a99aff', '#71b9f4', '#f0bc70', '#ed8eae', '#84d3db', '#bac97c', '#c4a396'];
export function hash(text) {
  let h = 2166136261;
  for (const character of String(text)) h = Math.imul(h ^ character.codePointAt(0), 16777619);
  return h >>> 0;
}
export function colorIndex(label) { return hash(label) % PALETTE.length; }
export function quoteIdentifier(name) { return '`' + String(name).replaceAll('`', '``') + '`'; }
export function graphQuery({ kind = 'edges', label = '', limit = 150 } = {}) {
  if (!Number.isInteger(limit) || limit < 1 || limit > 500) throw new Error('Choose a sample size from 1 to 500.');
  const node = label ? `(source:${quoteIdentifier(label)})` : '(source)';
  if (kind === 'nodes') return `MATCH ${node}\nRETURN source AS node, labels(source) AS node_labels,\n       properties(source) AS node_properties\nLIMIT ${limit}`;
  return `MATCH ${node}-[relationship]->(target)\nRETURN source, relationship, target,\n       labels(source) AS source_labels, properties(source) AS source_properties,\n       type(relationship) AS relationship_type,\n       properties(relationship) AS relationship_properties,\n       labels(target) AS target_labels, properties(target) AS target_properties\nLIMIT ${limit}`;
}
export function queryBody(statement, parameters = '{}') {
  if (!statement.trim()) throw new Error('Write a query first.');
  if (new TextEncoder().encode(statement).length > 65536 || new TextEncoder().encode(parameters).length > 65536) throw new Error('The editor accepts up to 64 KiB each for a query and its parameters.');
  const raw = parameters.trim() || '{}';
  const checked = JSON.parse(raw);
  if (!checked || Array.isArray(checked) || typeof checked !== 'object') throw new Error('Parameters must be a JSON object.');
  // Validate, but send the ORIGINAL JSON. JSON.stringify(checked) would round Int64 arguments.
  return `{"statement":${JSON.stringify(statement)},"parameters":${raw}}`;
}
export function parseResponse(text) {
  return JSON.parse(text, (_key, value, context) => {
    if (typeof value === 'number' && (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value)))) {
      if (context?.source && /^-?\d+$/.test(context.source)) return context.source;
      throw new Error('The browser cannot preserve this response number exactly. Use a current browser.');
    }
    return value;
  });
}
export async function boundedText(response, limit = LIMITS.bytes) {
  const declared = response.headers.get('content-length');
  if (declared && /^\d+$/.test(declared) && Number(declared) > limit) {
    await response.body?.cancel();
    throw new Error('Result exceeds the 8 MiB browser limit. Add a smaller LIMIT or return fewer properties.');
  }
  if (!response.body) return '';
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let bytes = 0, text = '';
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > limit) throw new Error('Result exceeds the 8 MiB browser limit. Add a smaller LIMIT or return fewer properties.');
      text += decoder.decode(value, { stream: true });
    }
    return text + decoder.decode();
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
}
export function validateResult(result) {
  if (result?.v !== 1 || !Array.isArray(result.columns) || !Array.isArray(result.rows)) throw new Error('The server did not return a v1 query result.');
  if (result.columns.length > 256 || !result.columns.every(x => typeof x === 'string')) throw new Error('Invalid result columns.');
  if (result.rows.length > LIMITS.rows) throw new Error('Result exceeds 10,000 rows. Add a LIMIT before sending it to Studio.');
  let cells = 0;
  const visit = (cell, depth) => {
    if (++cells > LIMITS.cells || depth > 32 || !cell || typeof cell.type !== 'string') throw new Error('Result exceeds the browser value bounds or contains an invalid cell.');
    if (cell.type === 'list') {
      if (!Array.isArray(cell.value)) throw new Error('Invalid list cell.');
      for (const item of cell.value) visit(item, depth + 1);
    } else if (cell.type === 'map') {
      if (!cell.value || typeof cell.value !== 'object' || Array.isArray(cell.value)) throw new Error('Invalid map cell.');
      for (const item of Object.values(cell.value)) visit(item, depth + 1);
    }
  };
  for (const row of result.rows) {
    if (!Array.isArray(row) || row.length !== result.columns.length) throw new Error('Result row width does not match its columns.');
    for (const cell of row) visit(cell, 0);
  }
  return result;
}
export function plain(cell, depth = 0) {
  if (!cell || depth > 32) return null;
  if (cell.type === 'null') return null;
  if (cell.type === 'list') return cell.value.map(value => plain(value, depth + 1));
  if (cell.type === 'map') {
    const result = Object.create(null);
    for (const [key, value] of Object.entries(cell.value)) result[key] = plain(value, depth + 1);
    return result;
  }
  return cell.value;
}
export function cellText(cell) {
  if (cell?.type === 'null') return 'null';
  const value = plain(cell);
  if (typeof value === 'object') return JSON.stringify(value);
  return value == null ? '' : String(value);
}
export function csv(result) {
  const field = value => {
    let text = String(value ?? '');
    // Spreadsheet formula injection applies to headers too, not just data.
    if (/^[\s]*[=+@-]/.test(text)) text = "'" + text;
    return '"' + text.replaceAll('"', '""') + '"';
  };
  return [result.columns.map(field).join(','), ...result.rows.map(row => row.map(cellText).map(field).join(','))].join('\r\n');
}
function identity(value) { return typeof value === 'string' && /^(0|[1-9]\d{0,38})$/.test(value) && BigInt(value) <= (1n << 128n) - 1n; }
export function extractGraph(result) {
  validateResult(result);
  const nodes = new Map(), edges = new Map(), unresolved = new Set();
  let omittedNodes = 0, omittedEdges = 0;
  const index = new Map(result.columns.map((name, i) => [name, i]));
  const addNode = (id, labels, properties) => {
    if (!identity(id)) throw new Error('Invalid vertex identity in result.');
    let node = nodes.get(id);
    if (!node) {
      if (nodes.size >= LIMITS.nodes) { omittedNodes++; return null; }
      node = { id, labels: [], properties: Object.create(null), title: `Vertex ${id}`, degree: 0 };
      nodes.set(id, node);
    }
    if (Array.isArray(labels)) node.labels = [...new Set(labels.filter(x => typeof x === 'string'))];
    if (properties && typeof properties === 'object' && !Array.isArray(properties)) node.properties = properties;
    node.title = String(node.properties.name ?? node.properties.title ?? node.properties.label ?? `Vertex ${id}`);
    node.group = node.labels[0] || 'Unlabeled';
    return node;
  };
  const addEdge = (id, source, target, directed, label = '', properties = null) => {
    if (!identity(id) || !identity(source) || !identity(target)) throw new Error('Invalid relationship identity in result.');
    addNode(source); addNode(target);
    const old = edges.get(id);
    if (old) {
      const samePair = (old.source === source && old.target === target) || ((!old.directed || !directed) && old.source === target && old.target === source);
      if (!samePair) throw new Error('A relationship has inconsistent endpoints in this result.');
      if (directed && !old.directed) Object.assign(old, { source, target, directed, label, properties: properties || Object.create(null) });
      unresolved.delete(id);
      return;
    }
    if (!nodes.has(source) || !nodes.has(target) || edges.size >= LIMITS.edges) { omittedEdges++; return; }
    edges.set(id, { id, source, target, directed, label, properties: properties || Object.create(null) });
    unresolved.delete(id);
  };
  const visit = cell => {
    switch (cell.type) {
      case 'vertex': addNode(cell.value); break;
      case 'edge': if (identity(cell.value)) unresolved.add(cell.value); else throw new Error('Invalid edge identity.'); break;
      case 'vertices': for (const id of cell.value) addNode(id); break;
      case 'edges': for (const id of cell.value) { if (!identity(id)) throw new Error('Invalid edge identity.'); unresolved.add(id); } break;
      case 'path': {
        const path = cell.value;
        if (!Array.isArray(path?.nodes) || !Array.isArray(path?.edges) || path.nodes.length !== path.edges.length + 1) throw new Error('Invalid path cell.');
        for (const id of path.nodes) addNode(id);
        // The path protocol records traversal order, NOT stored edge orientation.
        for (let i = 0; i < path.edges.length; i++) addEdge(path.edges[i], path.nodes[i], path.nodes[i + 1], false);
        break;
      }
      case 'list': for (const child of cell.value) visit(child); break;
      case 'map': for (const child of Object.values(cell.value)) visit(child); break;
    }
  };
  for (const row of result.rows) {
    for (const cell of row) visit(cell);
    const get = name => row[index.get(name)];
    for (const prefix of ['source', 'target', 'node']) {
      const cell = get(prefix);
      if (cell?.type === 'vertex') addNode(cell.value, plain(get(prefix + '_labels')), plain(get(prefix + '_properties')));
    }
    // Only the explicit Studio column contract establishes endpoints. Arbitrary
    // co-occurring vertex/edge cells are not evidence of an adjacency.
    if (get('source')?.type === 'vertex' && get('target')?.type === 'vertex' && get('relationship')?.type === 'edge') {
      addEdge(get('relationship').value, get('source').value, get('target').value, true,
        cellText(get('relationship_type')), plain(get('relationship_properties')));
    }
  }
  for (const edge of edges.values()) {
    nodes.get(edge.source).degree++;
    if (edge.source !== edge.target) nodes.get(edge.target).degree++;
    unresolved.delete(edge.id);
  }
  const warnings = [];
  if (omittedNodes || omittedEdges) warnings.push(`Canvas capped at ${LIMITS.nodes} nodes / ${LIMITS.edges} relationships. The table and JSON retain the complete received result.`);
  if (unresolved.size) warnings.push(`${unresolved.size} relationship IDs have no endpoints. Return source, relationship, target to draw directed connections.`);
  if ([...edges.values()].some(edge => !edge.directed)) warnings.push('Path connections show traversal adjacency; stored direction is not supplied.');
  return { nodes: [...nodes.values()], edges: [...edges.values()], warnings };
}
export function filterGraph(graph, hidden = new Set(), search = '') {
  const term = search.trim().toLowerCase();
  const nodes = graph.nodes.filter(node => !hidden.has(node.group));
  const allowed = new Set(nodes.map(node => node.id));
  return { nodes, edges: graph.edges.filter(edge => allowed.has(edge.source) && allowed.has(edge.target)), matches: new Set(nodes.filter(node => !term || `${node.title} ${node.id} ${node.labels.join(' ')}`.toLowerCase().includes(term)).map(node => node.id)), warnings: graph.warnings };
}
