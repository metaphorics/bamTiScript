// F2.1 Contract: package manifest, tsc executable, persistent API transport,
// and thirteen-subpath export map.
//
// Asserts the bamti package manifest declares all 13 export subpaths,
// the tsc bin entry, the package.json self-import, and the root index.js
// exports the public API surface (Session, createSession, Transport, errors,
// native loader, run, runNative).

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const npmRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const pkgPath = join(npmRoot, "bamti", "package.json");
const pkg = JSON.parse(await readFile(pkgPath, "utf8"));

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

// --- Manifest: 13 export subpaths ---
const EXPECTED_EXPORTS = [
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

check("manifest has 13 exports", () => {
  const keys = Object.keys(pkg.exports);
  assert.equal(keys.length, 13, `expected 13 exports, got ${keys.length}: ${keys.join(", ")}`);
});

for (const sub of EXPECTED_EXPORTS) {
  check(`export ${sub} exists`, () => {
    assert.ok(pkg.exports[sub], `missing export ${sub}`);
  });
}

// --- Manifest: tsc bin entry ---
check("bin.tsc points to native-runner.js", () => {
  assert.equal(pkg.bin?.tsc, "native-runner.js");
});

// --- Manifest: package metadata ---
check("name is bamti", () => assert.equal(pkg.name, "bamti"));
check("type is module", () => assert.equal(pkg.type, "module"));
check("engines.node >=24", () => {
  assert.ok(pkg.engines?.node?.includes("24"), `engines.node=${pkg.engines?.node}`);
});

// --- Root index.js exports ---
const mod = await import("bamti");

check("Session exported", () => assert.ok(typeof mod.Session === "function"));
check("createSession exported", () => assert.ok(typeof mod.createSession === "function"));
check("Transport exported", () => assert.ok(typeof mod.Transport === "function"));
check("run exported", () => assert.ok(typeof mod.run === "function"));
check("runNative exported", () => assert.ok(typeof mod.runNative === "function"));
check("BamtiError exported", () => assert.ok(mod.BamtiError));
check("DisposedError exported", () => assert.ok(mod.DisposedError));
check("ProtocolError exported", () => assert.ok(mod.ProtocolError));
check("TransportBusyError exported", () => assert.ok(mod.TransportBusyError));
check("TransportCrashError exported", () => assert.ok(mod.TransportCrashError));
check("loadNativeAddon exported", () => assert.ok(typeof mod.loadNativeAddon === "function"));
check("selectNativeTarget exported", () => assert.ok(typeof mod.selectNativeTarget === "function"));
check("NativeArtifactLoadError exported", () => assert.ok(mod.NativeArtifactLoadError));
check("NativeArtifactNotFoundError exported", () => assert.ok(mod.NativeArtifactNotFoundError));
check("UnsupportedPlatformError exported", () => assert.ok(mod.UnsupportedPlatformError));
check("ArtifactNotFoundError exported", () => assert.ok(mod.ArtifactNotFoundError));
check("artifactPackage exported", () => assert.ok(typeof mod.artifactPackage === "function"));
check("resolveBinary exported", () => assert.ok(typeof mod.resolveBinary === "function"));

// --- package.json self-import ---
check("package.json self-import works", async () => {
  const pkgMod = await import("bamti/package.json", { with: { type: "json" } });
  assert.equal(pkgMod.default.name, "bamti");
});
await (() => {});

// --- Internal transport files exist ---
import { access } from "node:fs/promises";
check("internal/errors.js exists", async () => {
  await access(join(npmRoot, "bamti", "internal", "errors.js"));
});
check("internal/framing.js exists", async () => {
  await access(join(npmRoot, "bamti", "internal", "framing.js"));
});
check("internal/session.js exists", async () => {
  await access(join(npmRoot, "bamti", "internal", "session.js"));
});
check("internal/transport.js exists", async () => {
  await access(join(npmRoot, "bamti", "internal", "transport.js"));
});

// --- tsc CLI binary exists ---
import { existsSync } from "node:fs";
check("native-runner.js exists", () => {
  assert.ok(existsSync(join(npmRoot, "bamti", "native-runner.js")));
});
check("native-loader.js exists", () => {
  assert.ok(existsSync(join(npmRoot, "bamti", "native-loader.js")));
});

// --- Prebuilt release CLI works ---
import { spawnSync } from "node:child_process";
const CLI = process.env.BAMTS_CLI ?? join(process.env.HOME, ".cache", "bamts-main-target", "release", "bamts");
check("prebuilt CLI --version succeeds", () => {
  const r = spawnSync(CLI, ["--version"], { encoding: "utf8", timeout: 10000 });
  assert.equal(r.status, 0, `exit ${r.status}, stderr=${r.stderr}`);
  assert.ok(r.stdout.trim().length > 0, `stdout empty`);
});

// --- Report ---
const total = pass + fail.length;
process.stdout.write(`F2.1_CONTRACT ${pass}/${total} pass\n`);
if (fail.length > 0) {
  for (const f of fail) process.stdout.write(`FAIL: ${f}\n`);
  process.exit(1);
}
