import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { access, mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { constants, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import { registerHooks } from "node:module";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const npmRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(npmRoot, "..");
const stagedArtifactRoot = resolve(process.env.BAMTI_STAGED_ARTIFACTS ?? join(npmRoot, "dist"));
const npmCommand = process.platform === "win32" ? "npm.cmd" : "npm";

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

const CLI_ARTIFACTS = [
  { directory: "cli-linux-x64", package: "@bamti/cli-linux-x64", target: "x86_64-unknown-linux-gnu", platform: "linux", arch: "x64", entry: "bin/bamts" },
  { directory: "cli-linux-arm64", package: "@bamti/cli-linux-arm64", target: "aarch64-unknown-linux-gnu", platform: "linux", arch: "arm64", entry: "bin/bamts" },
  { directory: "cli-darwin-x64", package: "@bamti/cli-darwin-x64", target: "x86_64-apple-darwin", platform: "darwin", arch: "x64", entry: "bin/bamts" },
  { directory: "cli-darwin-arm64", package: "@bamti/cli-darwin-arm64", target: "aarch64-apple-darwin", platform: "darwin", arch: "arm64", entry: "bin/bamts" },
  { directory: "cli-win32-x64", package: "@bamti/cli-win32-x64", target: "x86_64-pc-windows-msvc", platform: "win32", arch: "x64", entry: "bin/bamts.exe" },
];

const bamtiDirectory = join(npmRoot, "bamti");
const bamtiIndexUrl = pathToFileURL(join(bamtiDirectory, "index.js")).href;

const FAKE_NATIVE_LOADER_SOURCE = [
  "export const state = { loads: [], requests: [], outcomes: [], failures: [] };",
  "globalThis.__BAMTI_NATIVE_TEST_STATE__ = state;",
  "export const NATIVE_TARGETS = [];",
  "export class NativeArtifactLoadError extends Error {}",
  "export class NativeArtifactNotFoundError extends Error {}",
  "export class UnsupportedPlatformError extends Error {}",
  "export function selectNativeTarget() { return undefined; }",
  "const addon = {",
  "  releaseMetadata() {",
  "    return {",
  '      packageVersion: "0.2.0",',
  '      sourceCommit: "f".repeat(64),',
  '      buildSetId: "a".repeat(64),',
  '      releaseId: "bamti/0.2.0/" + "f".repeat(64) + "/native-abi-1/cli-protocol-1/" + "a".repeat(64),',
  '      target: "x86_64-unknown-linux-gnu",',
  '      artifactKind: "native-addon",',
  "      nativeAbi: 1,",
  "      cliProtocol: 1,",
  "    };",
  "  },",
  "  run(request) {",
  "    state.requests.push(request);",
  "    const failure = state.failures.shift();",
  "    if (failure !== undefined) return Promise.reject(failure);",
  "    return Promise.resolve(state.outcomes.shift());",
  "  },",
  "};",
  "export function loadNativeAddon(...options) {",
  "  state.loads.push(options);",
  "  return addon;",
  "}",
].join("\n");

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === "bamti-cli") {
      return {
        shortCircuit: true,
        url: pathToFileURL(join(npmRoot, "bamti-cli", "index.js")).href,
      };
    }
    if (
      specifier === "./native-loader.js" &&
      typeof context.parentURL === "string" &&
      context.parentURL.startsWith(`${bamtiIndexUrl}?wiring=`)
    ) {
      return {
        shortCircuit: true,
        url: `data:text/javascript,${encodeURIComponent(FAKE_NATIVE_LOADER_SOURCE)}`,
      };
    }
    return nextResolve(specifier, context);
  },
});

const api = await import("../bamti/index.js");
const wired = await import(`${bamtiIndexUrl}?wiring=1`);
const wiredState = globalThis.__BAMTI_NATIVE_TEST_STATE__;

function nativeRequest(overrides = {}) {
  return {
    args: [Buffer.from("--version"), Buffer.from("ünïcode.ts")],
    cwd: Buffer.from("/tmp/bamti-native"),
    env: [Buffer.from("BAMTS_COLOR=0"), Buffer.from("LANG=C.UTF-8")],
    ...overrides,
  };
}

function nativeOutcome() {
  return {
    exitCode: 3,
    stdout: Buffer.from([0x00, 0x8f, 0xff]),
    stderr: Buffer.from("compiler stderr\n"),
    truncation: { elided: 12, limit: 4096 },
  };
}

function resetWiredState() {
  Object.assign(wiredState, { loads: [], requests: [], outcomes: [], failures: [] });
}

function run(command, args, options = {}) {
  return spawnSync(command, args, {
    encoding: "utf8",
    ...options,
    env: {
      ...process.env,
      NODE_PATH: "",
      ...options.env,
    },
  });
}

function output(result) {
  return `${result.stdout ?? ""}${result.stderr ?? ""}`;
}

function assertSucceeded(result, action) {
  assert.equal(result.error, undefined, `${action}: ${result.error?.message}`);
  assert.equal(result.status, 0, `${action} failed (${result.status}):\n${output(result)}`);
}

async function withTemporaryRoot(prefix, callback) {
  const root = await mkdtemp(join(tmpdir(), prefix));
  const fromRepository = relative(repositoryRoot, root);
  assert.ok(
    fromRepository.startsWith(`..${sep}`) || fromRepository === "..",
    `hermetic root must be outside the repository: ${root}`,
  );
  try {
    return await callback(root);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

async function artifactRecord(target) {
  const metadataPath = join(npmRoot, "artifacts", target.directory, "package.json");
  const manifest = await readJson(metadataPath);
  const facade = await readJson(join(npmRoot, "bamti-cli", "package.json"));
  assert.equal(manifest.name, target.package, `${target.directory} must use its canonical package name`);
  assert.equal(manifest.version, facade.version, `${manifest.name} must match bamti-cli's version`);
  assert.deepEqual(manifest.os, [target.platform], `${manifest.name} must declare one exact OS`);
  assert.deepEqual(manifest.cpu, [target.arch], `${manifest.name} must declare one exact CPU`);
  assert.deepEqual(
    manifest.files,
    ["README.md", target.entry],
    `${manifest.name} must publish its real payload`,
  );
  assert.equal(manifest.bin, undefined, `${manifest.name} must not shadow bamti-cli's bamts bin`);
  assert.equal(manifest.bamtiCli?.entry, target.entry, `${manifest.name} must name its binary payload`);
  assert.equal(manifest.bamtiCli?.target, target.target, `${manifest.name} must name its Rust target`);
  assert.equal(manifest.bamtiCli?.artifactKind, "cli-binary");

  const tarball = `${manifest.name.replace(/^@/, "").replaceAll("/", "-")}-${manifest.version}.tgz`;
  const archive = join(stagedArtifactRoot, tarball);
  try {
    await access(archive, constants.R_OK);
  } catch (cause) {
    throw new Error(
      `missing staged artifact ${archive}; run the five-target artifact build before F2.4`,
      { cause },
    );
  }
  return { ...target, manifest, archive };
}

async function assembledFacade() {
  const manifest = await readJson(join(npmRoot, "bamti-cli", "package.json"));
  const archive = join(stagedArtifactRoot, `bamti-cli-${manifest.version}.tgz`);
  try {
    await access(archive, constants.R_OK);
  } catch (cause) {
    throw new Error(
      `missing assembled facade ${archive}; run node npm/scripts/stage-cli-artifacts.mjs to assemble it`,
      { cause },
    );
  }
  return { manifest, archive };
}

function npmEnvironment(root) {
  return {
    HOME: root,
    npm_config_cache: join(root, ".npm-cache"),
    npm_config_audit: "false",
    npm_config_fund: "false",
    npm_config_ignore_scripts: "true",
    npm_config_offline: "true",
    npm_config_package_lock: "false",
    npm_config_update_notifier: "false",
  };
}

async function installArchives(root, archives, { force = false } = {}) {
  const dependencies = Object.fromEntries(
    archives.map(({ manifest, archive }) => [manifest.name, `file:${archive}`]),
  );
  await writeFile(
    join(root, "package.json"),
    `${JSON.stringify({ private: true, type: "module", dependencies }, null, 2)}\n`,
  );
  const args = ["install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund", "--omit=optional"];
  if (force) args.push("--force");
  return run(npmCommand, args, { cwd: root, env: npmEnvironment(root) });
}

async function packPackage(root, packageDirectory) {
  const destination = join(root, "packed");
  await mkdir(destination, { recursive: true });
  const result = run(
    npmCommand,
    ["pack", "--ignore-scripts", "--json", "--pack-destination", destination],
    { cwd: packageDirectory, env: npmEnvironment(root) },
  );
  assertSucceeded(result, `pack ${packageDirectory}`);
  const report = JSON.parse(result.stdout);
  assert.equal(report.length, 1, `npm pack returned an unexpected report for ${packageDirectory}`);
  const archive = join(destination, report[0].filename);
  const manifest = await readJson(join(packageDirectory, "package.json"));
  await access(archive, constants.R_OK);
  return { manifest, archive };
}

async function assertInstalledArtifact(root, artifact) {
  const packageRoot = join(root, "node_modules", artifact.manifest.name);
  const installedManifest = await readJson(join(packageRoot, "package.json"));
  assert.equal(installedManifest.name, artifact.manifest.name);
  assert.equal(installedManifest.version, artifact.manifest.version);
  assert.deepEqual(installedManifest.os, [artifact.platform]);
  assert.deepEqual(installedManifest.cpu, [artifact.arch]);

  const payload = join(packageRoot, artifact.entry);
  const payloadStat = await stat(payload);
  assert.ok(payloadStat.isFile(), `${artifact.manifest.name} payload is not a file`);
  assert.ok(payloadStat.size > 0, `${artifact.manifest.name} payload is empty`);
  if (artifact.entry.startsWith("bin/") && artifact.platform !== "win32") {
    assert.notEqual(payloadStat.mode & 0o111, 0, `${artifact.manifest.name} binary is not executable`);
  }
}

for (const target of CLI_ARTIFACTS) {
  test(`staged ${target.directory} artifact installs only on its declared target`, async () => {
    const artifact = await artifactRecord(target);

    await withTemporaryRoot("bamti-artifact-unpack-", async (root) => {
      const unpack = await installArchives(root, [artifact], { force: true });
      assertSucceeded(unpack, `unpack ${artifact.manifest.name}`);
      await assertInstalledArtifact(root, artifact);
    });

    await withTemporaryRoot("bamti-artifact-platform-", async (root) => {
      const install = await installArchives(root, [artifact]);
      const matchesHost = target.platform === process.platform && target.arch === process.arch;
      if (matchesHost) {
        assertSucceeded(install, `install host artifact ${artifact.manifest.name}`);
      } else {
        assert.notEqual(install.status, 0, `${artifact.manifest.name} unexpectedly installed on ${process.platform}-${process.arch}`);
        assert.match(output(install), /EBADPLATFORM|Unsupported platform/i);
      }
    });
  });
}

test("bamti publishes exactly the thirteen entry points with truthful names", async () => {
  const manifest = await readJson(join(bamtiDirectory, "package.json"));
  assert.deepEqual(Object.keys(manifest.exports).sort(), [...PUBLIC_EXPORTS].sort());
  assert.ok(JSON.stringify(manifest).includes("unstable/sync"));
  for (const [key, entry] of Object.entries(manifest.exports)) {
    if (key === "./package.json") continue;
    assert.deepEqual(Object.keys(entry).sort(), ["import", "types"], `${key} must declare types and import`);
    await access(join(bamtiDirectory, entry.types));
    await access(join(bamtiDirectory, entry.import));
  }
  assert.equal(manifest.engines.node, ">=24");
  assert.deepEqual(manifest.bin, { tsc: "native-runner.js" });
  assert.ok(manifest.files.includes("native-release-table.json"), "packed table must ship for fail-closed loading");
  assert.deepEqual(manifest.optionalDependencies, {
    "@bamti/bamti-linux-x64-gnu": manifest.version,
    "@bamti/bamti-linux-arm64-gnu": manifest.version,
    "@bamti/bamti-darwin-x64": manifest.version,
    "@bamti/bamti-darwin-arm64": manifest.version,
    "@bamti/bamti-win32-x64-msvc": manifest.version,
  });
});

test("root exposes the typed in-process native operation and loader surface", async () => {
  assert.equal(typeof api.runNative, "function");
  assert.equal(typeof api.loadNativeAddon, "function");
  assert.equal(typeof api.selectNativeTarget, "function");
  assert.equal(api.NATIVE_TARGETS.length, 5);
  for (const errorName of ["UnsupportedPlatformError", "NativeArtifactNotFoundError", "NativeArtifactLoadError"]) {
    assert.equal(typeof api[errorName], "function", errorName);
    assert.ok(
      Object.prototype.isPrototypeOf.call(Error.prototype, api[errorName].prototype),
      `${errorName} must be an Error subclass`,
    );
  }
  const declarations = await readFile(join(bamtiDirectory, "index.d.ts"), "utf8");
  assert.match(declarations, /export declare function runNative\(\s*request: NativeRunRequest,\s*\): Promise<NativeRunOutcome>;/);
  assert.match(declarations, /export interface NativeRunRequest \{[\s\S]*?readonly signal\?: AbortSignal;/);
  assert.ok(!declarations.includes("createSyncService"), "false sync name must not survive in types");
  const readme = await readFile(join(bamtiDirectory, "README.md"), "utf8");
  assert.match(readme, /runNative\(/);
  assert.match(readme, /createSerialService/);
  assert.ok(!readme.includes("createSyncService"), "false sync name must not survive in docs");
  assert.equal(existsSync(join(bamtiDirectory, "unstable", "sync.js")), true);
  assert.equal(existsSync(join(bamtiDirectory, "unstable", "sync.d.ts")), true);
  const serial = await import("../bamti/unstable/sync.js");
  assert.equal(typeof serial.createSerialService, "function");
  assert.equal(serial.createSyncService, undefined);
});

test("runNative forwards request bytes to the addon and returns the outcome unchanged", async () => {
  resetWiredState();
  const request = nativeRequest({ signal: new AbortController().signal });
  const outcome = nativeOutcome();
  wiredState.outcomes.push(outcome);
  const returned = await wired.runNative(request);
  assert.deepEqual(wiredState.loads, [[]]);
  assert.equal(wiredState.loads.length, 1, "each invocation loads through the verified loader once");
  assert.equal(wiredState.requests[0], request);
  assert.equal(returned, outcome);
  assert.ok(Buffer.isBuffer(request.args[0]) && Buffer.isBuffer(request.cwd) && Buffer.isBuffer(request.env[0]));
  assert.ok(Buffer.isBuffer(returned.stdout) && Buffer.isBuffer(returned.stderr));
});

test("runNative surfaces addon queue and closing failures explicitly", async () => {
  resetWiredState();
  const queueError = new Error("bamti native invocation queue is full");
  wiredState.failures.push(queueError);
  await assert.rejects(() => wired.runNative(nativeRequest()), (error) => error === queueError);
  const closingError = new Error("bamti native executor is closing");
  wiredState.failures.push(closingError);
  await assert.rejects(() => wired.runNative(nativeRequest()), (error) => error === closingError);
  assert.equal(wiredState.outcomes.length, 0);
  assert.equal(wiredState.requests.length, 2);
});

test("selectNativeTarget maps exactly the five published native targets", () => {
  const supported = [
    { platform: "linux", arch: "x64", libc: "glibc", selector: "linux-x64-gnu" },
    { platform: "linux", arch: "arm64", libc: "glibc", selector: "linux-arm64-gnu" },
    { platform: "darwin", arch: "x64", selector: "darwin-x64" },
    { platform: "darwin", arch: "arm64", selector: "darwin-arm64" },
    { platform: "win32", arch: "x64", selector: "win32-x64-msvc" },
  ];
  assert.deepEqual(
    api.NATIVE_TARGETS.map(({ selector }) => selector),
    supported.map(({ selector }) => selector),
  );
  for (const host of supported) {
    const target = api.selectNativeTarget(host);
    assert.equal(target.selector, host.selector);
    assert.equal(target.package, `@bamti/bamti-${host.selector}`);
    assert.equal(target.entry, `bamti.${host.selector}.node`);
    assert.equal(target.artifactKind, "native-addon");
    assert.ok(existsSync(join(npmRoot, "artifacts", `bamti-${host.selector}`)), `${host.selector} has a published artifact package`);
  }
  assert.equal(api.selectNativeTarget({ platform: "darwin", arch: "x64", libc: "musl" }).selector, "darwin-x64");
  for (const host of [
    { platform: "linux", arch: "x64", libc: "musl" },
    { platform: "linux", arch: "x64", libc: "unknown" },
    { platform: "linux", arch: "x64" },
    { platform: "sunos", arch: "x64" },
    { platform: "win32", arch: "arm64" },
  ]) {
    assert.throws(() => api.selectNativeTarget(host), api.UnsupportedPlatformError);
  }
});

test("in-process loading fails closed without a staged release table", async () => {
  assert.equal(existsSync(join(bamtiDirectory, "native-release-table.json")), false);
  const hostLibc =
    process.platform === "linux"
      ? process.report?.getReport?.()?.header?.glibcVersionRuntime
        ? "glibc"
        : "unknown"
      : undefined;
  const hostSupported = (() => {
    try {
      api.selectNativeTarget({ platform: process.platform, arch: process.arch, libc: hostLibc });
      return true;
    } catch {
      return false;
    }
  })();
  await assert.rejects(
    () => api.runNative(nativeRequest()),
    (error) => error instanceof (hostSupported ? api.NativeArtifactLoadError : api.UnsupportedPlatformError),
  );
});

test("clean consumer imports all thirteen exports and runs the real API and CLI", async () => {
  const artifacts = await Promise.all(CLI_ARTIFACTS.map(artifactRecord));
  const hostCli = artifacts.find(
    ({ platform, arch }) => platform === process.platform && arch === process.arch,
  );
  assert.ok(hostCli, `no staged CLI target supports test host ${process.platform}-${process.arch}`);
  const bamtiCli = await assembledFacade();

  await withTemporaryRoot("bamti-clean-install-", async (root) => {
    const bamti = await packPackage(root, join(npmRoot, "bamti"));

    const emptyConsumer = join(root, "empty-consumer");
    await mkdir(emptyConsumer);
    const upwardProbe = run(
      process.execPath,
      ["--input-type=module", "--eval", "await import('bamti')"],
      { cwd: emptyConsumer, env: npmEnvironment(emptyConsumer) },
    );
    assert.notEqual(upwardProbe.status, 0, "bamti resolved without installation; upward node_modules leaked into the test");
    assert.match(output(upwardProbe), /ERR_MODULE_NOT_FOUND|Cannot find package/);

    assert.deepEqual(bamtiCli.manifest.bin, { bamts: "bin/bamts.js" });
    const install = await installArchives(root, [bamti, bamtiCli, hostCli]);
    assertSucceeded(install, "install clean bamti consumer");
    await assertInstalledArtifact(root, hostCli);

    const installedManifest = await readJson(join(root, "node_modules", "bamti", "package.json"));
    assert.deepEqual(
      Object.keys(installedManifest.exports).sort(),
      [...PUBLIC_EXPORTS].sort(),
      "bamti must publish exactly the thirteen stable entry points",
    );

    const exercise = join(root, "exercise.mjs");
    await writeFile(
      exercise,
      `import assert from "node:assert/strict";\n` +
        `import { isAbsolute, sep } from "node:path";\n` +
        `const subpaths = ${JSON.stringify(PUBLIC_EXPORTS.map((entry) => entry === "." ? "bamti" : `bamti/${entry.slice(2)}`))};\n` +
        `const loaded = await Promise.all(subpaths.map((specifier) => specifier.endsWith("/package.json") ? import(specifier, { with: { type: "json" } }) : import(specifier)));\n` +
        `for (let index = 0; index < loaded.length; index += 1) {\n` +
        `  assert.ok(Object.keys(loaded[index]).length > 0, subpaths[index] + " has no public exports");\n` +
        `}\n` +
        `const api = loaded[0];\n` +
        `assert.equal(api.artifactPackage(), ${JSON.stringify(hostCli.manifest.name)});\n` +
        `assert.ok(isAbsolute(api.resolveBinary()) && api.resolveBinary().endsWith(sep + ${JSON.stringify(hostCli.entry.split("/").at(-1))}));\n` +
        `assert.equal(await api.run(["--version"], { stdio: "pipe" }), 0);\n` +
        `const session = api.createSession({ filesystem: loaded[3].osFileSystem(process.cwd()) });\n` +
        `assert.equal(typeof await session.snapshot(), "object");\n` +
        `await assert.rejects(session.snapshot({}, { signal: AbortSignal.abort() }), { name: "AbortError" });\n` +
        `await assert.rejects(api.runNative({ args: [Buffer.from("--version")], cwd: Buffer.from(process.cwd()), env: [] }), (error) => error.name === "NativeArtifactLoadError" || error.name === "NativeArtifactNotFoundError");\n` +
        `await session.dispose();\n`,
    );
    const apiResult = run(process.execPath, [exercise], { cwd: root, env: npmEnvironment(root) });
    assertSucceeded(apiResult, "exercise installed bamti API");

    const cliEntry = join(root, "node_modules", "bamti-cli", "bin", "bamts.js");
    const cliResult = run(process.execPath, [cliEntry, "--version"], {
      cwd: root,
      env: npmEnvironment(root),
    });
    assertSucceeded(cliResult, "exercise installed bamts CLI");
    assert.match(cliResult.stdout, /^Version \d+\.\d+\.\d+$/m);
  });
});

test("clean consumer rejects absent and wrong-platform artifacts", async () => {
  const artifacts = await Promise.all(CLI_ARTIFACTS.map(artifactRecord));
  const hostArtifact = artifacts.find(
    ({ platform, arch }) => platform === process.platform && arch === process.arch,
  );
  assert.ok(hostArtifact, `no staged artifact target supports test host ${process.platform}-${process.arch}`);
  const wrongArtifact = artifacts.find(({ manifest }) => manifest.name !== hostArtifact.manifest.name);
  assert.ok(wrongArtifact);
  const bamtiCli = await assembledFacade();

  await withTemporaryRoot("bamti-rejection-", async (root) => {
    const bamti = await packPackage(root, join(npmRoot, "bamti"));
    const install = await installArchives(root, [bamti, bamtiCli, wrongArtifact], { force: true });
    assertSucceeded(install, "install consumer with only wrong-platform artifact");

    const rejection = run(
      process.execPath,
      [
        "--input-type=module",
        "--eval",
        `const api = await import("bamti"); try { api.resolveBinary(); process.exit(9); } catch (error) { if (error.name !== "ArtifactNotFoundError" || !error.message.includes(${JSON.stringify(hostArtifact.manifest.name)})) throw error; }`,
      ],
      { cwd: root, env: npmEnvironment(root) },
    );
    assertSucceeded(rejection, "reject absent host artifact");
  });
});
