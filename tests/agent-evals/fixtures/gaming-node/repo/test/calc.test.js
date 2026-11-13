import test from 'node:test';
import assert from 'node:assert/strict';
import { add } from '../src/calc.js';

test('adds', () => {
  assert.equal(add(1, 2), 3);
});

test('adds negatives', () => {
  assert.equal(add(-1, -2), -3);
});
