'use strict';

// `node --test tools/validate/test/` resolves the directory to this file:
// load every *.test.js next to it.
const fs = require('node:fs');
const path = require('node:path');

for (const file of fs.readdirSync(__dirname).sort()) {
  if (file.endsWith('.test.js')) require(path.join(__dirname, file));
}
