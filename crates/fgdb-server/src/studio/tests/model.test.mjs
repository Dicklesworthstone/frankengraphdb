import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { LIMITS, graphQuery, queryBody, quoteIdentifier, parseResponse, boundedText, validateResult, plain, cellText, csv, extractGraph, filterGraph } from '../model.mjs';
import { createLayout, createSimulation } from '../layout.mjs';
import { demoResult } from '../demo.mjs';
const cell = (type, value) => ({ type, value });
const vertex = value => cell('vertex', value);
const edge = value => cell('edge', value);
const result = (columns, rows) => ({ v: 1, columns, rows, seq: 1 });
const forceModule = new WebAssembly.Module(readFileSync(new URL('../wasm/force.wasm', import.meta.url)));
const simulation = (nodes, edges, mode) => createSimulation(new WebAssembly.Instance(forceModule, {}).exports, nodes, edges, mode);

test('sample counts come from the actual typed fixture', () => {
  const graph = extractGraph(demoResult());
  assert.equal(graph.nodes.length, 32); assert.equal(graph.edges.length, 59);
  assert.equal(new Set(graph.nodes.map(node => node.group)).size, 4);
  assert.equal(graph.warnings.length, 0);
  assert.equal(graph.nodes.reduce((sum, node) => sum + node.degree, 0), 118);
});
test('full-width identities never pass through Number', () => {
  const max = ((1n << 128n) - 1n).toString(), neighbor = ((1n << 128n) - 2n).toString();
  const graph = extractGraph(result(['source', 'relationship', 'target'], [[vertex(max), edge('5'), vertex(neighbor)]]));
  assert.deepEqual(graph.nodes.map(node => node.id), [max, neighbor]);
  assert.equal(graph.edges[0].source, max); assert.equal(graph.edges[0].target, neighbor);
  for (const id of ['-1', '01', '', (1n << 128n).toString(), 42]) assert.throws(() => extractGraph(result(['n'], [[vertex(id)]])), /identity/);
});
test('parallel edges and self-loops preserve distinct identities', () => {
  const graph = extractGraph(result(['source', 'relationship', 'target'], [[vertex('1'), edge('1'), vertex('2')], [vertex('1'), edge('2'), vertex('2')], [vertex('1'), edge('3'), vertex('1')]]));
  assert.equal(graph.nodes.length, 2); assert.equal(graph.edges.length, 3);
  assert.deepEqual(graph.nodes.map(node => node.degree), [3, 2]);
});
test('arbitrary co-occurrence never fabricates graph adjacency', () => {
  const graph = extractGraph(result(['a', 'r', 'b'], [[vertex('1'), edge('3'), vertex('2')]]));
  assert.equal(graph.nodes.length, 2); assert.equal(graph.edges.length, 0);
  assert.match(graph.warnings.join(' '), /no endpoints/);
});
test('paths preserve repeated reverse traversal without inventing stored direction', () => {
  const graph = extractGraph(result(['path'], [[cell('path', { nodes: ['1', '2', '1'], edges: ['7', '7'] })]]));
  assert.equal(graph.nodes.length, 2); assert.equal(graph.edges.length, 1);
  assert.equal(graph.edges[0].directed, false);
  assert.match(graph.warnings.join(' '), /stored direction/);
});
test('a reverse path may coexist with explicitly directed endpoints', () => {
  const graph = extractGraph(result(['source', 'relationship', 'target', 'path'], [[vertex('1'), edge('7'), vertex('2'), cell('path', { nodes: ['2', '1'], edges: ['7'] })], [vertex('1'), edge('7'), vertex('2'), cell('path', { nodes: ['2', '1'], edges: ['7'] })]]));
  assert.equal(graph.edges.length, 1); assert.equal(graph.edges[0].source, '1'); assert.equal(graph.edges[0].target, '2'); assert.equal(graph.edges[0].directed, true);
});
test('inconsistent endpoints refuse instead of silently merging unrelated edges', () => {
  assert.throws(() => extractGraph(result(['source', 'relationship', 'target'], [[vertex('1'), edge('7'), vertex('2')], [vertex('3'), edge('7'), vertex('2')]])), /inconsistent/);
});
test('nested graph cells are found; map keys cannot pollute object prototypes', () => {
  const malicious = JSON.parse('{"type":"map","value":{"__proto__":{"type":"text","value":"polluted"},"node":{"type":"vertex","value":"9"}}}');
  assert.equal(Object.getPrototypeOf(plain(malicious)), null); assert.equal(plain(malicious).__proto__, 'polluted');
  assert.equal({}.polluted, undefined);
  const graph = extractGraph(result(['nested'], [[cell('list', [malicious])]])); assert.deepEqual(graph.nodes.map(node => node.id), ['9']);
});
test('metadata follows the explicit alias contract and retains numeric strings', () => {
  const graph = extractGraph(result(['node', 'node_labels', 'node_properties'], [[vertex('9'), cell('list', [cell('text', 'Person')]), cell('map', { name: cell('text', '<script>not markup</script>'), balance: cell('int', '9007199254740993') })]]));
  assert.equal(graph.nodes[0].title, '<script>not markup</script>'); assert.equal(graph.nodes[0].group, 'Person'); assert.equal(graph.nodes[0].properties.balance, '9007199254740993');
});
test('canvas bounds do not truncate the received table or silently hide clipping', () => {
  const source = result(['node'], Array.from({ length: LIMITS.nodes + 10 }, (_, i) => [vertex(String(i))]));
  const graph = extractGraph(source); assert.equal(graph.nodes.length, LIMITS.nodes);
  assert.equal(source.rows.length, LIMITS.nodes + 10); assert.match(graph.warnings[0], /capped/);
});
test('label filters affect the view, never the graph source', () => {
  const graph = extractGraph(demoResult()), before = JSON.stringify(graph);
  const view = filterGraph(graph, new Set(['Person']), 'knowledge');
  assert.equal(view.nodes.length, 24); assert.equal(view.matches.size, 2);
  const ids = new Set(view.nodes.map(node => node.id)); assert.ok(view.edges.every(edge => ids.has(edge.source) && ids.has(edge.target)));
  assert.equal(JSON.stringify(graph), before);
});
test('query templates quote identifiers and admit only a bounded integer sample size', () => {
  assert.equal(quoteIdentifier('a`b'), '`a``b`');
  assert.match(graphQuery({ label: 'x`) RETURN 1 //', limit: 50 }), /`x``\) RETURN 1 \/\/`/);
  for (const limit of [0, -1, 501, 1.5, '50', NaN, Infinity]) assert.throws(() => graphQuery({ limit }));
  assert.match(graphQuery({ kind: 'nodes' }), /node_properties/);
});
test('parameters are validated but sent byte-for-byte without integer or decimal rounding', () => {
  const raw = '{"wide":9007199254740993,"amount":{"$decimal":"0.000000000000000001"}}';
  assert.ok(queryBody('RETURN $wide', raw).endsWith(`"parameters":${raw}}`));
  assert.deepEqual(JSON.parse(queryBody('RETURN "quoted"', '{}')).statement, 'RETURN "quoted"');
  for (const bad of ['[]', 'null', 'true', '{']) assert.throws(() => queryBody('RETURN 1', bad));
  assert.throws(() => queryBody(' ', '{}')); assert.throws(() => queryBody('x'.repeat(65537), '{}'));
});
test('unsafe metadata integers are retained from the parser source, not rounded', () => {
  assert.equal(parseResponse('{"seq":18446744073709551615,"v":1}').seq, '18446744073709551615');
  assert.equal(parseResponse('{"value":"9007199254740993"}').value, '9007199254740993');
  assert.throws(() => parseResponse('{"bad":1e999}'), /exactly/);
});
test('cell text does not coerce exact averages, decimals, binary data or null', () => {
  assert.equal(cellText(cell('average', '9007199254740993/7')), '9007199254740993/7');
  assert.equal(cellText(cell('decimal', '1.000000000000000001')), '1.000000000000000001');
  assert.equal(cellText(cell('null')), 'null'); assert.equal(cellText(cell('bytes', '00ff')), '00ff');
});
test('CSV quotes data and prevents spreadsheet formulas in headers and values', () => {
  const output = csv(result(['=HYPERLINK("evil")', 'name'], [[cell('text', '\t=1+1'), cell('text', 'comma,"quote"\nline')]]));
  assert.ok(output.startsWith('"\'=HYPERLINK(""evil"")"')); assert.ok(output.includes('"\'\t=1+1"')); assert.ok(output.includes('comma,""quote""\nline'));
});
test('result validation refuses oversized rows, invalid widths and nested cells', () => {
  assert.throws(() => validateResult(result(['a'], [[cell('int', '1'), cell('int', '2')]])), /width/);
  assert.throws(() => validateResult(result([], Array.from({ length: LIMITS.rows + 1 }, () => []))), /10,000/);
  let nested = cell('text', 'x'); for (let i = 0; i < 34; i++) nested = cell('list', [nested]);
  assert.throws(() => validateResult(result(['a'], [[nested]])), /bounds/);
  assert.throws(() => validateResult(result(['a'], [[cell('map', null)]])), /map/);
});
test('response streaming admits exact byte limits and cancels an oversized body', async () => {
  assert.equal(await boundedText(new Response('abc'), 3), 'abc');
  let cancelled = false;
  const stream = new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array([1, 2, 3, 4])); }, cancel() { cancelled = true; } });
  await assert.rejects(boundedText(new Response(stream), 3), /8 MiB/); assert.equal(cancelled, true);
  let declaredCancelled = false;
  const declared = new ReadableStream({ cancel() { declaredCancelled = true; } });
  await assert.rejects(boundedText(new Response(declared, { headers: { 'content-length': '4' } }), 3)); assert.equal(declaredCancelled, true);
});
test('UTF-8 streaming preserves split multibyte characters and rejects invalid encoding', async () => {
  const bytes = new TextEncoder().encode('🌱 graph');
  const stream = new ReadableStream({ start(c) { c.enqueue(bytes.slice(0, 2)); c.enqueue(bytes.slice(2)); c.close(); } });
  assert.equal(await boundedText(new Response(stream)), '🌱 graph');
  await assert.rejects(boundedText(new Response(new Uint8Array([0xff]))));
});
test('layout is deterministic, finite and reaches a bounded settled state in every mode', () => {
  const graph = extractGraph(demoResult());
  for (const mode of ['force', 'clusters', 'radial']) {
    const a = simulation(graph.nodes, graph.edges, mode), b = simulation(graph.nodes, graph.edges, mode);
    for (let i = 0; i < 100; i++) { a.step(8); b.step(8); }
    assert.equal(a.stats().ticks, 800); assert.deepEqual(a.positions(), b.positions());
    assert.ok([...a.positions()].every(Number.isFinite));
    const settled = a.positions(); a.step(8); assert.deepEqual(a.positions(), settled);
  }
});
test('layout handles empty, disconnected and self-loop graphs within input bounds', () => {
  const empty = simulation([], []); empty.step(8); assert.equal(empty.positions().length, 0);
  const nodes = Array.from({ length: 80 }, (_, i) => ({ id: String(i), group: 'g' + i % 5 }));
  const state = simulation(nodes, nodes.map(n => ({ source: n.id, target: n.id }))); for (let i = 0; i < 100; i++) state.step(8);
  assert.ok([...state.positions()].every(Number.isFinite));
  assert.throws(() => createLayout(Array.from({ length: 751 }, () => ({})), []), /bounds/);
});
test('browser shell links only local assets and the application has no markup-evaluation sink', () => {
  const html = readFileSync(new URL('../index.html', import.meta.url), 'utf8');
  const app = readFileSync(new URL('../app.mjs', import.meta.url), 'utf8');
  assert.ok(!/https?:\/\//.test(html)); assert.ok(!/\b(?:innerHTML|outerHTML|eval)\b/.test(app));
  assert.ok(!app.includes('/write')); assert.ok(app.includes("credentials: 'omit'")); assert.ok(app.includes("redirect: 'error'"));
  const storageWrites = app.match(/localStorage\.setItem\([^\n]+/g) || [];
  assert.equal(storageWrites.length, 1); assert.match(storageWrites[0], /fgdb-studio-theme/);
  assert.ok(!app.includes('sessionStorage')); assert.ok(!app.includes('indexedDB'));
});
