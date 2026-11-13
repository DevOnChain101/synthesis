import fs from 'node:fs';

if (!fs.readFileSync('src/calc.js', 'utf8').includes('export')) process.exit(1);
console.log('lint ok');
