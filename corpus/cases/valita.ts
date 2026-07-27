// Corpus driver for @badrap/valita — exercises the schema validation
// engine from the vendored single-file source tree.
//
// valita's entire library is `src/index.ts` with zero runtime imports,
// so it resolves cleanly under Node 24's native TypeScript stripping
// with an explicit `.ts` specifier and no special flags.

import * as v from "../projects/valita/src/index.ts";

function emit(label: string, value: unknown): void {
  const s = typeof value === "string" ? value : String(value);
  process.stdout.write(`${label}=${s}\n`);
}

// --- Primitive type validators ---

emit("string-ok", v.string().try("hello").ok);
emit("string-bad", v.string().try(42).ok);
emit("number-ok", v.number().try(42).ok);
emit("number-bad", v.number().try("x").ok);
emit("boolean-ok", v.boolean().try(true).ok);
emit("boolean-bad", v.boolean().try(0).ok);
emit("bigint-ok", v.bigint().try(10n).ok);
emit("bigint-bad", v.bigint().try(10).ok);

// --- Literal validator ---

emit("literal-ok", v.literal("red").try("red").ok);
emit("literal-bad", v.literal("red").try("blue").ok);

// --- Object validator: success path (fixed key order for determinism) ---

const point = v.object({
  x: v.number(),
  y: v.number(),
});

const pOk = point.try({ x: 1, y: 2 });
emit("obj-ok", pOk.ok);
if (pOk.ok) {
  emit("obj-val", JSON.stringify(pOk.value));
}

// Object with extra keys — default mode is strict (forbid extra)
const pExtra = point.try({ x: 1, y: 2, z: 3 });
emit("obj-extra-strict", pExtra.ok);

// Strip mode removes extra keys
const pStrip = point.try({ x: 1, y: 2, z: 3 }, { mode: "strip" });
emit("obj-extra-strip", pStrip.ok);
if (pStrip.ok) {
  emit("obj-strip-val", JSON.stringify(pStrip.value));
}

// Passthrough mode keeps extra keys
const pPass = point.try({ x: 1, y: 2, z: 3 }, { mode: "passthrough" });
emit("obj-extra-pass", pPass.ok);
if (pPass.ok) {
  emit("obj-pass-val", JSON.stringify(pPass.value));
}

// Missing required key
const pMissing = point.try({ x: 1 });
emit("obj-missing", pMissing.ok);

// --- Union of literal-tagged objects (discriminated union) ---

const vehicle = v.union(
  v.object({ type: v.literal("bike"), gears: v.number() }),
  v.object({ type: v.literal("car"), wheels: v.number() }),
);

// Valid: bike
const bikeOk = vehicle.try({ type: "bike", gears: 6 });
emit("union-bike-ok", bikeOk.ok);
if (bikeOk.ok) {
  emit("union-bike-val", JSON.stringify(bikeOk.value));
}

// Valid: car
const carOk = vehicle.try({ type: "car", wheels: 4 });
emit("union-car-ok", carOk.ok);
if (carOk.ok) {
  emit("union-car-val", JSON.stringify(carOk.value));
}

// Invalid: unknown discriminator
const badVehicle = vehicle.try({ type: "plane", wings: 2 });
emit("union-bad-ok", badVehicle.ok);
if (!badVehicle.ok) {
  const issues = badVehicle.issues;
  emit("union-bad-count", issues.length);
  for (const issue of issues) {
    const pathStr = issue.path.join(".");
    emit("union-bad-issue", `${issue.code}@${pathStr}`);
  }
}

// --- Array validator ---

const strArray = v.array(v.string());

const arrOk = strArray.try(["a", "b", "c"]);
emit("arr-ok", arrOk.ok);
if (arrOk.ok) {
  emit("arr-val", JSON.stringify(arrOk.value));
}

const arrBad = strArray.try(["a", 2, "c"]);
emit("arr-bad", arrBad.ok);
if (!arrBad.ok) {
  for (const issue of arrBad.issues) {
    emit("arr-bad-issue", `${issue.code}@${issue.path.join(".")}`);
  }
}

// --- Record validator (Record<string, number>) ---

const numRecord = v.record(v.number());
const recOk = numRecord.try({ a: 1, b: 2 });
emit("rec-ok", recOk.ok);
if (recOk.ok) {
  emit("rec-val", JSON.stringify(recOk.value));
}

const recBad = numRecord.try({ a: 1, b: "x" });
emit("rec-bad", recBad.ok);

// --- Tuple validator ---

const pair = v.tuple([v.string(), v.number()]);
const tupOk = pair.try(["age", 42]);
emit("tup-ok", tupOk.ok);
if (tupOk.ok) {
  emit("tup-val", JSON.stringify(tupOk.value));
}

const tupBad = pair.try(["age", "42"]);
emit("tup-bad", tupBad.ok);

// --- .assert() refinement ---

const intNum = v.number().assert((n) => Number.isInteger(n), "not an integer");
emit("assert-int-ok", intNum.try(3).ok);
const assertBad = intNum.try(3.5);
emit("assert-int-bad", assertBad.ok);
if (!assertBad.ok) {
  emit("assert-int-issue", assertBad.issues[0].code);
}

// --- .map() transformation ---

const len = v.string().map((s) => s.length);
const mapped = len.try("hello");
emit("map-ok", mapped.ok);
if (mapped.ok) {
  emit("map-val", mapped.value);
}

// --- .optional() with default ---

const withDefault = v.object({
  name: v.string(),
  age: v.number().optional(() => 0),
});
const optOk = withDefault.try({ name: "alice" });
emit("opt-ok", optOk.ok);
if (optOk.ok) {
  emit("opt-val", JSON.stringify(optOk.value));
}

// --- .nullable() ---

const nullableStr = v.string().nullable();
emit("null-ok-str", nullableStr.try("hi").ok);
emit("null-ok-null", nullableStr.try(null).ok);
emit("null-bad-num", nullableStr.try(5).ok);

// --- parse() throws on failure ---

try {
  v.string().parse(42);
  emit("parse-threw", "no");
} catch (e) {
  emit("parse-threw", "yes");
  if (e instanceof v.ValitaError) {
    emit("parse-error-name", e.name);
  }
}
