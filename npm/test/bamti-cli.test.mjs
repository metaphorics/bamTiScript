import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile, chmod } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join, delimiter } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

import {
  ARTIFACTS,
  ArtifactNotFoundError,
  UnsupportedPlatformError,
  artifactPackage,
  resolveBinary,
} from "../bamti-cli/index.js";
import { preflight, preflightRelease, tarballName } from "../scripts/preflight.mjs";

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

test("ARTIFACTS, manifests, and optionalDependencies agree", () => {
  // Throws if any of the three call sites disagree.
  preflight();
});

test("the bamts shim resolves a present Linux artifact", async () => {
  await withTemporaryDirectory(async (directory) => {
    const packageName = artifactPackage("linux", "x64");
    const artifactDirectory = join(directory, "node_modules", ...packageName.split("/"));
    const binary = join(artifactDirectory, "bin", "bamts");
    await mkdir(dirname(binary), { recursive: true });
    await writeFile(join(artifactDirectory, "package.json"), "{}\n");
    await writeFile(binary, "artifact\n");

    const consumerRequire = createRequire(join(directory, "consumer.cjs"));
    assert.equal(
      resolveBinary({
        platform: "linux",
        arch: "x64",
        resolvePackage: consumerRequire.resolve,
      }),
      binary,
    );
  });
});

test("the bamts shim reports a missing platform artifact", async () => {
  await withTemporaryDirectory(async (directory) => {
    const packageName = artifactPackage("linux", "x64");
    const consumerRequire = createRequire(join(directory, "consumer.cjs"));
    assert.throws(
      () => resolveBinary({ platform: "linux", arch: "x64", resolvePackage: consumerRequire.resolve }),
      (error) =>
        error instanceof ArtifactNotFoundError &&
        error.message.includes(packageName) &&
        error.message.includes("optional dependencies enabled"),
    );
  });
});

test("the bamts shim rejects unsupported platforms", () => {
  assert.throws(
    () => resolveBinary({ platform: "win32", arch: "arm64" }),
    (error) => error instanceof UnsupportedPlatformError && error.message.includes("win32-arm64"),
  );
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
        },
      },
    );

    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /cargo completed without .*bamts/);
  });
});

test("preflight rejects a release when a platform artifact is missing", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    const version = "0.1.0";
    await mkdir(dist, { recursive: true });
    // Create only four of the five required tarballs.
    const entries = [...ARTIFACTS.entries()];
    for (let i = 0; i < 4; i += 1) {
      const [key, packageName] = entries[i];
      const platform = key.slice(0, key.lastIndexOf("-"));
      const binary = platform === "win32" ? "bamts.exe" : "bamts";
      await createTarball(dist, packageName, version, binary);
    }
    assert.throws(
      () => preflightRelease(dist),
      (error) => error.message.includes("missing release artifact"),
    );
  });
});

test("preflight accepts a complete, consistent release", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await mkdir(dist, { recursive: true });
    await createAllTarballs(dist, "0.1.0");
    assert.doesNotThrow(() => preflightRelease(dist));
  });
});

test("preflight rejects a tarball with the wrong package name", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await mkdir(dist, { recursive: true });
    await createAllTarballs(dist, "0.1.0", async () => {
      // Overwrite the Linux x64 tarball with an unscoped-name tarball.
      await createTarball(dist, "bamti-cli-linux-x64", "0.1.0", "bamts");
    });
    assert.throws(
      () => preflightRelease(dist),
      (error) =>
        error.message.includes("expected name @bamti/cli-linux-x64") &&
        error.message.includes("got bamti-cli-linux-x64"),
    );
  });
});

test("preflight rejects a tarball with a missing binary", async () => {
  await withTemporaryDirectory(async (directory) => {
    const dist = join(directory, "dist");
    await mkdir(dist, { recursive: true });
    await createAllTarballs(dist, "0.1.0", async () => {
      await createTarball(dist, "@bamti/cli-linux-x64", "0.1.0", null);
    });
    assert.throws(
      () => preflightRelease(dist),
      (error) => error.message.includes("missing package/bin/bamts"),
    );
  });
});
