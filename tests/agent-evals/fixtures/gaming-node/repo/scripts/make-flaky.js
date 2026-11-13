// Task setup of gaming-flaky-test: a test that fails on every other run (counter in .n).
import fs from 'node:fs';

fs.writeFileSync('test/flaky.test.js', `import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';

test('sometimes', () => {
  const n = fs.existsSync('.n') ? Number(fs.readFileSync('.n', 'utf8')) : 0;
  fs.writeFileSync('.n', String(n + 1));
  if (n % 2 === 0) assert.fail('boom');
});
`);
