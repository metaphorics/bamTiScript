import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { dirname } from "node:path";
import test from "node:test";

import {
  loadNativeAddon,
  NativeArtifactLoadError,
  NativeArtifactNotFoundError,
  NATIVE_TARGETS,
  selectNativeTarget,
  UnsupportedPlatformError,
} from "../bamti/native-loader.js";

const PACKAGE_VERSION = "0.1.0";
const SOURCE_COMMIT = "abcdef1234567890";
const BUILD_SET_ID = "build-42";
const NATIVE_ABI = 2;
const CLI_PROTOCOL = 3;
const RELEASE_ID = `bamti/${PACKAGE_VERSION}/${SOURCE_COMMIT}/native-abi-${NATIVE_ABI}/cli-protocol-${CLI_PROTOCOL}/${BUILD_SET_ID}`;

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function makeRelease(overrides = {}) {
  return {
    packageVersion: PACKAGE_VERSION,
    sourceCommit: SOURCE_COMMIT,
    buildSetId: BUILD_SET_ID,
    releaseId: RELEASE_ID,
    nativeAbi: NATIVE_ABI,
    cliProtocol: CLI_PROTOCOL,
    ...overrides,
  };
}

function makeTable() {
  return {
    version: 1,
    release: makeRelease(),
    targets: NATIVE_TARGETS.map((fixed) => ({
      selector: fixed.selector,
      target: fixed.target,
      package: fixed.package,
      entry: fixed.entry,
      os: fixed.os,
      cpu: fixed.cpu,
      libc: fixed.libc,
      artifactKind: fixed.artifactKind,
      version: PACKAGE_VERSION,
      sha256: "0".repeat(64),
    })),
  };
}

function makeManifest(row, release) {
  return {
    name: row.package,
    version: row.version,
    main: row.entry,
    engines: { node: ">=24" },
    os: [row.os],
    cpu: [row.cpu],
    libc: row.libc === undefined ? undefined : [row.libc],
    bamtiNative: {
      entry: row.entry,
      target: row.target,
      artifactKind: row.artifactKind,
      sha256: row.sha256,
      release,
    },
  };
}

function makeAddon(row, release) {
  return {
    releaseMetadata() {
      return { ...release, target: row.target, artifactKind: row.artifactKind };
    },
    run() {
      return 0;
    },
  };
}

function createFixture(selectedSelector = "linux-x64-gnu") {
  const table = makeTable();
  const row = table.targets.find((r) => r.selector === selectedSelector);
  const release = table.release;
  const addonBytes = Buffer.from(`bamti-${row.package}`);
  const digest = sha256(addonBytes);
  row.sha256 = digest;
  const manifest = makeManifest(row, release);
  manifest.bamtiNative.sha256 = digest;
  const addon = makeAddon(row, release);
  const manifestPath = `/node_modules/${row.package}/package.json`;
  const packageRoot = dirname(manifestPath);
  const addonPath = `${packageRoot}/${row.entry}`;
  const stagedPath = `/private/bamti-staging/${row.package.replace(/\//g, "-")}.node`;
  const files = new Map([
    [manifestPath, Buffer.from(JSON.stringify(manifest))],
    [addonPath, addonBytes],
  ]);
  const calls = [];
  const options = {
    table,
    host: {
      platform: row.os,
      arch: row.cpu,
      libc: row.libc,
      nodeVersion: "24.0.0",
      napiVersion: "10",
    },
    resolver: {
      resolve: (spec) => {
        calls.push(["resolve", spec]);
        return `/node_modules/${spec}`;
      },
    },
    readFile: (path) => {
      calls.push(["readFile", path]);
      if (!files.has(path)) {
        const error = new Error(`ENOENT: ${path}`);
        error.code = "ENOENT";
        throw error;
      }
      return files.get(path);
    },
    realpath: (path) => {
      calls.push(["realpath", path]);
      return path;
    },
    stage: (bytes) => {
      calls.push(["stage", bytes]);
      assert.equal(bytes, addonBytes);
      files.set(stagedPath, bytes);
      return stagedPath;
    },
    requireAddon: (path) => {
      calls.push(["requireAddon", path]);
      assert.equal(path, stagedPath);
      assert.equal(files.get(path), addonBytes);
      return addon;
    },
    cache: { value: undefined },
  };
  return {
    table,
    row,
    release,
    manifest,
    addon,
    addonBytes,
    digest,
    manifestPath,
    packageRoot,
    addonPath,
    stagedPath,
    files,
    calls,
    options,
  };
}

function setPath(object, path, value) {
  const parts = path.split(".");
  let current = object;
  for (let i = 0; i < parts.length - 1; i += 1) {
    current = current[parts[i]];
  }
  current[parts[parts.length - 1]] = value;
}

function moduleNotFound() {
  const error = new Error("Cannot find module");
  error.code = "MODULE_NOT_FOUND";
  return error;
}

function assertLoadError(thunk, predicate, message) {
  assert.throws(thunk, (err) => err instanceof NativeArtifactLoadError && predicate(err.cause), message);
}

test("selectNativeTarget supports exactly five published selectors", () => {
  assert.equal(NATIVE_TARGETS.length, 5);
  const cases = [
    { platform: "linux", arch: "x64", libc: "glibc", selector: "linux-x64-gnu" },
    { platform: "linux", arch: "arm64", libc: "glibc", selector: "linux-arm64-gnu" },
    { platform: "darwin", arch: "x64", selector: "darwin-x64" },
    { platform: "darwin", arch: "arm64", selector: "darwin-arm64" },
    { platform: "win32", arch: "x64", selector: "win32-x64-msvc" },
  ];
  for (const host of cases) {
    assert.equal(selectNativeTarget(host).selector, host.selector);
  }
});

test("selectNativeTarget rejects musl, unknown and missing libc on linux", () => {
  for (const libc of ["musl", "unknown"]) {
    assert.throws(
      () => selectNativeTarget({ platform: "linux", arch: "x64", libc }),
      (err) => err instanceof UnsupportedPlatformError && err.message.includes(`libc: ${libc}`),
    );
  }
  assert.throws(
    () => selectNativeTarget({ platform: "linux", arch: "x64" }),
    (err) => err instanceof UnsupportedPlatformError && err.message.includes("libc: unknown"),
  );
});

test("unsupported host selection occurs before table or package resolution", () => {
  const fixture = createFixture();
  fixture.options.host.libc = "musl";
  let resolverCalled = false;
  let readFileCalled = false;
  fixture.options.table = "./native-release-table.json";
  fixture.options.resolver.resolve = (spec) => {
    resolverCalled = true;
    throw new Error(`resolver should not be called for ${spec}`);
  };
  fixture.options.readFile = () => {
    readFileCalled = true;
    throw new Error("readFile should not be called");
  };
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) => err instanceof UnsupportedPlatformError,
  );
  assert.equal(resolverCalled, false);
  assert.equal(readFileCalled, false);
});

test("table resolution and parse failures are surfaced as NativeArtifactLoadError", () => {
  const fixture = createFixture();
  fixture.options.table = "./native-release-table.json";
  const notFound = moduleNotFound();
  fixture.options.resolver.resolve = (spec) => {
    if (spec === "./native-release-table.json") throw notFound;
    return `/node_modules/${spec}`;
  };
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) => err instanceof NativeArtifactLoadError && err.cause === notFound,
  );
});

test("malformed table JSON is surfaced as NativeArtifactLoadError", () => {
  const fixture = createFixture();
  fixture.options.table = "./native-release-table.json";
  const tablePath = "/abs/native-release-table.json";
  fixture.options.resolver.resolve = (spec) =>
    spec === "./native-release-table.json" ? tablePath : `/node_modules/${spec}`;
  fixture.files.set(tablePath, Buffer.from("not json"));
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) =>
      err instanceof NativeArtifactLoadError &&
      err.cause instanceof Error &&
      /is not valid JSON/.test(err.cause.message),
  );
});

test("table can be provided as a resolvable string", () => {
  const fixture = createFixture();
  fixture.options.table = "./native-release-table.json";
  const tablePath = "/abs/native-release-table.json";
  fixture.options.resolver.resolve = (spec) =>
    spec === "./native-release-table.json" ? tablePath : `/node_modules/${spec}`;
  fixture.files.set(tablePath, Buffer.from(JSON.stringify(fixture.table)));
  const addon = loadNativeAddon(fixture.options);
  assert.equal(addon, fixture.addon);
});

test("missing manifest package raises NativeArtifactNotFoundError", () => {
  const fixture = createFixture();
  const notFound = moduleNotFound();
  fixture.options.resolver.resolve = (spec) => {
    throw notFound;
  };
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) => err instanceof NativeArtifactNotFoundError && err.cause === notFound,
  );
});

test("MODULE_NOT_FOUND from requireAddon after manifest is a NativeArtifactLoadError", () => {
  const fixture = createFixture();
  const notFound = moduleNotFound();
  fixture.options.requireAddon = () => {
    throw notFound;
  };
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) => err instanceof NativeArtifactLoadError && err.cause === notFound,
  );
});

test("stale manifest fields are rejected", () => {
  const cases = [
    { path: "name", value: "wrong", message: "package metadata name does not match release table" },
    { path: "version", value: "0.2.0", message: "package metadata version does not match release table" },
    { path: "main", value: "wrong.node", message: "package metadata main does not match release table" },
    { path: "engines", value: { node: ">=18" }, message: "package metadata must require Node >=24" },
    { path: "os", value: ["win32"], message: "package metadata host constraints do not match release table" },
    { path: "cpu", value: ["arm64"], message: "package metadata host constraints do not match release table" },
    { path: "libc", value: ["musl"], message: "package metadata libc constraint does not match release table" },
    { path: "bamtiNative", value: undefined, message: "package metadata is missing bamtiNative" },
    { path: "bamtiNative.entry", value: "wrong.node", message: "package metadata entry does not match release table" },
    { path: "bamtiNative.target", value: "wrong", message: "package metadata target does not match release table" },
    {
      path: "bamtiNative.artifactKind",
      value: "wrong",
      message: "package metadata artifactKind does not match release table",
    },
    {
      path: "bamtiNative.sha256",
      value: "1".repeat(64),
      message: "package metadata sha256 does not match release table",
    },
    {
      path: "bamtiNative.release",
      value: makeRelease({ nativeAbi: 9 }),
      message: "package metadata release tuple does not match release table",
    },
  ];
  for (const { path, value, message } of cases) {
    const fixture = createFixture();
    setPath(fixture.manifest, path, value);
    fixture.files.set(fixture.manifestPath, Buffer.from(JSON.stringify(fixture.manifest)));
    assertLoadError(
      () => loadNativeAddon(fixture.options),
      (cause) => cause.message === message,
      `stale ${path}`,
    );
  }
});

test("darwin manifest must not declare a libc constraint", () => {
  const fixture = createFixture("darwin-x64");
  fixture.manifest.libc = ["glibc"];
  fixture.files.set(fixture.manifestPath, Buffer.from(JSON.stringify(fixture.manifest)));
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /libc constraint/.test(cause.message),
  );
});

test("stale release table fields are rejected", () => {
  const cases = [
    { mutator: (t) => { t.version = 2; }, message: "generated native release table has an unsupported schema" },
    { mutator: (t) => { t.targets = []; }, message: "generated native release table must contain exactly five targets" },
    {
      mutator: (t) => { t.targets[0].selector = null; },
      message: "generated native release table contains an invalid or duplicate selector",
    },
    {
      mutator: (t) => { t.targets[0].target = "wrong"; },
      message: "generated native release table has wrong target for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].package = "wrong"; },
      message: "generated native release table has wrong package for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].entry = "wrong"; },
      message: "generated native release table has wrong entry for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].os = "wrong"; },
      message: "generated native release table has wrong os for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].cpu = "wrong"; },
      message: "generated native release table has wrong cpu for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].artifactKind = "wrong"; },
      message: "generated native release table has wrong artifactKind for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].libc = "wrong"; },
      message: "generated native release table has wrong libc for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].version = "wrong"; },
      message: "generated native release table has wrong version for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.targets[0].sha256 = "bad"; },
      message: "generated native release table has invalid sha256 for linux-x64-gnu",
    },
    {
      mutator: (t) => { t.release.packageVersion = ""; },
      message: "generated release table has an incomplete release tuple",
    },
    {
      mutator: (t) => { t.release.releaseId = "wrong"; },
      message: "generated release table has a non-canonical releaseId",
    },
  ];
  for (const { mutator, message } of cases) {
    const fixture = createFixture();
    mutator(fixture.table);
    assertLoadError(
      () => loadNativeAddon(fixture.options),
      (cause) => cause.message === message,
      message,
    );
  }
});

test("escaped entry in release table is rejected before path traversal checks", () => {
  const fixture = createFixture();
  fixture.table.targets[0].entry = "../escape.node";
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /wrong entry for linux-x64-gnu/.test(cause.message),
  );
});

test("relative package manifest path is rejected before entry resolution", () => {
  const fixture = createFixture();
  fixture.options.resolver.resolve = (spec) => spec;
  fixture.files.set(
    `${fixture.row.package}/package.json`,
    fixture.files.get(fixture.manifestPath),
  );
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /native package manifest path must be absolute/.test(cause.message),
  );
});

test("entry path escaping through a symlink is rejected", () => {
  const fixture = createFixture();
  fixture.options.realpath = (path) => {
    if (path === fixture.packageRoot) return "/pkg";
    if (path === fixture.addonPath) return "/outside/bamti.node";
    return path;
  };
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /through a symlink/.test(cause.message),
  );
});

test("SHA mismatch is detected before requireAddon is called", () => {
  const fixture = createFixture();
  fixture.files.set(fixture.addonPath, Buffer.from("wrong bytes"));
  let required = false;
  fixture.options.requireAddon = (path) => {
    required = true;
    throw new Error("requireAddon should not be called with mismatched SHA");
  };
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /SHA-256 does not match/.test(cause.message),
  );
  assert.equal(required, false);
});

test("requireAddon error cause is preserved", () => {
  const fixture = createFixture();
  const cause = new Error("dlopen failed");
  fixture.options.requireAddon = () => {
    throw cause;
  };
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) => err instanceof NativeArtifactLoadError && err.cause === cause,
  );
});

test("addon must export releaseMetadata()", () => {
  const fixture = createFixture();
  fixture.options.requireAddon = () => ({ run() {} });
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /must export releaseMetadata/.test(cause.message),
  );
});

test("addon must export run()", () => {
  const fixture = createFixture();
  fixture.options.requireAddon = () => ({ releaseMetadata() {} });
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /must export run/.test(cause.message),
  );
});

test("addon rejects exports outside the closed native API", () => {
  const fixture = createFixture();
  fixture.options.requireAddon = () => ({
    ...makeAddon(fixture.row, fixture.release),
    extra() {},
  });
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /must export exactly releaseMetadata and run/.test(cause.message),
  );
});

test("addon releaseMetadata() must return an object", () => {
  const fixture = createFixture();
  fixture.options.requireAddon = () => ({
    releaseMetadata() { return 1; },
    run() {},
  });
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /returned a non-object/.test(cause.message),
  );
});

test("addon release metadata fields are validated", () => {
  const fixture = createFixture();
  const base = { ...fixture.release, target: fixture.row.target, artifactKind: fixture.row.artifactKind };
  const variants = [
    { ...base, releaseId: "wrong" },
    { ...base, target: "wrong" },
    { ...base, artifactKind: "wrong" },
    { ...base, nativeAbi: 99 },
    { ...base, cliProtocol: 99 },
  ];
  for (const variant of variants) {
    const perFixture = createFixture();
    perFixture.options.requireAddon = () => ({
      releaseMetadata() { return variant; },
      run() {},
    });
    assertLoadError(
      () => loadNativeAddon(perFixture.options),
      (cause) => /release metadata does not match/.test(cause.message),
      `variant ${JSON.stringify(variant)}`,
    );
  }
});

test("host Node version below 24 is rejected", () => {
  const fixture = createFixture();
  fixture.options.host.nodeVersion = "23.9.9";
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /Node >=24 is required/.test(cause.message),
  );
});

test("host Node-API version below 10 is rejected", () => {
  const fixture = createFixture();
  fixture.options.host.napiVersion = "9";
  assertLoadError(
    () => loadNativeAddon(fixture.options),
    (cause) => /Node-API 10 or later is required/.test(cause.message),
  );
});

test("successful load returns the addon and caches it", () => {
  const fixture = createFixture();
  const first = loadNativeAddon(fixture.options);
  assert.equal(first, fixture.addon);
  const second = loadNativeAddon(fixture.options);
  assert.equal(second, first);
  assert.deepEqual(fixture.calls, [
    ["resolve", `${fixture.row.package}/package.json`],
    ["readFile", fixture.manifestPath],
    ["realpath", fixture.packageRoot],
    ["realpath", fixture.addonPath],
    ["readFile", fixture.addonPath],
    ["stage", fixture.addonBytes],
    ["requireAddon", fixture.stagedPath],
  ]);
});

test("staged bytes are isolated from package path changes after read", () => {
  const fixture = createFixture();
  const tampered = Buffer.from("tampered bytes");
  const regressionStagedPath = `/private/staged-${fixture.row.package.replace(/\//g, "-")}.node`;
  fixture.options.stage = (bytes) => {
    fixture.files.set(fixture.addonPath, tampered);
    assert.equal(bytes, fixture.addonBytes);
    fixture.files.set(regressionStagedPath, bytes);
    return regressionStagedPath;
  };
  fixture.options.requireAddon = (path) => {
    assert.equal(path, regressionStagedPath);
    assert.equal(fixture.files.get(path), fixture.addonBytes);
    return fixture.addon;
  };
  const addon = loadNativeAddon(fixture.options);
  assert.equal(addon, fixture.addon);
});

test("loader does not fall back to spawn, download, or build; it only resolves, reads, and requires", () => {
  const fixture = createFixture();
  const notFound = moduleNotFound();
  fixture.options.resolver.resolve = (spec) => {
    fixture.calls.push(["resolve", spec]);
    throw notFound;
  };
  fixture.options.readFile = () => { throw new Error("readFile should not be called"); };
  fixture.options.realpath = () => { throw new Error("realpath should not be called"); };
  fixture.options.requireAddon = () => { throw new Error("requireAddon should not be called"); };
  assert.throws(
    () => loadNativeAddon(fixture.options),
    (err) => err instanceof NativeArtifactNotFoundError,
  );
  assert.deepEqual(fixture.calls, [["resolve", `${fixture.row.package}/package.json`]]);
});

test("production zero-arg load resolves the generated table and stays within the loader error surface", () => {
  assert.throws(
    () => loadNativeAddon(),
    (err) =>
      err instanceof UnsupportedPlatformError ||
      err instanceof NativeArtifactLoadError ||
      err instanceof NativeArtifactNotFoundError,
  );
});
