// Parse-check the browser modules before deploying.
//
// They only ever run in a page, so a typo surfaces as a blank terminal on the
// live site rather than as a build failure. This strips module syntax and hands
// the body to Function(), which parses without executing.
const fs = require('fs');

// Two traps, both of which this hit while being written, and both of which
// reported a syntax error in a file that was perfectly fine:
//
//   * Deleting a whole `export class X {` line takes the brace with it.
//     Remove only the keyword.
//   * Imports span lines. A line-based strip leaves the rest of a multi-line
//     import list behind, and the dangling `}` looks like the bug.
function strip(src) {
    return src
        .replace(/^[ \t]*import\s+[\s\S]*?\sfrom\s*['"][^'"]*['"]\s*;?/gm, '')
        .replace(/^[ \t]*import\s*['"][^'"]*['"]\s*;?/gm, '')
        .replace(/^(\s*)export\s+default\s+/gm, '$1')
        .replace(/^(\s*)export\s+(?=(class|function|const|let|var|async)\b)/gm, '$1')
        .replace(/^\s*export\s*\{[^}]*\}\s*;?\s*$/gm, '');
}

let bad = 0;
for (const f of process.argv.slice(2)) {
    try {
        new Function(strip(fs.readFileSync(f, 'utf8')));
        console.log(`${f}: parses`);
    } catch (e) {
        console.log(`${f}: SYNTAX ERROR ${e.message}`);
        bad++;
    }
}
process.exit(bad ? 1 : 0);
