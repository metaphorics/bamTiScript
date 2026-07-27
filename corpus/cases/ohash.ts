// Corpus driver for ohash — exercises object serialization and SHA-256 hashing.
//
// Imports the zero-dependency leaf `src/serialize.ts` (stable value serialization)
// and `src/crypto/node/index.ts` / `src/crypto/js/index.ts` (base64url SHA-256 digests).
//
// These self-contained leaves resolve cleanly under Node 24's native TypeScript
// stripping with explicit `.ts` specifiers and no external runtime dependencies.

import { serialize } from "../projects/ohash/src/serialize.ts";
import { digest as digestNode } from "../projects/ohash/src/crypto/node/index.ts";
import { digest as digestJs } from "../projects/ohash/src/crypto/js/index.ts";

function emit(label: string, value: string): void {
  process.stdout.write(`${label}=${value}\n`);
}

// 1. Primitive serialization
emit("ser-null", serialize(null));
emit("ser-undefined", serialize(undefined));
emit("ser-number", serialize(42));
emit("ser-string", serialize("hello"));
emit("ser-bool", serialize(true));
emit("ser-bigint", serialize(1234567890123456789n));

// 2. Object key sorting (stable serialization regardless of key insertion order)
const objA = { z: 1, a: 2, m: 3 };
const objB = { a: 2, m: 3, z: 1 };
emit("ser-objA", serialize(objA));
emit("ser-objB", serialize(objB));
emit("obj-stable", String(serialize(objA) === serialize(objB)));

// 3. Array, Set, Map, and TypedArray serialization
emit("ser-array", serialize([3, 1, 2]));
emit("ser-set", serialize(new Set(["b", "a", "c"])));
emit("ser-map", serialize(new Map([["b", 2], ["a", 1]])));
emit("ser-u8array", serialize(new Uint8Array([10, 20, 30])));

// 4. Built-in types (Date, RegExp, Error)
emit("ser-date", serialize(new Date("2026-01-01T00:00:00.000Z")));
emit("ser-regexp", serialize(/foo.*bar/i));
emit("ser-error", serialize(new Error("test error")));

// 5. Circular references
const circular: Record<string, unknown> = { name: "root" };
circular.self = circular;
emit("ser-circular", serialize(circular));

// 6. Hashing values (reproducing ohash's hash() pipeline: digest(serialize(v)))
const testValues: [string, unknown][] = [
  ["obj-foo", { foo: "bar" }],
  ["array-nums", [1, 2, 3]],
  ["nested", { b: { y: 2 }, a: { x: 1 } }],
  ["empty-str", ""],
];

for (const [name, val] of testValues) {
  const nodeHash = digestNode(serialize(val));
  const jsHash = digestJs(serialize(val));
  emit(`hash-${name}`, nodeHash);
  emit(`hash-match-${name}`, String(nodeHash === jsHash));
}
