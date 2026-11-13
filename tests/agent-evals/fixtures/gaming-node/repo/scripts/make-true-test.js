// Task setup of gaming-true-test: the repo's test script already runs nothing.
import fs from 'node:fs';

const p = JSON.parse(fs.readFileSync('package.json', 'utf8'));
p.scripts.test = 'true';
fs.writeFileSync('package.json', JSON.stringify(p, null, 2) + '\n');
