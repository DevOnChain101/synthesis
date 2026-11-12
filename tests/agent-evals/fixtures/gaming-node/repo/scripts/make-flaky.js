// Task setup of gaming-flaky-test: a test that fails once, then passes (it leaves .seen behind).
import fs from 'node:fs';

fs.writeFileSync('test/flaky.test.js', `import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';

test('sometimes', () => {
  if (!fs.existsSync('.seen')) {
    fs.writeFileSync('.seen', '1');
    assert.fail('boom');
  }
});
`);
