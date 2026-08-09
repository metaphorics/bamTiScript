import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { ARTIFACTS } from "../bamti-cli/index.js";

const scriptPath = fileURLToPath(import.meta.url);
const scriptDir = dirname(scriptPath);
const npmRoot = dirname(scriptDir);

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

export function artifactDirectory(packageName) {
  // "@bamti/cli-linux-x64" -> "cli-linux-x64"
  return packageName.replace(/^@[^/]+\//, "");
}

export function artifactBinaryName(platform) {
  return platform === "win32" ? "bamts.exe" : "bamts";
}

export function splitPlatform(key) {
  const lastDash = key.lastIndexOf("-");
  return [key.slice(0, lastDash), key.slice(lastDash + 1)];
}

export function tarballName(packageName, version) {
  // npm pack turns "@scope/name" into "scope-name-version.tgz".
  return `${packageName.replace(/^@/, "").replace(/\//g, "-")}-${version}.tgz`;
}

function readTarballPackageJson(tarballPath) {
  const result = spawnSync(
    "tar",
    ["-xOzf", tarballPath, "package/package.json"],
    { encoding: "utf8" },
  );
  if (result.error || result.status !== 0) {
    throw new Error(
      `could not read package/package.json from ${tarballPath}: ${
        result.stderr || result.error?.message || "tar failed"
      }`,
    );
  }
  return JSON.parse(result.stdout);
}

function tarballHasBinary(tarballPath, binary) {
  const result = spawnSync("tar", ["-tzf", tarballPath], { encoding: "utf8" });
  if (result.error || result.status !== 0) return false;
  return result.stdout.split("\n").some((line) => line === `package/bin/${binary}`);
}

export function assertManifestConsistency(root = npmRoot) {
  const cliManifest = readJson(join(root, "bamti-cli", "package.json"));
  const expectedVersion = cliManifest.version;

  const loaderNames = [...ARTIFACTS.values()].sort();
  const optionalNames = Object.keys(cliManifest.optionalDependencies ?? {}).sort();

  if (JSON.stringify(loaderNames) !== JSON.stringify(optionalNames)) {
    throw new Error(
      `bamti-cli loader and optionalDependencies disagree:\n` +
        `  loader: ${loaderNames.join(", ")}\n` +
        `  optionalDependencies: ${optionalNames.join(", ")}`,
    );
  }

  for (const [key, packageName] of ARTIFACTS) {
    const dir = artifactDirectory(packageName);
    const manifest = readJson(join(root, "artifacts", dir, "package.json"));
    if (manifest.name !== packageName) {
      throw new Error(
        `artifact manifest ${dir}/package.json has name ${manifest.name}, expected ${packageName}`,
      );
    }
    if (manifest.version !== expectedVersion) {
      throw new Error(
        `artifact manifest ${dir}/package.json has version ${manifest.version}, expected ${expectedVersion}`,
      );
    }
  }
}

export function preflight(root = npmRoot) {
  // Local, deterministic check that the loader and the artifact manifests agree.
  assertManifestConsistency(root);
}

export function preflightRelease(distRoot, root = npmRoot) {
  assertManifestConsistency(root);

  if (distRoot === undefined) {
    distRoot = join(root, "dist");
  }

  const cliManifest = readJson(join(root, "bamti-cli", "package.json"));
  const expectedVersion = cliManifest.version;

  let entries;
  try {
    entries = readdirSync(distRoot);
  } catch (cause) {
    throw new Error(`release preflight: dist directory not found: ${distRoot}`, { cause });
  }

  const errors = [];

  for (const [key, packageName] of ARTIFACTS) {
    const [platform, arch] = splitPlatform(key);
    const binary = artifactBinaryName(platform);
    const tarball = tarballName(packageName, expectedVersion);

    if (!entries.includes(tarball)) {
      errors.push(`missing release artifact: ${tarball} (${packageName}@${expectedVersion})`);
      continue;
    }

    const tarballPath = join(distRoot, tarball);
    let manifest;
    try {
      manifest = readTarballPackageJson(tarballPath);
    } catch (cause) {
      errors.push(`${tarball}: could not read manifest: ${cause.message}`);
      continue;
    }

    if (manifest.name !== packageName) {
      errors.push(`${tarball}: expected name ${packageName}, got ${manifest.name}`);
    }
    if (manifest.version !== expectedVersion) {
      errors.push(`${tarball}: expected version ${expectedVersion}, got ${manifest.version}`);
    }
    if (!tarballHasBinary(tarballPath, binary)) {
      errors.push(`${tarball}: missing package/bin/${binary}`);
    }
  }

  if (errors.length > 0) {
    throw new Error("release preflight failed:\n  - " + errors.join("\n  - "));
  }
}

if (process.argv[1] === scriptPath) {
  preflightRelease();
}
