import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test from "node:test";

import {
  deliverOutcome,
  encodeRequest,
  runNative,
} from "../bamti/native-runner.js";

function fakeProcess(platform = "linux") {
  return {
    platform,
    env: { INHERITED: "yes" },
    cwd: () => "/default",
    stderr: new RecordingStream("stderr"),
    stdout: new RecordingStream("stdout"),
  };
}

class RecordingStream extends EventEmitter {
  constructor(name, events = []) {
    super();
    this.name = name;
    this.events = events;
  }

  write(bytes, callback) {
    this.events.push(`${this.name}:${Buffer.from(bytes).toString("utf8")}`);
    queueMicrotask(callback);
    return false;
  }
}

const addon = {
  run: async () => ({ exitCode: 7, stdout: Buffer.from("out"), stderr: Buffer.from("err") }),
};

test("run validates only the argument-list shape synchronously", async () => {
  let loads = 0;
  const loadNativeAddon = () => {
    loads += 1;
    return addon;
  };
  assert.throws(
    () => runNative("check", {}, { loadNativeAddon }),
    /expects an array/,
  );
  assert.equal(loads, 0);

  const promise = runNative([42], {}, { loadNativeAddon, process: fakeProcess() });
  assert.equal(loads, 1);
  await assert.rejects(promise, /argument 0 must be a string/);
});

test("native package load failures remain synchronous", () => {
  const failure = new Error("native unavailable");
  assert.throws(
    () => runNative([], {}, { loadNativeAddon: () => { throw failure; } }),
    (error) => error === failure,
  );
});

test("request encoding snapshots cwd and replaces lone surrogates", () => {
  const processObject = fakeProcess();
  const { request, stdio } = encodeRequest(["\uD800"], {}, processObject);
  assert.equal(stdio, "inherit");
  assert.equal(request.cwd.toString(), "/default");
  assert.equal(request.args[0].toString("hex"), "efbfbd");
  assert.deepEqual(request.env.map((entry) => entry.toString()), ["INHERITED=yes"]);
});

test("environment serialization follows Node ordering and first-equals rules", () => {
  const unix = encodeRequest([], {
    cwd: "/tmp",
    env: { "A=B": "value", OMIT: undefined, NUMBER: 3 },
  }, fakeProcess()).request;
  assert.deepEqual(
    unix.env.map((entry) => entry.toString()),
    ["A=B=value", "NUMBER=3"],
  );

  const windows = encodeRequest([], {
    cwd: "C:\\work",
    env: { Path: "first", PATH: "second", alpha: "1" },
  }, fakeProcess("win32")).request;
  assert.deepEqual(
    windows.env.map((entry) => entry.toString()),
    ["PATH=second", "alpha=1"],
  );
});

test("NUL and invalid stdio failures reject the returned promise", async () => {
  const dependencies = { loadNativeAddon: () => addon, process: fakeProcess() };
  await assert.rejects(runNative(["bad\0arg"], {}, dependencies), /NUL/);
  await assert.rejects(runNative([], { cwd: "bad\0cwd" }, dependencies), /NUL/);
  await assert.rejects(runNative([], { env: { BAD: "x\0y" } }, dependencies), /NUL/);
  await assert.rejects(runNative([], { stdio: "wrong" }, dependencies), /stdio/);
});

test("inherit writes stderr then stdout and honors completion callbacks", async () => {
  const events = [];
  const processObject = fakeProcess();
  processObject.stderr = new RecordingStream("stderr", events);
  processObject.stdout = new RecordingStream("stdout", events);
  const code = await runNative([], {}, {
    loadNativeAddon: () => addon,
    process: processObject,
  });
  assert.equal(code, 7);
  assert.deepEqual(events, ["stderr:err", "stdout:out"]);
});

test("stream failure maps to exit code one and stops later output", async () => {
  const events = [];
  const processObject = fakeProcess();
  processObject.stderr = new RecordingStream("stderr", events);
  processObject.stderr.write = (_bytes, callback) => {
    events.push("stderr:failed");
    queueMicrotask(() => callback(new Error("closed")));
    return false;
  };
  processObject.stdout = new RecordingStream("stdout", events);
  assert.equal(await deliverOutcome(await addon.run(), "inherit", processObject), 1);
  assert.deepEqual(events, ["stderr:failed"]);
});

test("ignore and pipe discard native buffers", async () => {
  const processObject = fakeProcess();
  processObject.stderr.write = () => { throw new Error("must not write"); };
  processObject.stdout.write = () => { throw new Error("must not write"); };
  const outcome = await addon.run();
  assert.equal(await deliverOutcome(outcome, "ignore", processObject), 7);
  assert.equal(await deliverOutcome(outcome, "pipe", processObject), 7);
});
