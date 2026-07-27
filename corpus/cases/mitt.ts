// BamTS corpus driver for mitt @ 6b41670516ed8e8b738612f60491995470aa63b3
// Exercises tiny functional event emitter (mitt) behavior:
// registration, dispatch, wildcard handling, removal, custom event map,
// symbol keys, and snapshot-safe mutation during emission.
// Uses Node 24 raw TypeScript; no external dependencies or Node built-ins.

import mitt from "../projects/mitt/src/index.ts";

function emitLog(label: string, val: unknown): void {
  process.stdout.write(`${label}: ${JSON.stringify(val)}\n`);
}

// Type definitions for typed events
type Events = {
  foo: string;
  bar: { count: number };
  empty: void;
  [symEvent]: number;
};

const symEvent = Symbol("symEvent");

// 1. Basic on and emit
const emitter = mitt<Events>();

const fooLogs: string[] = [];
emitter.on("foo", (data) => {
  fooLogs.push(`h1:${data}`);
});
emitter.on("foo", (data) => {
  fooLogs.push(`h2:${data.toUpperCase()}`);
});

emitter.emit("foo", "hello");
emitLog("basic-emit-foo", fooLogs);

// 2. Objects as event payload
let barCount = 0;
emitter.on("bar", (evt) => {
  barCount += evt.count;
});
emitter.emit("bar", { count: 10 });
emitter.emit("bar", { count: 5 });
emitLog("object-payload-bar", barCount);

// 3. Wildcard handlers ('*')
const wildcardLogs: string[] = [];
emitter.on("*", (type, event) => {
  wildcardLogs.push(`${String(type)}->${JSON.stringify(event)}`);
});

emitter.emit("foo", "world");
emitter.emit("bar", { count: 1 });
emitLog("wildcard-logs", wildcardLogs);

// 4. Specific handler removal (off)
const h1 = (data: string) => {
  fooLogs.push(`h3:${data}`);
};
emitter.on("foo", h1);
emitter.emit("foo", "test1");

emitter.off("foo", h1);
emitter.emit("foo", "test2");
emitLog("off-specific-foo-logs", fooLogs);

// 5. Remove all handlers for a type (off without handler argument)
emitter.off("foo");
emitter.emit("foo", "test3");
emitLog("off-all-foo-logs", fooLogs);

// 6. Wildcard removal
const wildcardH = (type: string | symbol, event: unknown) => {
  wildcardLogs.push(`temp-wildcard:${String(type)}`);
};
emitter.on("*", wildcardH);
emitter.emit("bar", { count: 0 });

emitter.off("*", wildcardH);
emitter.emit("bar", { count: 0 });
emitLog("off-wildcard-logs", wildcardLogs);

// 7. Custom initial Map (all)
const customMap = new Map();
const initialHandler = (val: string) => {
  customMapLogs.push(`initial:${val}`);
};
const customMapLogs: string[] = [];
customMap.set("foo", [initialHandler]);

const customEmitter = mitt<Events>(customMap);
customEmitter.emit("foo", "custom-init");
emitLog("custom-map-emit", customMapLogs);
emitLog("custom-map-has-foo", customEmitter.all.has("foo"));

// 8. Snapshot-safe mutation during emission
// Handlers modified during emit should not affect current emission cycle
const mutationLogs: string[] = [];
const selfRemovingEmitter = mitt<{ item: number }>();

const remover = (evt: number) => {
  mutationLogs.push(`remover:${evt}`);
  selfRemovingEmitter.off("item", remover);
  selfRemovingEmitter.on("item", (val) => mutationLogs.push(`new-handler:${val}`));
};

const secondHandler = (evt: number) => {
  mutationLogs.push(`second:${evt}`);
};

selfRemovingEmitter.on("item", remover);
selfRemovingEmitter.on("item", secondHandler);

selfRemovingEmitter.emit("item", 1);
selfRemovingEmitter.emit("item", 2);
emitLog("snapshot-mutation-logs", mutationLogs);

// 9. Symbol event keys
const symbolLogs: number[] = [];
emitter.on(symEvent, (num) => {
  symbolLogs.push(num * 2);
});
emitter.emit(symEvent, 21);
emitLog("symbol-event-logs", symbolLogs);

// 10. Emitting unhandled event or calling off on unhandled event
const unhandledEmitter = mitt<{ nonExistent: string }>();
unhandledEmitter.emit("nonExistent", "nobody-listening"); // Should not throw
const dummyFn = () => {};
unhandledEmitter.off("nonExistent", dummyFn); // Should not throw
emitLog("unhandled-safe", true);
