// BamTS corpus driver for p-map @ bc26cf03f81292325236a1188063dac8e7a4de0f
// Exercises the real runtime surface of index.js:
//   - pMap (default export): concurrent mapping with ordered results
//   - pMapSkip: sentinel to exclude items from the result array
//   - pMapIterable (named export): async-iterator variant with backpressure
// All output is derived from the project's own computation; no hardcoded expected values.
import pMap, { pMapSkip, pMapIterable } from "../projects/p-map/index.js";

const out: string[] = [];

// --- pMap: concurrent mapping preserves input order regardless of concurrency ---
// The mapper resolves after a deterministic number of microtask ticks so that
// higher-concurrency items finish before lower-concurrency ones; pMap must still
// return results in input order.
const tick = (n: number): Promise<void> =>
  n <= 0 ? Promise.resolve() : Promise.resolve().then(() => tick(n - 1));

const input = [5, 1, 3, 2, 4];
const mapped = await pMap(input, async (value: number, index: number) => {
  // items with higher value resolve first (fewer ticks), testing order preservation
  await tick(input.length - value);
  return value * 10 + index;
}, { concurrency: 3 });

out.push(`pMap.order=${JSON.stringify(mapped)}`);

// --- pMap: concurrency=1 runs sequentially, preserving order ---
const seq = await pMap([10, 20, 30], async (v: number) => {
  await tick(0);
  return v + 1;
}, { concurrency: 1 });
out.push(`pMap.seq=${JSON.stringify(seq)}`);

// --- pMapSkip: items returning the sentinel are excluded from the result ---
const filtered = await pMap([1, 2, 3, 4, 5, 6], async (v: number) => {
  await tick(0);
  return v % 2 === 0 ? pMapSkip : v;
}, { concurrency: 2 });
out.push(`pMap.skip=${JSON.stringify(filtered)}`);

// --- pMap: stopOnError=false collects all errors into an AggregateError ---
let aggError: unknown = null;
let aggMessage = "";
try {
  await pMap([1, 2, 3], async (v: number) => {
    await tick(0);
    throw new Error(`err-${v}`);
  }, { stopOnError: false });
} catch (e: unknown) {
  aggError = e;
  aggMessage = e instanceof AggregateError ? `Aggregate(${(e as AggregateError).errors.length})` : "non-aggregate";
}
out.push(`pMap.stopOnErrorFalse=${aggError !== null},${aggMessage}`);

// --- pMap: stopOnError=true rejects with the first error ---
let firstErr = "";
try {
  await pMap([1, 2], async (v: number) => {
    await tick(0);
    if (v === 1) throw new Error(`first-${v}`);
    return v;
  }, { stopOnError: true, concurrency: 1 });
} catch (e: unknown) {
  firstErr = e instanceof Error ? e.message : String(e);
}
out.push(`pMap.stopOnErrorTrue=${firstErr}`);

// --- pMap: invalid concurrency throws TypeError synchronously ---
let concurrencyErr = "";
try {
  await pMap([], async (v: number) => v, { concurrency: 0 });
} catch (e: unknown) {
  concurrencyErr = e instanceof TypeError ? "TypeError" : String(e);
}
out.push(`pMap.invalidConcurrency=${concurrencyErr}`);

// --- pMap: non-iterable input throws TypeError ---
let iterableErr = "";
try {
  await pMap(42 as never, async (v: number) => v);
} catch (e: unknown) {
  iterableErr = e instanceof TypeError ? "TypeError" : String(e);
}
out.push(`pMap.nonIterable=${iterableErr}`);

// --- pMapIterable: async iterator yields mapped values in order, respecting backpressure ---
const iter = pMapIterable([1, 2, 3, 4, 5], async (v: number, i: number) => {
  await tick(input.length - v);
  return v * 100 + i;
}, { concurrency: 2, backpressure: 4 });

const iterResults: number[] = [];
for await (const v of iter) {
  iterResults.push(v);
}
out.push(`pMapIterable.order=${JSON.stringify(iterResults)}`);

// --- pMapIterable: pMapSkip items are omitted from the yielded stream ---
const iterSkip = pMapIterable([1, 2, 3, 4], async (v: number) => {
  await tick(0);
  return v === 2 || v === 4 ? pMapSkip : v * 7;
}, { concurrency: 2 });

const iterSkipResults: number[] = [];
for await (const v of iterSkip) {
  iterSkipResults.push(v);
}
out.push(`pMapIterable.skip=${JSON.stringify(iterSkipResults)}`);

// --- pMap: empty iterable yields empty array ---
const empty = await pMap([], async (v: number) => v);
out.push(`pMap.empty=${JSON.stringify(empty)}`);

// --- pMap: async iterable input (generator) ---
async function* gen(): AsyncGenerator<number> {
  yield 1;
  yield 2;
  yield 3;
}
const asyncIterResult = await pMap(gen(), async (v: number) => v * 2);
out.push(`pMap.asyncInput=${JSON.stringify(asyncIterResult)}`);

// --- pMap: pMapSkip alone yields empty array ---
const allSkipped = await pMap([1, 2, 3], async () => pMapSkip);
out.push(`pMap.allSkipped=${JSON.stringify(allSkipped)}`);

// --- pMapIterable: invalid backpressure throws TypeError ---
let bpErr = "";
try {
  pMapIterable([1, 2], async (v: number) => v, { concurrency: 4, backpressure: 2 });
} catch (e: unknown) {
  bpErr = e instanceof TypeError ? "TypeError" : String(e);
}
out.push(`pMapIterable.badBackpressure=${bpErr}`);

// Emit all lines as deterministic, newline-separated stdout.
process.stdout.write(out.join("\n") + "\n");
