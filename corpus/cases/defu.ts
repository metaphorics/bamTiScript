// BamTS corpus driver for defu @ 82632b66f5914e9946edce300e10633a3d5c0cb7
// Exercises the project-specific plain object detection utility (_utils.ts)
// which guards prototype pollution and determines object merger behavior.
// Uses Node 24 raw TypeScript; no external dependencies or Node built-ins.

import { isPlainObject } from "../projects/defu/src/_utils.ts";

function emit(label: string, val: unknown): void {
  process.stdout.write(`${label}: ${JSON.stringify(isPlainObject(val))}\n`);
}

// 1. Primitives and nullish values
emit("null", null);
emit("undefined", undefined);
emit("number", 42);
emit("string", "hello");
emit("boolean", true);

// 2. Plain objects and Object.create(null)
emit("plain-empty", {});
emit("plain-props", { a: 1, b: "two" });
emit("null-proto", Object.create(null));

// 3. Nested prototype chains
const protoObj = Object.create({});
emit("nested-proto", protoObj);

// 4. Arrays and built-in objects
emit("array", [1, 2, 3]);
emit("date", new Date());

// 5. Custom class instances
class CustomClass {
  foo = "bar";
}
emit("custom-class", new CustomClass());

// 6. Custom iterables
const customIterable = {
  [Symbol.iterator]() {
    return { next: () => ({ done: true, value: undefined }) };
  },
};
emit("custom-iterable", customIterable);

// 7. Symbol.toStringTag handling
const moduleTag = { [Symbol.toStringTag]: "Module" };
const otherTag = { [Symbol.toStringTag]: "Custom" };
emit("module-tag", moduleTag);
emit("other-tag", otherTag);
