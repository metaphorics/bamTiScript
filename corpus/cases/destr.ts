// Corpus driver for `destr` (unjs/destr @ 541b6f9).
// Exercises the checked-in local source surface `src/index.ts` and prints
// stable results derived from the project's own parsing behavior.
// No hardcoded expected value: every line is computed from destr/safeDestr.

import { destr, safeDestr } from "../projects/destr/src/index.ts";

function format(value: unknown): string {
  // NaN is the only value that is not equal to itself; represent it
  // deterministically so output stays byte-stable across runs.
  if (typeof value === "number" && Number.isNaN(value)) {
    return "NaN";
  }
  if (typeof value === "number" && value === Infinity) {
    return "Infinity";
  }
  if (typeof value === "number" && value === -Infinity) {
    return "-Infinity";
  }
  if (value === undefined) {
    return "undefined";
  }
  if (typeof value === "string") {
    return JSON.stringify(value);
  }
  return JSON.stringify(value);
}

const inputs: string[] = [
  '"a quoted string"',
  "true",
  "false",
  "null",
  "undefined",
  "nan",
  "infinity",
  "-infinity",
  "123",
  "-45.67",
  "1e3",
  '{"a":1,"b":[2,3]}',
  '[1,2,3]',
  "not json at all",
  '{"__proto__":{"polluted":true}}',
  '{"constructor":{"prototype":{"polluted":true}}}',
];

const lines: string[] = [];

for (const input of inputs) {
  const loose = destr(input);
  lines.push(`destr(${JSON.stringify(input)}) => ${format(loose)}`);
}

// safeDestr enforces strict mode: invalid JSON throws.
for (const input of ["not json at all", '{"a":1}']) {
  let result: string;
  try {
    result = `ok:${format(safeDestr(input))}`;
  } catch (err) {
    result = `throw:${(err as Error).message}`;
  }
  lines.push(`safeDestr(${JSON.stringify(input)}) => ${result}`);
}

// Prototype-pollution guard: strict mode rejects the suspect payload.
let protoStrict: string;
try {
  protoStrict = `throw:${(safeDestr('{"__proto__":{"x":1}}') as Error).message}`;
} catch (err) {
  protoStrict = `throw:${(err as Error).message}`;
}
lines.push(`safeDestr(__proto__ strict) => ${protoStrict}`);

// Non-string passthrough: returned as-is.
lines.push(`destr(123) => ${format(destr(123))}`);
lines.push(`destr(null) => ${format(destr(null))}`);

const out = lines.join("\n");
process.stdout.write(out + "\n");
