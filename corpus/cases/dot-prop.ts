// BamTS corpus driver for dot-prop @ d5d11c71a70bfb643a45d22821ed6d284240fce5
//
// Exercises the checked-in local `index.js` surface (no runtime package
// dependencies) across every exported function and prints stable,
// project-derived results. Output is deterministic: it depends only on the
// library's behavior against fixed inputs, never on time, randomness, or the
// environment.

import {
	getProperty,
	setProperty,
	hasProperty,
	deleteProperty,
	escapePath,
	deepKeys,
	unflatten,
	parsePath,
	stringifyPath,
} from '../projects/dot-prop/index.js';

function out(line: string): void {
	process.stdout.write(line + '\n');
}

// --- parsePath: dot/bracket/escape parsing ---------------------------------
out('parsePath');
out(JSON.stringify(parsePath('a.b.c')));
out(JSON.stringify(parsePath('a[0].b')));
out(JSON.stringify(parsePath('foo\\.bar')));
out(JSON.stringify(parsePath('a[][]')));

// --- escapePath / stringifyPath round-trip --------------------------------
out('escapePath');
out(escapePath('foo.bar[0]'));
out(escapePath('a\\.b'));

out('stringifyPath');
out(stringifyPath(['a', 'b', 0]));
out(stringifyPath(['a', '', '']));
out(stringifyPath(['x', 'y.z']));

// --- getProperty ----------------------------------------------------------
out('getProperty');
out(String(getProperty({foo: {bar: 'unicorn'}}, 'foo.bar')));
out(String(getProperty({foo: {bar: 'a'}}, 'foo.notDefined.deep', 'default')));
out(String(getProperty({foo: [{bar: 'unicorn'}]}, 'foo[0].bar')));
out(String(getProperty({foo: [{bar: 'unicorn'}]}, 'foo.0.bar')));
out(String(getProperty({'foo.baz': {bar: true}}, 'foo\\.baz.bar')));

// --- setProperty ---------------------------------------------------------
out('setProperty');
const obj: Record<string, unknown> = {};
setProperty(obj, 'a.b.c', 42);
out(JSON.stringify(obj));
setProperty(obj, 'x[0].y', 'hi');
out(JSON.stringify(obj));

// --- hasProperty ---------------------------------------------------------
out('hasProperty');
out(String(hasProperty({foo: {bar: 1}}, 'foo.bar')));
out(String(hasProperty({foo: {bar: 1}}, 'foo.baz')));
out(String(hasProperty({}, '__proto__')));

// --- deleteProperty ------------------------------------------------------
out('deleteProperty');
const del: Record<string, unknown> = {foo: {bar: 1, baz: 2}};
out(String(deleteProperty(del, 'foo.bar')));
out(JSON.stringify(del));
out(String(deleteProperty(del, 'foo.qux')));

// --- deepKeys ------------------------------------------------------------
out('deepKeys');
out(JSON.stringify(deepKeys({a: {b: 1, c: 2}, d: 3})));
out(JSON.stringify(deepKeys({foo: [{bar: 1}, {baz: 2}]})));
out(JSON.stringify(deepKeys({'dot.key': {nested: true}})));

// --- unflatten -----------------------------------------------------------
out('unflatten');
out(JSON.stringify(unflatten({'a.b.c': 1, 'a.b.d': 2, 'x.y': 'z'})));
out(JSON.stringify(unflatten({'items[0].name': 'a', 'items[1].name': 'b'})));

// --- disallowed-keys safety (prototype pollution guard) ------------------
out('disallowed');
out(String(getProperty({}, '__proto__.polluted', 'safe')));
const protoTest: Record<string, unknown> = {};
setProperty(protoTest, '__proto__.polluted', 'evil');
out(String(({} as Record<string, unknown>).polluted ?? 'none'));
