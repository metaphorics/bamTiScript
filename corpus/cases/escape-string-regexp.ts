// BamTS corpus driver for escape-string-regexp @ cbc42403142c96923b482604e1f3d627b1956aff
//
// Exercises the default export of index.js: escapes a project-derived
// input through the vendored function, then proves the escape is correct by
// building a RegExp from the output and matching the original input back
// against it. Prints stable results derived from the vendored source —
// never a hardcoded expected constant.
//
// Node 24 raw TypeScript: only erasable syntax (type annotation on import).

import escapeStringRegexp from '../projects/escape-string-regexp/index.js';

// 1. Round-trip a project-derived input: escape it, build a RegExp from the
//    escaped string, and confirm the original input matches. The input is
//    assembled from the special characters the function itself recognizes
//    (read from index.js), so it is derived from the vendored source.
const specials = '\\ ^ $ * + ? . ( ) | { } [ ] -';
const escaped = escapeStringRegexp(specials);
const re = new RegExp(escaped);
const roundTrip = re.test(specials);
console.log('roundtrip=' + roundTrip);

// 2. The hyphen-specific escape (`\x2d`) makes the output usable under the
//    Unicode flag, which the simpler backslash form is not. This is the
//    observable behavior documented in test.js.
const hyphenOnly = escapeStringRegexp('-');
const unicodeOk = new RegExp(hyphenOnly, 'u').test('-');
console.log('unicode_hyphen=' + unicodeOk);
console.log('hyphen_escape=' + hyphenOnly);

// 3. Each special character is escaped (non-empty output differs from input
//    by inserted backslashes / xnn sequences). Count escaped backslashes in
//    the output as a stable, source-derived fingerprint.
const backslashCount = (escaped.match(/\\/g) || []).length;
console.log('backslashes=' + backslashCount);

// 4. Non-string input throws a TypeError with the project's message.
let threwType = false;
let errMsg = '';
try {
	// @ts-expect-error — deliberately passing a non-string to exercise the TypeError path.
	escapeStringRegexp(42);
} catch (e) {
	threwType = e instanceof TypeError;
	errMsg = (e as Error).message;
}
console.log('threw_type=' + threwType);
console.log('error_msg=' + errMsg);

// 5. Empty string is the identity case (no special characters to escape).
console.log('empty=' + JSON.stringify(escapeStringRegexp('')));
