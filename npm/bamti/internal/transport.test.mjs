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
