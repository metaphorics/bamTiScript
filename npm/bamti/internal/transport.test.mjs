import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import { registerHooks } from "node:module";
import { PassThrough } from "node:stream";
import test from "node:test";

import { FrameDecoder, encodeFrame } from "./framing.js";

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "bamti-cli") {
      return {
        shortCircuit: true,
        url: new URL("../../bamti-cli/index.js", import.meta.url).href,
      };
    }
    return nextResolve(specifier, context);
  },
});

const { Transport } = await import("./transport.js");
const { runExecutable } = await import("../native-runner.js");
const api = await import("../index.js");
const { SERVICE_METHODS } = await import("./session.js");
const { createAsyncService } = await import("../unstable/async.js");
const { createSerialService } = await import("../unstable/sync.js");

const PUBLIC_EXPORTS = [
  ".",
  "./unstable/sync",
  "./unstable/async",
  "./unstable/fs",
  "./unstable/proto",
  "./unstable/ast",
  "./unstable/ast/is",
  "./unstable/ast/factory",
  "./unstable/ast/utils",
  "./unstable/ast/scanner",
  "./unstable/ast/visitor",
  "./unstable/ast/clone",
  "./package.json",
];

class FakeChild extends EventEmitter {
  constructor() {
    super();
    this.stdin = new PassThrough();
    this.stdout = new PassThrough();
    this.stderr = new PassThrough();
    this.exitCode = null;
    this.signalCode = null;
  }

  kill(signal = "SIGTERM") {
    this.signalCode = signal;
    queueMicrotask(() => this.emit("exit", this.exitCode, this.signalCode));
    return true;
  }
}

function requestCollector(child) {
  const decoder = new FrameDecoder();
  const requests = [];
  child.stdin.on("data", (chunk) => requests.push(...decoder.push(chunk)));
  return requests;
}

function requestFixture() {
  const children = [];
  const requests = [];
  const decoder = new FrameDecoder();
  const transport = new Transport({
    binary: "/virtual/bamts",
    disposeTimeoutMs: 0,
    spawnChild() {
      const child = new FakeChild();
      child.stdin.on("data", (chunk) => requests.push(...decoder.push(chunk)));
      children.push(child);
      return child;
    },
  });
  return { transport, children, requests };
}

async function awaitRequests(requests, count) {
  while (requests.length < count) {
    await new Promise((resolve) => setImmediate(resolve));
  }
}

test("manifest publishes thirteen subpaths and the tsc executable", async () => {
  const manifest = JSON.parse(
    await readFile(new URL("../package.json", import.meta.url), "utf8"),
  );
  assert.deepEqual(Object.keys(manifest.exports), PUBLIC_EXPORTS);
  assert.deepEqual(manifest.bin, { tsc: "native-runner.js" });
  assert.ok(manifest.files.includes(manifest.bin.tsc));
  assert.equal(typeof api.createSession, "function");
});

test("persistent transport correlates fragmented out-of-order responses", async () => {
  const children = [];
  const transport = new Transport({
    binary: "/virtual/bamts",
    disposeTimeoutMs: 0,
    spawnChild() {
      const child = new FakeChild();
      children.push(child);
      return child;
    },
  });

  transport.ready();
  assert.equal(children.length, 1);
  const requests = requestCollector(children[0]);

  const first = transport.request("service/snapshot", { label: "first" });
  const second = transport.request("service/snapshot", { label: "second" });
  const third = transport.request("service/snapshot", { label: "third" });
  const fourth = transport.request("service/snapshot", { label: "fourth" });
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(
    requests.map(({ method }) => method),
    Array(4).fill("service/snapshot"),
  );

  const combined = Buffer.concat([
    encodeFrame({ jsonrpc: "2.0", id: requests[3].id, result: "四" }),
    encodeFrame({ jsonrpc: "2.0", id: requests[2].id, result: "three" }),
  ]);
  children[0].stdout.write(combined.subarray(0, 7));
  children[0].stdout.write(combined.subarray(7, combined.length - 2));
  children[0].stdout.write(combined.subarray(combined.length - 2));

  assert.deepEqual(await Promise.all([third, fourth]), ["three", "四"]);
  assert.equal(transport.inFlight, 2);

  const firstCrash = assert.rejects(first, { name: "TransportCrashError", code: 9 });
  const secondCrash = assert.rejects(second, { name: "TransportCrashError", code: 9 });
  children[0].emit("exit", 9, null);
  await Promise.all([firstCrash, secondCrash]);
  assert.equal(transport.inFlight, 0);
  await transport.dispose();
});

test("session publishes exactly the ten service methods including quickInfo", () => {
  assert.deepEqual(SERVICE_METHODS, [
    "open",
    "update",
    "close",
    "snapshot",
    "completions",
    "definition",
    "quickInfo",
    "references",
    "rename",
    "diagnostics",
  ]);
});

test("session quickInfo returns the populated symbol description over service/quickInfo", async () => {
  const { transport, children, requests } = requestFixture();
  const session = new api.Session({ transport });
  try {
    const pending = session.quickInfo({ path: "input.ts", position: 6 });
    await awaitRequests(requests, 1);
    children[0].stdout.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: requests[0].id,
        result: { root: "/virtual", methods: [...SERVICE_METHODS] },
      }),
    );
    await awaitRequests(requests, 2);
    assert.deepEqual(
      requests.map(({ method }) => method),
      ["initialize", "service/quickInfo"],
    );
    assert.deepEqual(requests[1].params, { path: "input.ts", position: 6 });
    children[0].stdout.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: requests[1].id,
        result: {
          name: "answer",
          kind: "const",
          typeDisplay: "42",
          display: "const answer: 42",
          range: { start: 6, end: 12 },
        },
      }),
    );
    assert.deepEqual(await pending, {
      name: "answer",
      kind: "const",
      typeDisplay: "42",
      display: "const answer: 42",
      range: { start: 6, end: 12 },
    });
  } finally {
    await transport.dispose();
  }
});

test("session quickInfo resolves null when no symbol sits under the position", async () => {
  const { transport, children, requests } = requestFixture();
  const session = new api.Session({ transport });
  try {
    const pending = session.quickInfo({ path: "input.ts", position: 0 });
    await awaitRequests(requests, 1);
    children[0].stdout.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: requests[0].id,
        result: { root: "/virtual", methods: [...SERVICE_METHODS] },
      }),
    );
    await awaitRequests(requests, 2);
    assert.equal(requests[1].method, "service/quickInfo");
    assert.deepEqual(requests[1].params, { path: "input.ts", position: 0 });
    children[0].stdout.write(
      encodeFrame({ jsonrpc: "2.0", id: requests[1].id, result: null }),
    );
    assert.equal(await pending, null);
  } finally {
    await transport.dispose();
  }
});

test("async and serial services wire quickInfo with the exact method name and request shape", async () => {
  const asyncFixture = requestFixture();
  const serialFixture = requestFixture();
  const asyncService = createAsyncService({ transport: asyncFixture.transport });
  const serialService = createSerialService({ transport: serialFixture.transport });
  try {
    const asyncPending = asyncService.quickInfo({ path: "input.ts", position: 6 });
    const serialPending = serialService.quickInfo({ path: "input.ts", position: 6 });
    await awaitRequests(asyncFixture.requests, 1);
    await awaitRequests(serialFixture.requests, 1);
    asyncFixture.children[0].stdout.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: asyncFixture.requests[0].id,
        result: { root: "/virtual", methods: [...SERVICE_METHODS] },
      }),
    );
    serialFixture.children[0].stdout.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: serialFixture.requests[0].id,
        result: { root: "/virtual", methods: [...SERVICE_METHODS] },
      }),
    );
    await awaitRequests(asyncFixture.requests, 2);
    await awaitRequests(serialFixture.requests, 2);
    assert.deepEqual(
      asyncFixture.requests.map(({ method }) => method),
      ["initialize", "service/quickInfo"],
    );
    assert.deepEqual(
      serialFixture.requests.map(({ method }) => method),
      ["initialize", "service/quickInfo"],
    );
    assert.deepEqual(asyncFixture.requests[1].params, {
      path: "input.ts",
      position: 6,
      async: true,
    });
    assert.deepEqual(serialFixture.requests[1].params, { path: "input.ts", position: 6 });
    asyncFixture.children[0].stdout.write(
      encodeFrame({
        jsonrpc: "2.0",
        id: asyncFixture.requests[1].id,
        result: {
          name: "answer",
          kind: "const",
          typeDisplay: "42",
          display: "const answer: 42",
          range: { start: 6, end: 12 },
        },
      }),
    );
    serialFixture.children[0].stdout.write(
      encodeFrame({ jsonrpc: "2.0", id: serialFixture.requests[1].id, result: null }),
    );
    assert.deepEqual(await asyncPending, {
      name: "answer",
      kind: "const",
      typeDisplay: "42",
      display: "const answer: 42",
      range: { start: 6, end: 12 },
    });
    assert.equal(await serialPending, null);
  } finally {
    await asyncService.dispose();
    await serialService.dispose();
  }
});

test("tsc executable forwards arguments and records the CLI exit status", async () => {
  const stderr = new PassThrough();
  const processObject = { stderr, exitCode: undefined };
  let invocation;
  const exitCode = await runExecutable(["--version"], {
    process: processObject,
    runCli(args, options) {
      invocation = { args, options };
      return 7;
    },
  });

  assert.equal(exitCode, 7);
  assert.equal(processObject.exitCode, 7);
  assert.deepEqual(invocation, { args: ["--version"], options: { stdio: "inherit" } });
});
