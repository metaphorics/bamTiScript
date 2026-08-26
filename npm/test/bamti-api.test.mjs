import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { access, mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

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

test("clean consumer imports all thirteen exports and runs the real API and CLI", async () => {
  const artifacts = await Promise.all(CLI_ARTIFACTS.map(artifactRecord));
  const hostCli = artifacts.find(
    ({ platform, arch }) => platform === process.platform && arch === process.arch,
  );
  assert.ok(hostCli, `no staged CLI target supports test host ${process.platform}-${process.arch}`);

  await withTemporaryRoot("bamti-clean-install-", async (root) => {
    const bamti = await packPackage(root, join(npmRoot, "bamti"));
    const bamtiCli = await packPackage(root, join(npmRoot, "bamti-cli"));

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
        `import { sep } from "node:path";\n` +
        `const subpaths = ${JSON.stringify(PUBLIC_EXPORTS.map((entry) => entry === "." ? "bamti" : `bamti/${entry.slice(2)}`))};\n` +
        `const loaded = await Promise.all(subpaths.map((specifier) => specifier.endsWith("/package.json") ? import(specifier, { with: { type: "json" } }) : import(specifier)));\n` +
        `for (let index = 0; index < loaded.length; index += 1) {\n` +
        `  assert.ok(Object.keys(loaded[index]).length > 0, subpaths[index] + " has no public exports");\n` +
        `}\n` +
        `const api = loaded[0];\n` +
        `assert.equal(api.artifactPackage(), ${JSON.stringify(hostCli.manifest.name)});\n` +
        `assert.ok(api.resolveBinary().endsWith(["", "bin", ${JSON.stringify(hostCli.entry.slice(4))}].join(sep)));\n` +
        `assert.equal(await api.run(["--version"], { stdio: "pipe" }), 0);\n` +
        `const session = api.createSession({ filesystem: loaded[3].osFileSystem(process.cwd()) });\n` +
        `assert.equal(typeof await session.snapshot(), "object");\n` +
        `await assert.rejects(session.snapshot({}, { signal: AbortSignal.abort() }), { name: "AbortError" });\n` +
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
    assert.match(cliResult.stdout, /^bamts\s+\S+/m);
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

  await withTemporaryRoot("bamti-rejection-", async (root) => {
    const bamti = await packPackage(root, join(npmRoot, "bamti"));
    const bamtiCli = await packPackage(root, join(npmRoot, "bamti-cli"));
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
