import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import {
  chmod,
  copyFile as fsCopyFile,
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { CLI_TARGETS } from "../bamti-cli/index.js";
import { createNativeBuildPlan } from "./native-build-plan.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const defaultNpmRoot = resolve(dirname(scriptPath), "..");
const defaultRepositoryRoot = resolve(defaultNpmRoot, "..");

const RELEASE_STRING_KEYS = [
  "packageVersion",
  "sourceCommit",
  "buildSetId",
  "releaseId",
];
const RELEASE_NUMBER_KEYS = ["nativeAbi", "cliProtocol"];
const COMMON_PROVENANCE_KEYS = [
  "cargoLockSha256",
  "workflowRevision",
  "toolchain",
  "builderRevision",
];

function fail(message) {
  throw new Error(`bamti CLI package build: ${message}`);
}

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function canonicalReleaseId(release) {
  return `bamti/${release.packageVersion}/${release.sourceCommit}/native-abi-${release.nativeAbi}/cli-protocol-${release.cliProtocol}/${release.buildSetId}`;
}

function validateReleaseTuple(release) {
  if (
    !isRecord(release) ||
    RELEASE_STRING_KEYS.some(
      (key) => typeof release[key] !== "string" || release[key] === "",
    ) ||
    release.nativeAbi !== 1 ||
    release.cliProtocol !== 1
  ) {
    fail("release tuple is incomplete or has an unsupported ABI/protocol");
  }
  if (!/^[0-9a-f]{64}$/.test(release.buildSetId)) {
    fail("release tuple has an invalid buildSetId");
  }
  if (release.releaseId !== canonicalReleaseId(release)) {
    fail("release tuple has a non-canonical releaseId");
  }
}

function equalRelease(actual, expected) {
  return (
    isRecord(actual) &&
    RELEASE_STRING_KEYS.every((key) => actual[key] === expected[key]) &&
    RELEASE_NUMBER_KEYS.every((key) => actual[key] === expected[key])
  );
}

function validateProvenance(value, label) {
  if (!isRecord(value) || !/^[0-9a-f]{64}$/.test(value.cargoLockSha256)) {
    fail(`${label} has an invalid cargoLockSha256`);
  }
  for (const key of ["workflowRevision", "toolchain", "builderRevision", "runner"]) {
    if (typeof value[key] !== "string" || value[key] === "") {
      fail(`${label} has an invalid ${key}`);
    }
  }
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

async function writeJson(path, value) {
  await writeFile(path, `${JSON.stringify(value, null, 2)}\n`);
}

function defaultRunCargo({ args, env, cwd }) {
  const command = process.platform === "win32" ? "cargo.cmd" : "cargo";
  const result = spawnSync(command, args, { cwd, env, stdio: "inherit" });
  if (result.error) {
    fail(`could not start ${command}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} failed with exit code ${result.status ?? "unknown"}.`);
  }
}

function defaultRunNpm({ args, cwd }) {
  const command = process.platform === "win32" ? "npm.cmd" : "npm";
  const result = spawnSync(command, args, { cwd, stdio: "inherit" });
  if (result.error) {
    fail(`could not start ${command}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} failed with exit code ${result.status ?? "unknown"}.`);
  }
}

function artifactDirectoryName(packageName) {
  return packageName.replace(/^@[^/]+\//, "");
}

function packageNameToTarballName(packageName, version) {
  return `${packageName.replace(/^@/, "").replace(/\//g, "-")}-${version}.tgz`;
}

function sha256Hex(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function gitSourceCommit(repositoryRoot) {
  const result = spawnSync("git", ["rev-parse", "HEAD"], {
    cwd: repositoryRoot,
    encoding: "utf8",
  });
  if (result.error || result.status !== 0) {
    const detail = result.stderr?.trim() || result.error?.message || "git failed";
    fail(`could not determine source commit: ${detail}`);
  }
  return result.stdout.trim();
}

function requiredProvenance(options, env) {
  const provenance = {
    cargoLockSha256:
      options.cargoLockSha256 ?? env.BAMTI_CARGO_LOCK_SHA256,
    workflowRevision:
      options.workflowRevision ?? env.BAMTI_WORKFLOW_REVISION,
    toolchain: options.toolchain ?? env.BAMTI_TOOLCHAIN,
    builderRevision:
      options.builderRevision ?? env.BAMTI_BUILDER_REVISION,
    runner: options.runner,
  };
  validateProvenance(provenance, "build plan");
  return provenance;
}

function releaseForBuild({
  packageVersion,
  sourceCommit,
  provenance,
  configuredBuildSetId,
}) {
  const { buildSetId } = createNativeBuildPlan({
    sourceCommit,
    sourceVersion: packageVersion,
    cargoLockSha256: provenance.cargoLockSha256,
    workflowRevision: provenance.workflowRevision,
    toolchain: provenance.toolchain,
    builderRevision: provenance.builderRevision,
    targets: CLI_TARGETS.map(({ target }) => target),
  });
  if (configuredBuildSetId !== undefined && configuredBuildSetId !== buildSetId) {
    fail(
      `configured buildSetId ${configuredBuildSetId} does not match canonical plan ${buildSetId}`,
    );
  }
  const release = {
    packageVersion,
    sourceCommit,
    nativeAbi: 1,
    cliProtocol: 1,
    buildSetId,
    releaseId: "",
  };
  release.releaseId = canonicalReleaseId(release);
  validateReleaseTuple(release);
  return release;
}

function assertCanonicalBuildIdentity(release, provenance) {
  const { buildSetId } = createNativeBuildPlan({
    sourceCommit: release.sourceCommit,
    sourceVersion: release.packageVersion,
    cargoLockSha256: provenance.cargoLockSha256,
    workflowRevision: provenance.workflowRevision,
    toolchain: provenance.toolchain,
    builderRevision: provenance.builderRevision,
    targets: CLI_TARGETS.map(({ target }) => target),
  });
  if (release.buildSetId !== buildSetId) {
    fail("record release buildSetId does not match its canonical build plan");
  }
}

export async function packageCliTarget(targetTriple, options = {}) {
  const fixed = CLI_TARGETS.find(({ target }) => target === targetTriple);
  if (!fixed) {
    fail(
      `unsupported target ${targetTriple}. Supported targets: ${CLI_TARGETS.map(({ target }) => target).join(", ")}.`,
    );
  }

  const npmRoot = options.npmRoot ?? defaultNpmRoot;
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  const outputDirectory = options.outputDirectory ?? join(npmRoot, "dist");
  const runCargo = options.runCargo ?? defaultRunCargo;
  const runNpm = options.runNpm ?? defaultRunNpm;
  const env = options.env ?? process.env;

  const facadeManifest = await readJson(join(npmRoot, "bamti-cli", "package.json"));
  const version = facadeManifest.version;
  if (typeof version !== "string" || version === "") {
    fail("bamti-cli/package.json has no version");
  }

  const sourceCommit =
    options.sourceCommit ?? env.BAMTI_SOURCE_COMMIT ?? gitSourceCommit(repositoryRoot);
  const provenance = requiredProvenance({ ...options, runner: fixed.runner }, env);
  const release = releaseForBuild({
    packageVersion: version,
    sourceCommit,
    provenance,
    configuredBuildSetId: options.buildSetId ?? env.BAMTI_BUILD_SET_ID,
  });

  runCargo({
    args: ["build", "--release", "--locked", "--target", targetTriple, "-p", "bamts-cli"],
    env: {
      ...env,
      BAMTI_RELEASE_ID: release.releaseId,
      BAMTI_RELEASE_PACKAGE_VERSION: release.packageVersion,
      BAMTI_BUILD_SET_ID: release.buildSetId,
      BAMTI_SOURCE_COMMIT: release.sourceCommit,
      BAMTI_TARGET: targetTriple,
      BAMTI_ARTIFACT_KIND: fixed.artifactKind,
      BAMTI_NATIVE_ABI: String(release.nativeAbi),
      BAMTI_CLI_PROTOCOL: String(release.cliProtocol),
    },
    cwd: repositoryRoot,
  });

  const cargoTargetDirectory = env.CARGO_TARGET_DIR
    ? resolve(repositoryRoot, env.CARGO_TARGET_DIR)
    : join(repositoryRoot, "target");
  const binary = join(
    cargoTargetDirectory,
    targetTriple,
    "release",
    fixed.entry.split("/").at(-1),
  );
  if (!existsSync(binary)) {
    fail(
      `cargo completed without ${binary}. The workspace must define a bamts binary target before an npm artifact can be packed.`,
    );
  }
  const stagingRoot = await mkdtemp(join(tmpdir(), "bamti-cli-package-"));
  try {
    const directory = artifactDirectoryName(fixed.package);
    const packageDirectory = join(stagingRoot, directory);
    await cp(join(npmRoot, "artifacts", directory), packageDirectory, {
      recursive: true,
    });
    const packagedBinary = join(packageDirectory, fixed.entry);
    await mkdir(dirname(packagedBinary), { recursive: true });
    const copy = options.copyFile ?? fsCopyFile;
    await copy(binary, packagedBinary);
    if (fixed.os !== "win32") {
      await chmod(packagedBinary, 0o755);
    }
    const binaryBytes = await readFile(packagedBinary);
    const sha256 = sha256Hex(binaryBytes);

    const manifestPath = join(packageDirectory, "package.json");
    const manifest = await readJson(manifestPath);
    if (!isRecord(manifest.bamtiCli)) {
      fail(`${directory}/package.json is missing bamtiCli`);
    }
    manifest.version = version;
    manifest.engines = { node: ">=24" };
    manifest.os = [fixed.os];
    manifest.cpu = [fixed.cpu];
    manifest.files = ["README.md", fixed.entry];
    manifest.bamtiCli = {
      entry: fixed.entry,
      target: fixed.target,
      artifactKind: fixed.artifactKind,
      sha256,
      cargoLockSha256: provenance.cargoLockSha256,
      workflowRevision: provenance.workflowRevision,
      toolchain: provenance.toolchain,
      builderRevision: provenance.builderRevision,
      runner: fixed.runner,
      release,
    };
    await writeJson(manifestPath, manifest);

    await mkdir(outputDirectory, { recursive: true });
    await runNpm({
      args: ["pack", "--pack-destination", outputDirectory],
      cwd: packageDirectory,
    });

    const record = {
      selector: fixed.selector,
      target: fixed.target,
      package: fixed.package,
      entry: fixed.entry,
      os: fixed.os,
      cpu: fixed.cpu,
      artifactKind: fixed.artifactKind,
      version,
      sha256,
      cargoLockSha256: provenance.cargoLockSha256,
      workflowRevision: provenance.workflowRevision,
      toolchain: provenance.toolchain,
      builderRevision: provenance.builderRevision,
      runner: fixed.runner,
      release,
    };
    const recordPath = join(
      outputDirectory,
      `bamti-cli-${fixed.selector}-${version}.json`,
    );
    await writeJson(recordPath, record);

    return {
      record,
      recordPath,
      tarballPath: join(
        outputDirectory,
        packageNameToTarballName(fixed.package, version),
      ),
    };
  } finally {
    await rm(stagingRoot, { recursive: true, force: true });
  }
}

export async function assembleCliRecords(recordPaths, options = {}) {
  if (!Array.isArray(recordPaths)) {
    fail("record paths must be an array");
  }
  if (recordPaths.length !== CLI_TARGETS.length) {
    fail(
      `assemble requires exactly ${CLI_TARGETS.length} records, got ${recordPaths.length}`,
    );
  }

  const npmRoot = options.npmRoot ?? defaultNpmRoot;
  const outputDirectory = options.outputDirectory ?? join(npmRoot, "dist");
  const runNpm = options.runNpm ?? defaultRunNpm;

  const records = [];
  for (const recordPath of recordPaths) {
    const record = await readJson(recordPath);
    if (!isRecord(record) || !isRecord(record.release)) {
      fail(`record ${recordPath} is incomplete`);
    }
    validateReleaseTuple(record.release);
    validateProvenance(record, `record ${recordPath}`);
    records.push(record);
  }

  const first = records[0];
  for (let index = 1; index < records.length; index += 1) {
    const other = records[index];
    if (!equalRelease(other.release, first.release)) {
      fail(`record ${recordPaths[index]} has a different release tuple`);
    }
    if (
      COMMON_PROVENANCE_KEYS.some((key) => other[key] !== first[key])
    ) {
      fail(`record ${recordPaths[index]} has different build provenance`);
    }
  }
  assertCanonicalBuildIdentity(first.release, first);

  const bySelector = new Map();
  for (let index = 0; index < records.length; index += 1) {
    const record = records[index];
    if (typeof record.selector !== "string" || record.selector === "") {
      fail(`record ${recordPaths[index]} has an invalid selector`);
    }
    if (bySelector.has(record.selector)) {
      fail(`duplicate selector ${record.selector} in records`);
    }
    const fixed = CLI_TARGETS.find(({ selector }) => selector === record.selector);
    if (!fixed) {
      fail(`record ${recordPaths[index]} has unknown selector ${record.selector}`);
    }
    for (const key of [
      "target",
      "package",
      "entry",
      "os",
      "cpu",
      "artifactKind",
      "runner",
    ]) {
      if (record[key] !== fixed[key]) {
        fail(`record ${recordPaths[index]} has wrong ${key}`);
      }
    }
    if (record.version !== first.release.packageVersion) {
      fail(
        `record ${recordPaths[index]} has version ${record.version}, expected ${first.release.packageVersion}`,
      );
    }
    if (!/^[0-9a-f]{64}$/.test(record.sha256)) {
      fail(`record ${recordPaths[index]} has invalid sha256`);
    }
    bySelector.set(record.selector, record);
  }

  for (const { selector } of CLI_TARGETS) {
    if (!bySelector.has(selector)) {
      fail(`missing record for selector ${selector}`);
    }
  }

  const targets = CLI_TARGETS.map((fixed) => {
    const record = bySelector.get(fixed.selector);
    return Object.freeze({
      selector: fixed.selector,
      target: fixed.target,
      package: fixed.package,
      entry: fixed.entry,
      os: fixed.os,
      cpu: fixed.cpu,
      artifactKind: fixed.artifactKind,
      version: record.version,
      sha256: record.sha256,
      cargoLockSha256: record.cargoLockSha256,
      workflowRevision: record.workflowRevision,
      toolchain: record.toolchain,
      builderRevision: record.builderRevision,
      runner: fixed.runner,
    });
  });
  const table = Object.freeze({
    version: 1,
    release: Object.freeze({ ...first.release }),
    targets: Object.freeze(targets),
  });

  const stagingRoot = await mkdtemp(join(tmpdir(), "bamti-cli-facade-"));
  try {
    const packageDirectory = join(stagingRoot, "bamti-cli");
    await cp(join(npmRoot, "bamti-cli"), packageDirectory, { recursive: true });

    const manifestPath = join(packageDirectory, "package.json");
    const manifest = await readJson(manifestPath);
    if (!isRecord(manifest.optionalDependencies)) {
      fail("bamti-cli/package.json is missing optionalDependencies");
    }
    manifest.version = first.release.packageVersion;
    for (const { package: packageName } of CLI_TARGETS) {
      if (!(packageName in manifest.optionalDependencies)) {
        fail(`bamti-cli/package.json is missing optional dependency ${packageName}`);
      }
      manifest.optionalDependencies[packageName] = first.release.packageVersion;
    }
    await writeJson(manifestPath, manifest);
    await writeJson(join(packageDirectory, "cli-release-table.json"), table);

    await mkdir(outputDirectory, { recursive: true });
    await runNpm({
      args: ["pack", "--pack-destination", outputDirectory],
      cwd: packageDirectory,
    });

    return {
      table,
      tarballPath: join(
        outputDirectory,
        packageNameToTarballName("bamti-cli", first.release.packageVersion),
      ),
    };
  } finally {
    await rm(stagingRoot, { recursive: true, force: true });
  }
}

export async function main(argv = process.argv.slice(2), options = {}) {
  const npmRoot = options.npmRoot ?? defaultNpmRoot;
  const repositoryRoot = options.repositoryRoot ?? defaultRepositoryRoot;
  const outputDirectory =
    options.outputDirectory ??
    (process.env.BAMTI_DIST_ROOT
      ? resolve(process.env.BAMTI_DIST_ROOT)
      : join(npmRoot, "dist"));

  if (argv[0] === "--assemble") {
    return assembleCliRecords(argv.slice(1).map((path) => resolve(path)), {
      npmRoot,
      outputDirectory,
    });
  }

  const values = {};
  for (let index = 0; index < argv.length; index += 1) {
    const key = argv[index];
    if (!key.startsWith("--") || index + 1 >= argv.length) {
      fail(`unknown argument ${key}`);
    }
    values[key.slice(2)] = argv[index + 1];
    index += 1;
  }
  if (!values.target) {
    fail("use --target <rust-target-triple> or --assemble <five record paths>");
  }

  return packageCliTarget(values.target, {
    npmRoot,
    repositoryRoot,
    outputDirectory,
    sourceCommit: values["source-commit"],
    buildSetId: values["build-set-id"],
    cargoLockSha256: values["cargo-lock-sha256"],
    workflowRevision: values["workflow-revision"],
    toolchain: values.toolchain,
    builderRevision: values["builder-revision"],
  });
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
