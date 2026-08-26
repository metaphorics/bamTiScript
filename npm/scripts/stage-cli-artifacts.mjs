#!/usr/bin/env node

import { createHash } from "node:crypto";
import { mkdir, readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

import { CLI_TARGETS } from "../bamti-cli/index.js";
import {
  assembleCliRecords,
  packageCliTarget,
} from "./package-platform.mjs";
import { createNativeBuildPlan } from "./native-build-plan.mjs";
import { assertTargetBinaryIdentity } from "./target-binary-identity.mjs";

export {
  ArtifactIdentityError,
  assertTargetBinaryIdentity,
} from "./target-binary-identity.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const npmRoot = resolve(dirname(scriptPath), "..");
const repositoryRoot = resolve(npmRoot, "..");

export class ArtifactMissingError extends Error {
  constructor(target, path, cause) {
    super(`stage-cli-artifacts: ${target} binary missing at ${path}`, { cause });
    this.name = "ArtifactMissingError";
    this.target = target;
    this.path = path;
  }
}

export function targetBinaryPath(artifactRoot, fixed) {
  return join(artifactRoot, fixed.target, "release", fixed.entry.split("/").at(-1));
}

export async function readTargetBinary(artifactRoot, fixed) {
  const path = targetBinaryPath(artifactRoot, fixed);
  let bytes;
  try {
    bytes = await readFile(path);
  } catch (cause) {
    throw new ArtifactMissingError(fixed.target, path, cause);
  }
  assertTargetBinaryIdentity(bytes, fixed.target);
  return { path, bytes };
}

async function stageTarget(fixed, options) {
  await readTargetBinary(options.artifactRoot, fixed);
  return packageCliTarget(fixed.target, {
    npmRoot,
    repositoryRoot,
    outputDirectory: options.outputDirectory,
    buildSetId: options.buildSetId,
    sourceCommit: options.sourceCommit,
    ...options.provenance,
    env: {
      ...process.env,
      CARGO_TARGET_DIR: options.artifactRoot,
    },
    runCargo: ({ args, env }) => {
      if (
        args.join("\0") !==
        ["build", "--release", "--locked", "--target", fixed.target, "-p", "bamts-cli"].join("\0")
      ) {
        throw new Error(`unexpected cargo invocation: ${args.join(" ")}`);
      }
      if (env.BAMTI_TARGET !== fixed.target) {
        throw new Error(`cargo env target mismatch for ${fixed.target}`);
      }
    },
  });
}

function commandOutput(command, args, cwd = repositoryRoot) {
  const result = spawnSync(command, args, { cwd, encoding: "utf8" });
  if (result.error || result.status !== 0) {
    const detail = result.stderr?.trim() || result.error?.message || `exit ${result.status}`;
    throw new Error(`stage-cli-artifacts: ${command} ${args.join(" ")} failed: ${detail}`);
  }
  return result.stdout.trim();
}

export async function stageCliArtifacts() {
  const outputDirectory = process.env.BAMTI_DIST_ROOT
    ? resolve(process.env.BAMTI_DIST_ROOT)
    : join(npmRoot, "dist");
  const artifactRoot = process.env.BAMTI_CLI_ARTIFACT_ROOT
    ? resolve(process.env.BAMTI_CLI_ARTIFACT_ROOT)
    : join(repositoryRoot, "target");
  const sourceCommit =
    process.env.BAMTI_SOURCE_COMMIT ?? commandOutput("git", ["rev-parse", "HEAD"]);
  const provenance = Object.freeze({
    cargoLockSha256:
      process.env.BAMTI_CARGO_LOCK_SHA256 ??
      createHash("sha256")
        .update(await readFile(join(repositoryRoot, "Cargo.lock")))
        .digest("hex"),
    workflowRevision: process.env.BAMTI_WORKFLOW_REVISION ?? "local-stage-cli-artifacts",
    toolchain:
      process.env.BAMTI_TOOLCHAIN ?? commandOutput("rustc", ["--version"], repositoryRoot),
    builderRevision: process.env.BAMTI_BUILDER_REVISION ?? "stage-cli-artifacts-v2",
  });
  const buildSetId = createNativeBuildPlan({
    sourceCommit,
    sourceVersion: "0.2.0",
    ...provenance,
    targets: CLI_TARGETS.map(({ target }) => target),
  }).buildSetId;

  await mkdir(outputDirectory, { recursive: true });
  const results = [];
  for (const fixed of CLI_TARGETS) {
    const result = await stageTarget(fixed, {
      artifactRoot,
      outputDirectory,
      buildSetId,
      sourceCommit,
      provenance,
    });
    results.push(result);
    process.stdout.write(`staged ${result.tarballPath}\n`);
  }

  const assembled = await assembleCliRecords(
    results.map(({ recordPath }) => recordPath),
    { npmRoot, outputDirectory },
  );
  process.stdout.write(`assembled ${assembled.tarballPath}\n`);
  return { results, assembled };
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  await stageCliArtifacts();
}
