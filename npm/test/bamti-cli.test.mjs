import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile, chmod } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join, delimiter } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

import {
  ArtifactNotFoundError,
  UnsupportedPlatformError,
  resolveBinary,
} from "../bamti-cli/index.js";

const packagingScript = join(dirname(fileURLToPath(import.meta.url)), "..", "scripts", "package-platform.mjs");

async function withTemporaryDirectory(callback) {
  const directory = await mkdtemp(join(tmpdir(), "bamti-cli-test-"));
  try {
    await callback(directory);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("the bamts shim resolves a present Linux artifact", async () => {
  await withTemporaryDirectory(async (directory) => {
    const artifactDirectory = join(directory, "node_modules", "@bamti", "cli-linux-x64");
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
    const consumerRequire = createRequire(join(directory, "consumer.cjs"));
    assert.throws(
      () => resolveBinary({ platform: "linux", arch: "x64", resolvePackage: consumerRequire.resolve }),
      (error) =>
        error instanceof ArtifactNotFoundError &&
        error.message.includes("@bamti/cli-linux-x64") &&
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
