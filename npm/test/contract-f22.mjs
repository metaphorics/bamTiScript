// F2.2 Contract: sync, async, filesystem, protocol, project, checker,
// snapshot, completion, and reference API behavior.
//
// Asserts the sync/async/fs/proto subpaths import and expose the expected
// API surface, and that the protocol framing roundtrips correctly.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const npmRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

let pass = 0;
const fail = [];

function check(name, fn) {
  try {
    fn();
    pass++;
  } catch (err) {
    fail.push(`${name}: ${err.message}`);
  }
}

// --- sync subpath ---
const sync = await import("bamti/unstable/sync");
check("sync.createSerialService is function", () => {
  assert.ok(typeof sync.createSerialService === "function");
});
check("sync has 10 service methods + request/dispose", () => {
  const svc = sync.createSerialService({ root: "/tmp" });
  const methods = ["open", "update", "close", "snapshot", "completions",
    "definition", "quickInfo", "references", "rename", "diagnostics"];
  for (const m of methods) {
    assert.ok(typeof svc[m] === "function", `sync.${m} missing`);
  }
  assert.ok(typeof svc.request === "function");
  assert.ok(typeof svc.dispose === "function");
  svc.dispose();
});

// --- async subpath ---
const asyncMod = await import("bamti/unstable/async");
check("async.createAsyncService is function", () => {
  assert.ok(typeof asyncMod.createAsyncService === "function");
});
check("async has 10 service methods + request/dispose", () => {
  const svc = asyncMod.createAsyncService({ root: "/tmp" });
  const methods = ["open", "update", "close", "snapshot", "completions",
    "definition", "quickInfo", "references", "rename", "diagnostics"];
  for (const m of methods) {
    assert.ok(typeof svc[m] === "function", `async.${m} missing`);
  }
  assert.ok(typeof svc.request === "function");
  assert.ok(typeof svc.dispose === "function");
  svc.dispose();
});

// --- fs subpath ---
const fs = await import("bamti/unstable/fs");
check("fs.osFileSystem is function", () => {
  assert.ok(typeof fs.osFileSystem === "function");
});
check("fs.osFileSystem returns frozen descriptor", () => {
  const fsys = fs.osFileSystem("/tmp");
  assert.equal(fsys.kind, "os");
  assert.ok(typeof fsys.root === "string");
  assert.ok(Object.isFrozen(fsys));
});

// --- proto subpath ---
const proto = await import("bamti/unstable/proto");
check("proto.encodeFrame is function", () => {
  assert.ok(typeof proto.encodeFrame === "function");
});
check("proto.FrameDecoder is function", () => {
  assert.ok(typeof proto.FrameDecoder === "function");
});
check("proto.MAX_FRAME_BYTES is number", () => {
  assert.ok(typeof proto.MAX_FRAME_BYTES === "number");
});
check("proto.MAX_HEADER_BYTES is number", () => {
  assert.ok(typeof proto.MAX_HEADER_BYTES === "number");
});
check("proto.Transport is function", () => {
  assert.ok(typeof proto.Transport === "function");
});
check("proto exports 7 error classes", () => {
  for (const e of ["BamtiError", "DisposedError", "ProtocolError",
    "RestartLimitError", "ServiceRequestError", "TransportBusyError",
    "TransportCrashError"]) {
    assert.ok(proto[e], `proto.${e} missing`);
  }
});
check("encodeFrame/FrameDecoder roundtrip", () => {
  const message = { method: "service/open", params: {} };
  const frame = proto.encodeFrame(message);
  assert.ok(frame instanceof Uint8Array);
  assert.ok(frame.length > 0, "frame must be non-empty");

  const decoder = new proto.FrameDecoder();
  const msgs = decoder.push(frame);
  assert.equal(msgs.length, 1, "decoder must yield exactly one message");
  assert.deepEqual(msgs[0], message, "decoded message must match original");
});

check("encodeFrame rejects oversized payload", () => {
  const huge = "x".repeat(proto.MAX_FRAME_BYTES + 1);
  assert.throws(() => proto.encodeFrame({ data: huge }), /frame/);
});

// --- Report ---
const total = pass + fail.length;
process.stdout.write(`F2.2_CONTRACT ${pass}/${total} pass\n`);
if (fail.length > 0) {
  for (const f of fail) process.stdout.write(`FAIL: ${f}\n`);
  process.exit(1);
}
