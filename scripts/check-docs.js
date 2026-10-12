// Generic markdown table validator for the pie docs.
//
// Docs are part of the contract here (`ACCEPTANCE.md` carries the frozen gates),
// so a malformed table is a silent contract defect: it still renders, just
// wrong. Two tables in this repo were mangled by hand edits before this check
// existed, so it validates shape rather than specific labels.
//
// Rule: inside a table block, the separator row (`|---|---|`) fixes the column
// count, and every other row in the block must match it exactly. A pipe escaped
// as `\|` is cell content, not a separator.
//
// Run: node .scratch/check-table.js [files...]
const fs = require('fs');
const path = require('path');

/** Count `|` characters that are not escaped by a preceding backslash. */
function separators(row) {
  let count = 0;
  for (let i = 0; i < row.length; i += 1) {
    if (row[i] === '|' && row[i - 1] !== '\\') count += 1;
  }
  return count;
}

const isSeparator = (row) => /^\|[\s:|-]+\|$/.test(row);
const cells = (row) => separators(row) - 1;

function check(file) {
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  const problems = [];
  let blocks = 0;
  let rows = 0;

  let expected = null;
  let start = 0;

  const closeBlock = () => {
    expected = null;
  };

  lines.forEach((row, index) => {
    if (!row.startsWith('|')) {
      closeBlock();
      return;
    }

    if (expected === null) {
      // A new block begins. Only count it once its separator row is seen.
      start = index;
      if (isSeparator(row)) {
        expected = cells(row);
        blocks += 1;
      }
      return;
    }

    rows += 1;
    const got = cells(row);
    if (got !== expected) {
      problems.push(
        `${file}:${index + 1}: ${got} cells, table starting line ${start + 1} expects ${expected}`,
      );
    }
  });

  return { problems, blocks, rows };
}

// Default: every markdown doc that carries tables of consequence.
const defaults = [
  'README.md',
  'tests/README.md',
  ...fs.readdirSync('planning').map((f) => path.join('planning', f)),
  ...fs.readdirSync('knowledge').map((f) => path.join('knowledge', f)),
];

const files = (process.argv.slice(2).length ? process.argv.slice(2) : defaults).filter(
  (f) => f.endsWith('.md') && fs.existsSync(f),
);

let totalBlocks = 0;
let totalRows = 0;
const allProblems = [];

for (const file of files) {
  const { problems, blocks, rows } = check(file);
  totalBlocks += blocks;
  totalRows += rows;
  allProblems.push(...problems);
}

console.log(`checked ${files.length} files, ${totalBlocks} table blocks, ${totalRows} rows`);

if (allProblems.length) {
  console.log(`\nMALFORMED (${allProblems.length}):`);
  for (const p of allProblems) console.log('  ' + p);
  process.exit(1);
}

console.log('all tables well-formed');
