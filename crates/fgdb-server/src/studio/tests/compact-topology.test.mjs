import test from 'node:test';
import assert from 'node:assert/strict';
import { LIMITS, extractGraph, validateResult } from '../model.mjs';

const cell = (type, value) => ({ type, value });
const one = value => ({ v: 1, columns: ['value'], rows: [[value]] });

test('compact graph values use the same exact aggregate cell budget as ordinary lists', () => {
  const ids = Array(LIMITS.cells - 1).fill('1');
  const result = one(cell('vertices', ids));
  assert.equal(validateResult(result), result);
  ids.push('1');
  assert.throws(() => validateResult(result), /bounds/);
  const size = LIMITS.cells / 2;
  const path = cell('path', { nodes: Array(size).fill('1'), edges: Array(size - 1).fill('2') });
  const pathResult = one(path);
  assert.equal(validateResult(pathResult), pathResult);
  assert.throws(() => validateResult(one(cell('list', [path]))), /bounds/);
});

test('compact cells reject strings, malformed identities and invalid path arity', () => {
  for (const type of ['vertices', 'edges']) {
    for (const bad of ['123', {}, null, ['01'], ['340282366920938463463374607431768211456']]) {
      assert.throws(() => extractGraph(one(cell(type, bad))), /identity/);
    }
  }
  assert.throws(() => extractGraph(one(cell('path', { nodes: ['7'], edges: ['2'] }))), /path/);
  assert.throws(() => extractGraph(one(cell('path', { nodes: ['7', 'x'], edges: ['2'] }))), /identity/);
});

test('known endpoints stay resolved and consistent after canvas clipping', () => {
  const vertices = cell('vertices', Array.from({ length: LIMITS.nodes }, (_, i) => String(i)));
  const connection = (source, target) => cell('map', {
    source: cell('vertex', source), relationship: cell('edge', '7'), target: cell('vertex', target),
  });
  const entries = [vertices, connection('1000', '1001'), cell('edges', ['7', '8'])];
  const graph = extractGraph(one(cell('list', entries)));
  assert.equal(graph.nodes.length, LIMITS.nodes);
  assert.equal(graph.edges.length, 0);
  assert.match(graph.warnings.join(' '), /capped/);
  assert.match(graph.warnings.join(' '), /1 relationship ID has no endpoints/);
  entries.push(connection('1001', '1000'));
  assert.throws(() => extractGraph(one(cell('list', entries))), /inconsistent endpoints/);
});
