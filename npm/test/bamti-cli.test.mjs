import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, rm, writeFile, chmod, symlink } from "node:fs/promises";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, delimiter } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

import {
  ARTIFACTS,
  ArtifactLoadError,
  ArtifactNotFoundError,
  CLI_TARGETS,
  UnsupportedPlatformError,
  artifactPackage,
  resolveBinary,
  run,
} from "../bamti-cli/index.js";
import { NATIVE_TARGETS } from "../bamti/native-loader.js";
import {
  assertRegistryAvailability,
  preflight,
  preflightCliRelease,
  preflightRelease,
  tarballName,
} from "../scripts/preflight.mjs";
import { createNativeBuildPlan, NATIVE_RELEASE_TARGETS } from "../scripts/native-build-plan.mjs";

const availableVersion = (_packageName, version) => version;

const packagingScript = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "scripts",
  "package-platform.mjs",
);

async function withTemporaryDirectory(callback) {
  const directory = await mkdtemp(join(tmpdir(), "bamti-cli-test-"));
  try {
    await callback(directory);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

async function createTarball(distRoot, packageName, version, binaryName) {
  const [scope, unscoped] = packageName.split("/");
  const packageDir = join(distRoot, "package");
  const binDir = join(packageDir, "bin");
  await mkdir(binDir, { recursive: true });
  const pkg = {
    name: packageName,
    version,
    description: `test ${unscoped}`,
    files: ["README.md"],
  };
  if (binaryName !== null) {
    pkg.files.push(`bin/${binaryName}`);
  }
  await writeFile(join(packageDir, "package.json"), JSON.stringify(pkg));
  if (binaryName !== null) {
    await writeFile(join(binDir, binaryName), "binary\n");
  }
  const tarball = tarballName(packageName, version);
  const result = spawnSync("tar", ["-czf", tarball, "package"], {
    cwd: distRoot,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, `tar failed: ${result.stderr}`);
  await rm(packageDir, { recursive: true, force: true });
  return tarball;
}

async function createAllTarballs(distRoot, version, mutator) {
  for (const [key, packageName] of ARTIFACTS) {
    const platform = key.slice(0, key.lastIndexOf("-"));
    const binaryName = platform === "win32" ? "bamts.exe" : "bamts";
    await createTarball(distRoot, packageName, version, binaryName);
  }
  if (mutator) {
    await mutator(distRoot);
  }
}

function hashBuffer(buffer) {
  return createHash("sha256").update(buffer).digest("hex");
}

function canonicalReleaseId(release) {
  return `bamti/${release.packageVersion}/${release.sourceCommit}/native-abi-${release.nativeAbi}/cli-protocol-${release.cliProtocol}/${release.buildSetId}`;
}

function makeRelease(version, sourceCommit, buildSetId) {
  const release = {
    packageVersion: version,
    sourceCommit,
    nativeAbi: 1,
    cliProtocol: 1,
    buildSetId,
    releaseId: "",
  };
  release.releaseId = canonicalReleaseId(release);
  return release;
}

function nodeBytesFor(row) {
  return Buffer.from(`${row.package} node\n`, "utf8");
}

async function createNativeTarball(distRoot, row, version, nodeBytes, release) {
  const packageDir = join(distRoot, "package");
  await mkdir(packageDir, { recursive: true });
  const sha256 = hashBuffer(nodeBytes);
  const pkg = {
    name: row.package,
    version,
    type: "commonjs",
    main: row.entry,
    files: [row.entry],
    engines: { node: ">=24" },
    os: [row.os],
    cpu: [row.cpu],
    ...(row.libc === undefined ? {} : { libc: [row.libc] }),
    bamtiNative: {
      entry: row.entry,
      target: row.target,
      artifactKind: row.artifactKind,
      sha256,
      release: { ...release },
    },
  };
  await writeFile(join(packageDir, row.entry), nodeBytes);
  await writeFile(join(packageDir, "package.json"), JSON.stringify(pkg));
  const tarball = tarballName(row.package, version);
  const result = spawnSync("tar", ["-czf", tarball, "package"], {
    cwd: distRoot,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, `tar failed: ${result.stderr}`);
  await rm(packageDir, { recursive: true, force: true });
  return { tarball, sha256 };
}

async function createBamtiReleaseTarball(
  distRoot,
  version,
  nativeRows,
  release,
  { includeTable = true } = {},
) {
  const packageDir = join(distRoot, "package");
  await mkdir(packageDir, { recursive: true });
  const pkg = {
    name: "bamti",
    version,
    type: "module",
    files: [
      "README.md",
      "index.d.ts",
      "index.js",
      "native-loader.js",
      "native-release-table.json",
      "native-runner.js",
    ],
    optionalDependencies: Object.fromEntries(
      nativeRows.map((row) => [row.package, version]),
    ),
  };
  await writeFile(join(packageDir, "package.json"), JSON.stringify(pkg));
  await writeFile(join(packageDir, "README.md"), "bamti\n");
  await writeFile(
    join(packageDir, "index.d.ts"),
    "export function run(args: string[]): Promise<number>;\n",
  );
  await writeFile(
    join(packageDir, "index.js"),
    "export { run } from './native-runner.js';\n",
  );
  await writeFile(
    join(packageDir, "native-runner.js"),
    "import { loadNativeAddon } from './native-loader.js';\n",
  );
  await writeFile(
    join(packageDir, "native-loader.js"),
    "export { loadNativeAddon } from './native-loader.js';\n",
  );
  if (includeTable) {
    const table = {
      version: 1,
      release,
      targets: nativeRows.map((row) => ({
        selector: row.selector,
        target: row.target,
        package: row.package,
        entry: row.entry,
        os: row.os,
        cpu: row.cpu,
        ...(row.libc === undefined ? {} : { libc: row.libc }),
        artifactKind: row.artifactKind,
        version: row.version,
        sha256: row.sha256,
      })),
    };
    await writeFile(
      join(packageDir, "native-release-table.json"),
      JSON.stringify(table),
    );
  }
  const tarball = tarballName("bamti", version);
  const result = spawnSync("tar", ["-czf", tarball, "package"], {
    cwd: distRoot,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, `tar failed: ${result.stderr}`);
  await rm(packageDir, { recursive: true, force: true });
  return tarball;
}

async function createCliFacadeTarball(
  distRoot,
  version,
  { release, cliRows } = {},
) {
  const packageDir = join(distRoot, "package");
  const binDir = join(packageDir, "bin");
  await mkdir(binDir, { recursive: true });
  const pkg = {
    name: "bamti-cli",
    version,
    type: "module",
    bin: { bamts: "bin/bamts.js" },
    files: ["bin/bamts.js", "index.js", "index.d.ts", "README.md"],
    optionalDependencies: Object.fromEntries(
      [...ARTIFACTS.values()].map((packageName) => [packageName, version]),
    ),
  };
  if (cliRows !== undefined && release !== undefined) {
    pkg.files.push("cli-release-table.json");
    const table = makeCliTable(cliRows, release);
    await writeFile(
      join(packageDir, "cli-release-table.json"),
      JSON.stringify(table),
    );
  }
  await writeFile(join(packageDir, "package.json"), JSON.stringify(pkg));
  await writeFile(join(packageDir, "bin", "bamts.js"), "#!/usr/bin/env node\n");
  const tarball = tarballName("bamti-cli", version);
  const result = spawnSync("tar", ["-czf", tarball, "package"], {
    cwd: distRoot,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, `tar failed: ${result.stderr}`);
  await rm(packageDir, { recursive: true, force: true });
  return tarball;
}

async function createFullRelease(
  distRoot,
  version,
  { skipNative = new Set(), includeCliTable = true, includeNativeTable = true, mutator } = {},
) {
  await mkdir(distRoot, { recursive: true });
  const release = makeRelease(version, "a".repeat(40), "test-build");

  const cliRows = await createAllCliLeaves(distRoot, release);
  await createCliFacadeTarball(
    distRoot,
    version,
    includeCliTable ? { release, cliRows } : {},
  );

  const nativeRows = NATIVE_TARGETS.map((row) => {
    const bytes = nodeBytesFor(row);
    return { ...row, version, sha256: hashBuffer(bytes) };
  });

  for (const row of nativeRows) {
    if (skipNative.has(row.selector)) continue;
    await createNativeTarball(distRoot, row, version, nodeBytesFor(row), release);
  }

  await createBamtiReleaseTarball(distRoot, version, nativeRows, release, {
    includeTable: includeNativeTable,
  });

  if (mutator) {
    await mutator(distRoot, nativeRows, release, cliRows);
  }
}

test("ARTIFACTS, manifests, and optionalDependencies agree", () => {
  // Throws if any of the three call sites disagree.
  preflight();
});

// ---------------------------------------------------------------------------
// CLI loader fixture helpers
// ---------------------------------------------------------------------------

const CLI_VERSION = "0.2.0";
const CLI_SOURCE_COMMIT = "a".repeat(40);
const CLI_BUILD_SET_ID = "b".repeat(64);
const CLI_CARGO_LOCK_SHA256 = "c".repeat(64);
const CLI_WORKFLOW_REVISION = "cli-release-build-v1";
const CLI_TOOLCHAIN = "rustc 1.97.1";
const CLI_BUILDER_REVISION = "package-platform-v1";

function cliReleaseId(release) {
  return `bamti/${release.packageVersion}/${release.sourceCommit}/native-abi-${release.nativeAbi}/cli-protocol-${release.cliProtocol}/${release.buildSetId}`;
}

function makeCliRelease(version = CLI_VERSION) {
  const release = {
    packageVersion: version,
    sourceCommit: CLI_SOURCE_COMMIT,
    nativeAbi: 1,
    cliProtocol: 1,
    buildSetId: CLI_BUILD_SET_ID,
    releaseId: "",
  };
  release.releaseId = cliReleaseId(release);
  return release;
}

function makeCliFacadeManifest(version = CLI_VERSION) {
  return {
    name: "bamti-cli",
    version,
    engines: { node: ">=24" },
    optionalDependencies: Object.fromEntries(
      CLI_TARGETS.map(({ package: p }) => [p, version]),
    ),
  };
}

function makeCliTargetRow(fixed, bytes, release) {
  return {
    selector: fixed.selector,
    target: fixed.target,
    package: fixed.package,
    entry: fixed.entry,
    os: fixed.os,
    cpu: fixed.cpu,
    artifactKind: fixed.artifactKind,
    runner: fixed.runner,
    version: release.packageVersion,
    sha256: hashBuffer(bytes),
    cargoLockSha256: CLI_CARGO_LOCK_SHA256,
    workflowRevision: CLI_WORKFLOW_REVISION,
    toolchain: CLI_TOOLCHAIN,
    builderRevision: CLI_BUILDER_REVISION,
  };
}

function makeCliTable(rows, release) {
  return { version: 1, release, targets: rows };
}

function makeLeafManifest(row, release) {
  return {
    name: row.package,
    version: row.version,
    engines: { node: ">=24" },
    os: [row.os],
    cpu: [row.cpu],
    files: ["README.md", row.entry],
    bamtiCli: {
      entry: row.entry,
      target: row.target,
      artifactKind: row.artifactKind,
      sha256: row.sha256,
      cargoLockSha256: row.cargoLockSha256,
      workflowRevision: row.workflowRevision,
      toolchain: row.toolchain,
      builderRevision: row.builderRevision,
      runner: row.runner,
      release: { ...release },
    },
  };
}
async function createCliLeafTarball(distRoot, row, release, bytes) {
  const packageDir = join(distRoot, "package");
  const binDir = join(packageDir, "bin");
  await mkdir(binDir, { recursive: true });
  const binaryName = row.os === "win32" ? "bamts.exe" : "bamts";
  const binaryPath = join(binDir, binaryName);
  await writeFile(binaryPath, bytes);
  if (row.os !== "win32") {
    await chmod(binaryPath, 0o755);
  }
  const manifest = makeLeafManifest(row, release);
  await writeFile(join(packageDir, "package.json"), JSON.stringify(manifest));
  await writeFile(join(packageDir, "README.md"), "binary\n");
  const tarball = tarballName(row.package, row.version);
  const result = spawnSync("tar", ["-czf", tarball, "package"], {
    cwd: distRoot,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, `tar failed: ${result.stderr}`);
  await rm(packageDir, { recursive: true, force: true });
  return { row, tarball };
}

async function createAllCliLeaves(
  distRoot,
  release,
  { skip = new Set(), bytesFor = (fixed) => Buffer.from(`${fixed.package} binary\n`, "utf8") } = {},
) {
  const rows = [];
  for (const fixed of CLI_TARGETS) {
    if (skip.has(fixed.selector)) continue;
    const bytes = bytesFor(fixed);
    const row = makeCliTargetRow(fixed, bytes, release);
    await createCliLeafTarball(distRoot, row, release, bytes);
    rows.push(row);
  }
  return rows;
}

async function createCliRelease(
  distRoot,
  version,
  { skip = new Set(), includeTable = true } = {},
) {
  await mkdir(distRoot, { recursive: true });
  const release = makeRelease(version, "a".repeat(40), "test-build");
  const cliRows = await createAllCliLeaves(distRoot, release, { skip });
  await createCliFacadeTarball(
    distRoot,
    version,
    includeTable ? { release, cliRows } : {},
  );
  return { release, cliRows };
}

async function writeJsonFile(path, value) {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`);
}

/**
 * Creates a complete CLI fixture: a facade manifest, a release table with all
 * five targets, and a real node_modules tree for the requested platform with a
 * binary whose SHA-256 matches the table.
 *
 * Returns { facadeManifest, table, release, resolvePackage, binaryPath, bytes }.
 */
async function createCliFixture(directory, {
  platform = "linux",
  arch = "x64",
  binaryBytes = Buffer.from("#!/bin/sh\necho bamts\n", "utf8"),
  facadeOverride = {},
  tableOverride = undefined,
  leafOverride = undefined,
} = {}) {
  const fixed = CLI_TARGETS.find((t) => t.os === platform && t.cpu === arch);
  if (!fixed) throw new Error(`no CLI target for ${platform}-${arch}`);

  const release = makeCliRelease();
  const allRows = CLI_TARGETS.map((t) =>
    makeCliTargetRow(t, t === fixed ? binaryBytes : Buffer.from(`${t.package} placeholder\n`, "utf8"), release),
  );
  const table = tableOverride ?? makeCliTable(allRows, release);
  const facadeManifest = { ...makeCliFacadeManifest(), ...facadeOverride };

  // Build a real node_modules tree so createRequire.resolve works.
  const artifactDir = join(directory, "node_modules", ...fixed.package.split("/"));
  const binaryPath = join(artifactDir, fixed.entry);
  await mkdir(dirname(binaryPath), { recursive: true });
  let leafManifest = makeLeafManifest(
    table.targets.find((t) => t.os === platform && t.cpu === arch) ?? allRows[0],
    release,
  );
  if (leafOverride) leafManifest = leafOverride(leafManifest);
  await writeJsonFile(join(artifactDir, "package.json"), leafManifest);
  await writeFile(binaryPath, binaryBytes);
  if (platform !== "win32") await chmod(binaryPath, 0o755);

  const consumerRequire = createRequire(join(directory, "consumer.cjs"));

  return {
    facadeManifest,
    table,
    release,
    resolvePackage: consumerRequire.resolve,
    binaryPath,
    bytes: binaryBytes,
    fixed,
  };
}

// ---------------------------------------------------------------------------
// CLI loader tests
// ---------------------------------------------------------------------------

test("the default loader fails when no generated table exists", () => {
  // The source tree no longer ships cli-release-table.json; the default
  // loader must fail closed with ArtifactLoadError.
  assert.throws(
    () => resolveBinary({ platform: "linux", arch: "x64", nodeVersion: "24.0.0" }),
    (error) => error instanceof ArtifactLoadError,
  );
});

test("the CLI loader resolves an assembled valid fixture", async () => {
  await withTemporaryDirectory(async (directory) => {
    const fixture = await createCliFixture(directory);
    const stagedPath = resolveBinary({
      platform: "linux",
      arch: "x64",
      nodeVersion: "24.0.0",
      table: fixture.table,
      facadeManifest: fixture.facadeManifest,
      resolvePackage: fixture.resolvePackage,
    });
    assert.ok(stagedPath, "resolveBinary must return a path");
    assert.notEqual(stagedPath, fixture.binaryPath, "must return a staged path, not the source");
    assert.deepEqual(readFileSync(stagedPath), fixture.bytes);
  });
});

test("the CLI loader rejects an invalid facade manifest", async () => {
  await withTemporaryDirectory(async (directory) => {
    const fixture = await createCliFixture(directory, {
      facadeOverride: { name: "not-bamti-cli" },
    });
    assert.throws(
      () =>
        resolveBinary({
          platform: "linux",
          arch: "x64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: fixture.resolvePackage,
        }),
      (error) => error instanceof ArtifactLoadError,
    );
  });
});

test("the CLI loader rejects a facade with wrong optionalDependencies", async () => {
  await withTemporaryDirectory(async (directory) => {
    const fixture = await createCliFixture(directory, {
      facadeOverride: {
        optionalDependencies: { "@bamti/cli-linux-x64": CLI_VERSION },
      },
    });
    assert.throws(
      () =>
        resolveBinary({
          platform: "linux",
          arch: "x64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: fixture.resolvePackage,
        }),
      (error) => error instanceof ArtifactLoadError,
    );
  });
});

test("the CLI loader rejects a changed binary hash", async () => {
  await withTemporaryDirectory(async (directory) => {
    const original = Buffer.from("#!/bin/sh\necho bamts\n", "utf8");
    const tampered = Buffer.from("#!/bin/sh\necho evil\n", "utf8");
    const fixture = await createCliFixture(directory, {
      binaryBytes: original,
      tableOverride: (() => {
        const release = makeCliRelease();
        const rows = CLI_TARGETS.map((t) =>
          makeCliTargetRow(t, t.os === "linux" && t.cpu === "x64" ? tampered : original, release),
        );
        return makeCliTable(rows, release);
      })(),
    });
    assert.throws(
      () =>
        resolveBinary({
          platform: "linux",
          arch: "x64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: fixture.resolvePackage,
        }),
      (error) =>
        error instanceof ArtifactLoadError &&
        error.cause?.message.includes("SHA-256"),
    );
  });
});

test("the CLI loader rejects a missing artifact package", async () => {
  await withTemporaryDirectory(async (directory) => {
    const fixture = await createCliFixture(directory);
    // Simulate a missing optional dependency with a resolver that always throws.
    const missingResolver = (_specifier) => {
      throw new Error("Cannot find module");
    };
    assert.throws(
      () =>
        resolveBinary({
          platform: "linux",
          arch: "x64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: missingResolver,
        }),
      (error) =>
        error instanceof ArtifactNotFoundError &&
        error.message.includes("@bamti/cli-linux-x64"),
    );
  });
});

test("the CLI loader rejects an unsupported host", async () => {
  await withTemporaryDirectory(async (directory) => {
    const fixture = await createCliFixture(directory);
    assert.throws(
      () =>
        resolveBinary({
          platform: "win32",
          arch: "arm64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: fixture.resolvePackage,
        }),
      (error) => error instanceof UnsupportedPlatformError,
    );
  });
});

test("the CLI loader rejects a symlink entry that escapes its package root", async () => {
  await withTemporaryDirectory(async (directory) => {
    const evilBinary = join(directory, "evil");
    await writeFile(evilBinary, "#!/bin/sh\necho evil\n");
    await chmod(evilBinary, 0o755);

    const fixture = await createCliFixture(directory, {
      leafOverride: (manifest) => {
        manifest.bamtiCli.entry = "bin/link";
        return manifest;
      },
      tableOverride: (() => {
        const release = makeCliRelease();
        const fixed = CLI_TARGETS.find((t) => t.os === "linux" && t.cpu === "x64");
        const bytes = Buffer.from("#!/bin/sh\necho bamts\n", "utf8");
        const rows = CLI_TARGETS.map((t) => {
          const row = makeCliTargetRow(t, t === fixed ? bytes : Buffer.from(`${t.package} placeholder\n`, "utf8"), release);
          if (t === fixed) row.entry = "bin/link";
          return row;
        });
        return makeCliTable(rows, release);
      })(),
    });

    // Replace the real binary with a symlink that escapes the package root.
    const linkPath = join(dirname(fixture.binaryPath), "link");
    await symlink(evilBinary, linkPath);

    assert.throws(
      () =>
        resolveBinary({
          platform: "linux",
          arch: "x64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: fixture.resolvePackage,
        }),
      (error) => error instanceof ArtifactLoadError,
    );
  });
});

test("the CLI loader rejects tampered leaf metadata", async () => {
  await withTemporaryDirectory(async (directory) => {
    const fixture = await createCliFixture(directory, {
      leafOverride: (manifest) => {
        manifest.bamtiCli.sha256 = "0".repeat(64);
        return manifest;
      },
    });
    assert.throws(
      () =>
        resolveBinary({
          platform: "linux",
          arch: "x64",
          nodeVersion: "24.0.0",
          table: fixture.table,
          facadeManifest: fixture.facadeManifest,
          resolvePackage: fixture.resolvePackage,
        }),
      (error) => error instanceof ArtifactLoadError,
    );
  });
});

test("staged bytes remain verified even if the source binary is replaced after read", async () => {
  await withTemporaryDirectory(async (directory) => {
    const original = Buffer.from("#!/bin/sh\necho bamts\n", "utf8");
    const fixture = await createCliFixture(directory, { binaryBytes: original });
    const stagedPath = resolveBinary({
      platform: "linux",
      arch: "x64",
      nodeVersion: "24.0.0",
      table: fixture.table,
      facadeManifest: fixture.facadeManifest,
      resolvePackage: fixture.resolvePackage,
    });
    // Tamper with the source binary after resolveBinary has returned.
    await writeFile(fixture.binaryPath, "#!/bin/sh\necho tampered\n");
    // The staged copy must still hold the original verified bytes.
    assert.deepEqual(readFileSync(stagedPath), original);
  });
});

test("run spawns the staged binary, not the source path", async () => {
  await withTemporaryDirectory(async (directory) => {
    const original = Buffer.from("#!/bin/sh\necho bamts\n", "utf8");
    const fixture = await createCliFixture(directory, { binaryBytes: original });

    let spawnedPath = null;
    let spawnedBytes = null;
    const fakeChild = {
      once(event, listener) {
        if (event === "exit") {
          spawnedPath = this._path;
          spawnedBytes = this._bytes;
          // Defer to next tick so the promise can attach handlers.
          queueMicrotask(() => listener(0));
        }
        if (event === "error") {
          this._errorListener = listener;
        }
      },
    };

    const exitCode = await run([], {
      platform: "linux",
      arch: "x64",
      nodeVersion: "24.0.0",
      table: fixture.table,
      facadeManifest: fixture.facadeManifest,
      resolvePackage: fixture.resolvePackage,
      spawn: (command, args, opts) => {
        const child = Object.create(fakeChild);
        child._path = command;
        // Capture the staged bytes now, before cleanupStaged runs after exit.
        child._bytes = readFileSync(command);
        return child;
      },
    });

    assert.equal(exitCode, 0);
    assert.ok(spawnedPath, "spawn must have been called");
    assert.notEqual(spawnedPath, fixture.binaryPath, "spawn must use the staged path");
    assert.deepEqual(spawnedBytes, original);
  });
});

test("the platform packager refuses an empty Cargo build", { skip: process.platform === "win32" }, async () => {
  await withTemporaryDirectory(async (directory) => {
    const tools = join(directory, "tools");
    const cargo = join(tools, "cargo");
    await mkdir(tools, { recursive: true });
    await writeFile(cargo, "#!/bin/sh\nexit 0\n");
    await chmod(cargo, 0o755);

    const result = spawnSync(
      process.execPath,
      [packagingScript, "--target", "x86_64-unknown-linux-gnu"],
      {
        encoding: "utf8",
        env: {
          ...process.env,
          CARGO_TARGET_DIR: join(directory, "target"),
          PATH: `${tools}${delimiter}${process.env.PATH}`,
          BAMTI_SOURCE_COMMIT: "a".repeat(40),
          BAMTI_CARGO_LOCK_SHA256: "b".repeat(64),
          BAMTI_WORKFLOW_REVISION: "cli-release-build-v1",
          BAMTI_TOOLCHAIN: "rustc 1.97.1",
          BAMTI_BUILDER_REVISION: "package-platform-v1",
        },
      },
    );

    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /cargo completed without .*bamts/);
  });
});

test("preflightCliRelease accepts a complete CLI release", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createCliRelease(dist, "0.2.0");
    assert.doesNotThrow(() => preflightCliRelease(dist));
  });
});

test("preflightCliRelease rejects a CLI release with a missing leaf", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createCliRelease(dist, "0.2.0");
    const missing = CLI_TARGETS[0];
    await rm(join(dist, tarballName(missing.package, "0.2.0")));
    assert.throws(
      () => preflightCliRelease(dist),
      (error) =>
        error.message.includes("missing release artifact") &&
        error.message.includes(missing.package),
    );
  });
});

test("preflightCliRelease rejects a tarball with the wrong package name", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createCliRelease(dist, "0.2.0");
    const target = CLI_TARGETS[0];
    await rm(join(dist, tarballName(target.package, "0.2.0")));
    await createTarball(dist, "bamti-cli-linux-x64", "0.2.0", "bamts");
    assert.throws(
      () => preflightCliRelease(dist),
      (error) =>
        error.message.includes("expected name @bamti/cli-linux-x64") &&
        error.message.includes("got bamti-cli-linux-x64"),
    );
  });
});

test("preflightCliRelease rejects a tarball with a missing binary", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createCliRelease(dist, "0.2.0");
    const target = CLI_TARGETS[0];
    await rm(join(dist, tarballName(target.package, "0.2.0")));
    await createTarball(dist, "@bamti/cli-linux-x64", "0.2.0", null);
    assert.throws(
      () => preflightCliRelease(dist),
      (error) => error.message.includes("missing package/bin/bamts"),
    );
  });
});

test("preflightCliRelease rejects a changed binary digest", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    const version = "0.2.0";
    const { cliRows, release } = await createCliRelease(dist, version);
    const target = CLI_TARGETS[0];
    const originalRow = cliRows.find((row) => row.selector === target.selector);
    const tamperedBytes = Buffer.from("tampered\n", "utf8");
    const tamperedRow = makeCliTargetRow(target, tamperedBytes, release);
    tamperedRow.sha256 = originalRow.sha256;
    await rm(join(dist, tarballName(originalRow.package, version)));
    await createCliLeafTarball(dist, tamperedRow, release, tamperedBytes);
    assert.throws(
      () => preflightCliRelease(dist),
      (error) =>
        error.message.includes("digest") &&
        error.message.includes("does not match release table"),
    );
  });
});

test("preflightCliRelease rejects a missing cli-release-table.json", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createCliRelease(dist, "0.2.0", { includeTable: false });
    assert.throws(
      () => preflightCliRelease(dist),
      (error) => error.message.includes("missing or unreadable cli-release-table.json"),
    );
  });
});

test("registry preflight accepts every published optional dependency", () => {
  assert.doesNotThrow(() => assertRegistryAvailability(undefined, availableVersion));
});

test("registry preflight names every unavailable optional dependency", () => {
  const unavailable = new Set([
    "@bamti/cli-linux-x64",
    "@bamti/cli-win32-x64",
  ]);
  assert.throws(
    () =>
      assertRegistryAvailability(undefined, (packageName, version) => {
        if (unavailable.has(packageName)) {
          throw new Error(`${packageName}@${version} returned 404`);
        }
        return version;
      }),
    (error) =>
      error.message.includes("registry preflight failed") &&
      error.message.includes("@bamti/cli-linux-x64@0.2.0 returned 404") &&
      error.message.includes("@bamti/cli-win32-x64@0.2.0 returned 404"),
  );
});

test("preflight accepts a complete CLI and native release", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createFullRelease(dist, "0.2.0");
    assert.doesNotThrow(() => preflightRelease(dist));
  });
});

test("preflightRelease rejects a full release with a missing cli-release-table.json", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await createFullRelease(dist, "0.2.0", { includeCliTable: false });
    assert.throws(
      () => preflightRelease(dist),
      (error) => error.message.includes("missing or unreadable cli-release-table.json"),
    );
  });
});

test("preflight rejects a native release with a missing native target", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    const missing = NATIVE_TARGETS[0];
    await createFullRelease(dist, "0.2.0", { skipNative: new Set([missing.selector]) });
    assert.throws(
      () => preflightRelease(dist),
      (error) =>
        error.message.includes("missing release artifact") &&
        error.message.includes(missing.package),
    );
  });
});

test("preflight rejects a native release with a mismatched release tuple", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    const target = NATIVE_TARGETS[0];
    const version = "0.2.0";
    await createFullRelease(dist, version, {
      mutator: async (distRoot, nativeRows, release) => {
        const row = nativeRows.find(({ selector }) => selector === target.selector);
        const badRelease = makeRelease(version, "b" + release.sourceCommit.slice(1), release.buildSetId);
        await createNativeTarball(distRoot, row, version, nodeBytesFor(row), badRelease);
      },
    });
    assert.throws(
      () => preflightRelease(dist),
      (error) => error.message.includes("release tuple does not match release table"),
    );
  });
});

test("preflight rejects a native release with a changed addon digest", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    const target = NATIVE_TARGETS[0];
    const version = "0.2.0";
    await createFullRelease(dist, version, {
      mutator: async (distRoot, nativeRows, release) => {
        const badSha256 = "0".repeat(64);
        const badRow = nativeRows.find(({ selector }) => selector === target.selector);
        const modifiedRows = nativeRows.map((row) =>
          row.selector === target.selector ? { ...row, sha256: badSha256 } : row,
        );
        await createNativeTarball(distRoot, { ...badRow, sha256: badSha256 }, version, nodeBytesFor(badRow), release);
        await createBamtiReleaseTarball(distRoot, version, modifiedRows, release);
      },
    });
    assert.throws(
      () => preflightRelease(dist),
      (error) =>
        error.message.includes("digest") &&
        error.message.includes("does not match release table"),
    );
  });
});

test("preflight rejects a release with an absent generated table", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    const version = "0.2.0";
    await createFullRelease(dist, version, {
      mutator: async (distRoot, nativeRows, release) => {
        await createBamtiReleaseTarball(distRoot, version, nativeRows, release, {
          includeTable: false,
        });
      },
    });
    assert.throws(
      () => preflightRelease(dist),
      (error) => error.message.includes("missing or unreadable native-release-table.json"),
    );
  });
});

function makeNativeTemplateManifest(row, version) {
  return {
    name: row.package,
    version,
    type: "commonjs",
    main: row.entry,
    files: [row.entry],
    engines: { node: ">=24" },
    os: [row.os],
    cpu: [row.cpu],
    ...(row.libc === undefined ? {} : { libc: [row.libc] }),
    bamtiNative: {
      entry: row.entry,
      target: row.target,
      artifactKind: row.artifactKind,
      sha256: "__BAMTI_SHA256__",
      release: {
        packageVersion: version,
        sourceCommit: "__BAMTI_SOURCE_COMMIT__",
        nativeAbi: 1,
        cliProtocol: 1,
        buildSetId: "__BAMTI_BUILD_SET_ID__",
        releaseId: "__BAMTI_RELEASE_ID__",
      },
    },
  };
}

async function writeJson(root, relativePath, value) {
  const path = join(root, relativePath);
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`);
}

async function createArtifactRoot(directory, version, { missingSelector, mutator } = {}) {
  const bamtiCliManifest = {
    name: "bamti-cli",
    version,
    type: "module",
    optionalDependencies: Object.fromEntries(
      [...ARTIFACTS.values()].map((packageName) => [packageName, version]),
    ),
  };
  await writeJson(directory, "bamti-cli/package.json", bamtiCliManifest);

  for (const [key, packageName] of ARTIFACTS) {
    const dir = packageName.replace(/^@[^/]+\//, "");
    await writeJson(directory, `artifacts/${dir}/package.json`, {
      name: packageName,
      version,
    });
  }

  const bamtiManifest = {
    name: "bamti",
    version,
    type: "module",
    optionalDependencies: Object.fromEntries(
      NATIVE_TARGETS.map((row) => [row.package, version]),
    ),
  };
  await writeJson(directory, "bamti/package.json", bamtiManifest);

  for (const row of NATIVE_TARGETS) {
    if (missingSelector === row.selector) continue;
    const dir = row.package.replace(/^@[^/]+\//, "");
    let manifest = makeNativeTemplateManifest(row, version);
    if (mutator) {
      manifest = mutator(manifest, row);
    }
    await writeJson(directory, `artifacts/${dir}/package.json`, manifest);
  }
}

test("preflight rejects a missing native template manifest", async () => {
  await withTemporaryDirectory(async (directory) => {
    const version = "0.2.0";
    const missing = NATIVE_TARGETS[0];
    await createArtifactRoot(directory, version, { missingSelector: missing.selector });
    assert.throws(
      () => preflight(directory),
      (error) =>
        error.message.includes("missing native artifact manifest") &&
        error.message.includes(missing.selector),
    );
  });
});

test("preflight rejects an incomplete native template manifest", async () => {
  await withTemporaryDirectory(async (directory) => {
    const version = "0.2.0";
    await createArtifactRoot(directory, version, {
      mutator: (manifest, row) => {
        delete manifest.bamtiNative;
        return manifest;
      },
    });
    assert.throws(
      () => preflight(directory),
      (error) => error.message.includes("missing bamtiNative"),
    );
  });
});

test("preflight rejects a mismatched native template target", async () => {
  await withTemporaryDirectory(async (directory) => {
    const version = "0.2.0";
    const target = NATIVE_TARGETS[0];
    await createArtifactRoot(directory, version, {
      mutator: (manifest, row) => {
        if (row.selector === target.selector) {
          manifest.bamtiNative.target = "wrong-target";
        }
        return manifest;
      },
    });
    assert.throws(
      () => preflight(directory),
      (error) =>
        error.message.includes("bamtiNative target") &&
        error.message.includes("wrong-target"),
    );
  });
});

test("preflight rejects a native artifact template with the wrong version", async () => {
  await withTemporaryDirectory(async (directory) => {
    const version = "0.2.0";
    await createArtifactRoot(directory, version, {
      mutator: (manifest, row) => {
        if (row.selector === NATIVE_TARGETS[0].selector) {
          manifest.version = "0.1.0";
        }
        return manifest;
      },
    });
    assert.throws(
      () => preflight(directory),
      (error) =>
        error.message.includes("has version 0.1.0") &&
        error.message.includes("expected 0.2.0"),
    );
  });
});
test("assertRegistryAvailability accepts a consistent family of CLI and native records", () => {
  const commonProvenance = {
    cargoLockSha256: "c".repeat(64),
    workflowRevision: "workflow-v1",
    toolchain: "rustc 1.97.1",
    builderRevision: "builder-v1",
  };
  const { buildSetId } = createNativeBuildPlan({
    sourceCommit: CLI_SOURCE_COMMIT,
    sourceVersion: CLI_VERSION,
    ...commonProvenance,
    targets: NATIVE_RELEASE_TARGETS,
  });
  const release = {
    packageVersion: CLI_VERSION,
    sourceCommit: CLI_SOURCE_COMMIT,
    nativeAbi: 1,
    cliProtocol: 1,
    buildSetId,
    releaseId: `bamti/${CLI_VERSION}/${CLI_SOURCE_COMMIT}/native-abi-1/cli-protocol-1/${buildSetId}`,
  };

  const cliRecords = Object.fromEntries(
    [...ARTIFACTS.values()].map((packageName) => [
      packageName,
      {
        package: packageName,
        family: "cli",
        release,
        provenance: commonProvenance,
      },
    ]),
  );

  const nativeRecords = Object.fromEntries(
    NATIVE_TARGETS.map((fixed) => [
      fixed.package,
      {
        package: fixed.package,
        family: "native",
        release,
        provenance: commonProvenance,
      },
    ]),
  );

  assert.doesNotThrow(() =>
    assertRegistryAvailability(undefined, (packageName) =>
      cliRecords[packageName] ?? nativeRecords[packageName],
    ),
  );
});

test("assertRegistryAvailability rejects a mixed build set in the CLI family", () => {
  const commonProvenance = {
    cargoLockSha256: "c".repeat(64),
    workflowRevision: "workflow-v1",
    toolchain: "rustc 1.97.1",
    builderRevision: "builder-v1",
  };
  const { buildSetId } = createNativeBuildPlan({
    sourceCommit: CLI_SOURCE_COMMIT,
    sourceVersion: CLI_VERSION,
    ...commonProvenance,
    targets: NATIVE_RELEASE_TARGETS,
  });
  const release = {
    packageVersion: CLI_VERSION,
    sourceCommit: CLI_SOURCE_COMMIT,
    nativeAbi: 1,
    cliProtocol: 1,
    buildSetId,
    releaseId: `bamti/${CLI_VERSION}/${CLI_SOURCE_COMMIT}/native-abi-1/cli-protocol-1/${buildSetId}`,
  };

  const cliRecords = Object.fromEntries(
    [...ARTIFACTS.values()].map((packageName) => [
      packageName,
      {
        package: packageName,
        family: "cli",
        release,
        provenance: commonProvenance,
      },
    ]),
  );

  const mixedPackage = [...ARTIFACTS.values()][0];
  cliRecords[mixedPackage].provenance = {
    ...commonProvenance,
    builderRevision: "builder-v2",
  };

  const nativeRecords = Object.fromEntries(
    NATIVE_TARGETS.map((fixed) => [
      fixed.package,
      {
        package: fixed.package,
        family: "native",
        release,
        provenance: commonProvenance,
      },
    ]),
  );

  assert.throws(
    () =>
      assertRegistryAvailability(undefined, (packageName) =>
        cliRecords[packageName] ?? nativeRecords[packageName],
      ),
    (error) =>
      error.message.includes("CLI release family") &&
      error.message.includes("different build provenance"),
  );
});

test("assertRegistryAvailability accepts family-specific build sets and rejects shared drift", () => {
  const cliProvenance = {
    cargoLockSha256: "c".repeat(64),
    workflowRevision: "workflow-cli",
    toolchain: "rustc 1.97.1",
    builderRevision: "builder-cli",
  };
  const { buildSetId: cliBuildSetId } = createNativeBuildPlan({
    sourceCommit: CLI_SOURCE_COMMIT,
    sourceVersion: CLI_VERSION,
    ...cliProvenance,
    targets: NATIVE_RELEASE_TARGETS,
  });
  const release = {
    packageVersion: CLI_VERSION,
    sourceCommit: CLI_SOURCE_COMMIT,
    nativeAbi: 1,
    cliProtocol: 1,
    buildSetId: cliBuildSetId,
    releaseId: `bamti/${CLI_VERSION}/${CLI_SOURCE_COMMIT}/native-abi-1/cli-protocol-1/${cliBuildSetId}`,
  };
  const cliRecords = Object.fromEntries(
    [...ARTIFACTS.values()].map((packageName) => [
      packageName,
      {
        package: packageName,
        family: "cli",
        release,
        provenance: cliProvenance,
      },
    ]),
  );

  const nativeProvenance = {
    ...cliProvenance,
    workflowRevision: "workflow-native",
    builderRevision: "builder-native",
  };
  const { buildSetId: nativeBuildSetId } = createNativeBuildPlan({
    sourceCommit: CLI_SOURCE_COMMIT,
    sourceVersion: CLI_VERSION,
    ...nativeProvenance,
    targets: NATIVE_RELEASE_TARGETS,
  });
  const nativeRelease = {
    ...release,
    buildSetId: nativeBuildSetId,
    releaseId: `bamti/${CLI_VERSION}/${CLI_SOURCE_COMMIT}/native-abi-1/cli-protocol-1/${nativeBuildSetId}`,
  };
  const nativeRecords = Object.fromEntries(
    NATIVE_TARGETS.map((fixed) => [
      fixed.package,
      {
        package: fixed.package,
        family: "native",
        release: nativeRelease,
        provenance: nativeProvenance,
      },
    ]),
  );
  const lookupVersion = (packageName) =>
    cliRecords[packageName] ?? nativeRecords[packageName];

  assert.doesNotThrow(() => assertRegistryAvailability(undefined, lookupVersion));

  nativeRelease.nativeAbi = 2;
  nativeRelease.releaseId =
    `bamti/${CLI_VERSION}/${CLI_SOURCE_COMMIT}/native-abi-2/cli-protocol-1/${nativeBuildSetId}`;
  assert.throws(
    () => assertRegistryAvailability(undefined, lookupVersion),
    (error) =>
      error.message.includes("CLI and native release families") &&
      error.message.includes("different shared release identities"),
  );

  nativeRelease.nativeAbi = 1;
  nativeProvenance.cargoLockSha256 = "d".repeat(64);
  const { buildSetId: changedNativeBuildSetId } = createNativeBuildPlan({
    sourceCommit: CLI_SOURCE_COMMIT,
    sourceVersion: CLI_VERSION,
    ...nativeProvenance,
    targets: NATIVE_RELEASE_TARGETS,
  });
  nativeRelease.buildSetId = changedNativeBuildSetId;
  nativeRelease.releaseId =
    `bamti/${CLI_VERSION}/${CLI_SOURCE_COMMIT}/native-abi-1/cli-protocol-1/${changedNativeBuildSetId}`;
  assert.throws(
    () => assertRegistryAvailability(undefined, lookupVersion),
    (error) =>
      error.message.includes("CLI and native release families") &&
      error.message.includes("different shared build provenance"),
  );
});