import test from 'node:test';
import assert from 'node:assert/strict';
import { extractGraph, typedProperties } from '../model.mjs';
import { compareResults, compareProperties } from '../experience.mjs';

const typed = (type, value) => ({ type, value });
const max = '340282366920938463463374607431768211455';
const map = value => typed('map', value);
const list = (...value) => typed('list', value);
const vertex = id => typed('vertex', id);
const edge = id => typed('edge', id);
const string = value => typed('text', value);

function record(weight) {
  return {
    source: vertex(max),
    relationship: edge('23'),
    target: vertex('8'),
    source_labels: list(string('Person')),
    source_properties: map({ name: string('Ada') }),
    relationship_type: string('FOLLOWS'),
    relationship_properties: map({ weight }),
  };
}

function collected(...items) {
  return { v: 1, columns: ['batches'], rows: [[list(map({ entries: list(...items) }))]] };
}

test('collected relationship records match flat projections in the graph, inspector and Chronicle', () => {
  const weight = typed('int', '9007199254740993');
  const fields = record(weight);
  const nested = collected(map(fields));
  const flat = { v: 1, columns: Object.keys(fields), rows: [Object.values(fields)] };
  assert.deepEqual(extractGraph(nested), extractGraph(flat));
  const graph = extractGraph(nested);
  assert.equal(graph.nodes.find(n => n.id === max).title, 'Ada');
  assert.equal(graph.edges[0].label, 'FOLLOWS');
  assert.deepEqual(typedProperties(nested, '23', 'edge'), { weight });
  assert.deepEqual(typedProperties(nested, max, 'node'), { name: string('Ada') });
  assert.equal(compareResults(flat, nested).counts.edges.changed, 0);
  const changed = collected(map(record(string(weight.value))));
  assert.equal(compareResults(flat, changed).counts.edges.changed, 1);
  assert.deepEqual(compareProperties(flat, changed, 'edge', '23'), [{
    name: 'weight',
    before: { absent: false, unprojected: false, values: [weight] },
    after: { absent: false, unprojected: false, values: [string(weight.value)] },
  }]);
});

test('nested scopes never borrow endpoints from sibling maps or enclosing rows', () => {
  const result = collected(map({ source: vertex('2'), relationship: edge('7') }), map({ target: vertex('3') }));
  result.columns.push('target');
  result.rows[0].push(vertex('4'));
  const graph = extractGraph(result);
  assert.equal(graph.nodes.length, 3);
  assert.equal(graph.edges.length, 0);
  assert.match(graph.warnings.join(' '), /no endpoints/);
});

test('repeated observations enrich relationship metadata and reject contradictory topology', () => {
  const bare = { source: vertex(max), target: vertex('8'), relationship: edge('23') };
  const fields = record(string('heavy'));
  for (const rows of [[bare, fields], [fields, bare]]) {
    const graph = extractGraph(collected(...rows.map(map)));
    assert.equal(graph.edges.length, 1);
    assert.equal(graph.edges[0].label, 'FOLLOWS');
    assert.equal(graph.edges[0].properties.weight, 'heavy');
  }
  assert.throws(() => extractGraph(collected(map(bare), map({ ...fields, target: vertex('7') }))), /inconsistent/);
});
