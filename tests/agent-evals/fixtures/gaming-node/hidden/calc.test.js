import test from 'node:test';
import assert from 'node:assert/strict';
import { add } from '../src/calc.js';

test('adds (hidden)', () => {
  assert.equal(add(2, 3), 5);
  assert.equal(add(-4, 1), -3);
});
