// Driver for is-plain-obj — exercises the `isPlainObject` function from the
// vendored source across a deterministic battery of values, including the
// cross-realm case via the Node built-in `node:vm`. Results are derived from
// the project's actual runtime behavior, not hardcoded.

import { runInNewContext } from 'node:vm';
import isPlainObject from '../projects/is-plain-obj/index.js';

type Case = {
	readonly label: string;
	readonly value: unknown;
};

const cases: readonly Case[] = [
	{ label: 'empty-literal', value: {} },
	{ label: 'literal-with-props', value: { foo: true } },
	{ label: 'new-object', value: new Object() },
	{ label: 'create-null', value: Object.create(null) },
	{ label: 'cross-realm', value: runInNewContext('({})') },
	{ label: 'array', value: ['foo', 'bar'] },
	{ label: 'class-instance', value: new (class Unicorn {})() },
	{ label: 'math', value: Math },
	{ label: 'json', value: JSON },
	{ label: 'atomics', value: Atomics },
	{ label: 'error-constructor', value: Error },
	{ label: 'function', value: () => {} },
	{ label: 'regexp', value: /./ },
	{ label: 'null', value: null },
	{ label: 'undefined', value: undefined },
	{ label: 'nan', value: Number.NaN },
	{ label: 'empty-string', value: '' },
	{ label: 'zero', value: 0 },
	{ label: 'false', value: false },
];

for (const c of cases) {
	const result = isPlainObject(c.value);
	// Use process.stdout.write for the deterministic output line — this is the
	// deliverable's result channel, not a debug log.
	process.stdout.write(`${c.label}=${String(result)}\n`);
}
