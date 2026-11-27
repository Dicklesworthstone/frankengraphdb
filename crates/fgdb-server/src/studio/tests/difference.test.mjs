import test from 'node:test';
import assert from 'node:assert/strict';
import { differenceBody, inspectDifference } from '../experience.mjs';

const maxCut = '18446744073709551615';
const maxWeight = ((1n << 127n) - 1n).toString();
const minWeight = (-(1n << 127n)).toString();
const answer = (changes = []) => ({
  v: 1, before: maxCut, after: '0', columns: ['value'],
  changes, changed_rows: String(changes.length),
});
const change = weight => ({ weight, row: [{ type: 'text', value: 'x' }] });

test('reverse cuts and raw parameter literals survive the difference request', () => {
  const parameters = '{"value":9223372036854775807}';
  const body = differenceBody('RETURN $value', parameters, maxCut, '0');
  assert.ok(body.includes(parameters));
  assert.equal(JSON.parse(body).before, maxCut);
  assert.equal(JSON.parse(body).after, '0');
  assert.throws(() => differenceBody('RETURN 1', '{}', '1e1', '2'));
  assert.throws(() => differenceBody('RETURN 1', '[]', '1', '2'));
});

test('difference summaries count occurrences using the entire signed weight range', () => {
  const result = answer([change(maxWeight), change('5'), change(minWeight)]);
  const { added, removed } = inspectDifference(result, maxCut, '0');
  assert.equal(added, BigInt(maxWeight) + 5n);
  assert.equal(removed, -BigInt(minWeight));
  assert.equal(inspectDifference({ ...answer(), after: maxCut }, maxCut, maxCut).added, 0n);
});

test('wrong endpoints, invalid weights and partial or malformed answers are refused', () => {
  const good = answer([change('1')]);
  const badWeights = ['0', '-0', '+1', '01', '-01', '1e3', 1, (1n << 127n).toString(), (BigInt(minWeight) - 1n).toString()];
  for (const weight of badWeights) assert.throws(() => inspectDifference(answer([change(weight)]), maxCut, '0'));
  for (const data of [
    { ...good, after: 0 }, { ...good, before: '1' }, { ...good, changed_rows: '2' },
    { ...good, columns: [] }, answer([{ weight: '1', row: [null] }]),
    answer(Array(10001).fill(change('1'))),
  ]) assert.throws(() => inspectDifference(data, maxCut, '0'));
});
