import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";

const resolveFromHere = createRequire(import.meta.url).resolve;

const ARTIFACTS = new Map([
  ["linux-x64", "@bamti/cli-linux-x64"],
  ["linux-arm64", "@bamti/cli-linux-arm64"],
  ["darwin-x64", "@bamti/cli-darwin-x64"],
  ["darwin-arm64", "@bamti/cli-darwin-arm64"],
  ["win32-x64", "@bamti/cli-win32-x64"],
]);

export class UnsupportedPlatformError extends Error {
  constructor(platform, arch) {
    super(`bamti-cli does not support ${platform}-${arch}.`);
    this.name = "UnsupportedPlatformError";
  }
}

export class ArtifactNotFoundError extends Error {
  constructor(platform, arch, packageName, cause) {
    super(
      `Could not find ${packageName} for ${platform}-${arch}. Reinstall bamti-cli with optional dependencies enabled, or install ${packageName} explicitly.`,
      { cause },
    );
    this.name = "ArtifactNotFoundError";
  }
}

export function artifactPackage(platform = process.platform, arch = process.arch) {
  const packageName = ARTIFACTS.get(`${platform}-${arch}`);
  if (!packageName) {
    throw new UnsupportedPlatformError(platform, arch);
  }
  return packageName;
}

export function resolveBinary({
  platform = process.platform,
  arch = process.arch,
  resolvePackage = resolveFromHere,
} = {}) {
  const packageName = artifactPackage(platform, arch);
  let manifest;

  try {
    manifest = resolvePackage(`${packageName}/package.json`);
  } catch (cause) {
    throw new ArtifactNotFoundError(platform, arch, packageName, cause);
  }

  const binary = join(dirname(manifest), "bin", platform === "win32" ? "bamts.exe" : "bamts");
  if (!existsSync(binary)) {
    throw new ArtifactNotFoundError(platform, arch, packageName);
  }
  return binary;
}

export function run(args = [], options = {}) {
  if (!Array.isArray(args)) {
    throw new TypeError("bamti-cli run() expects an array of command-line arguments.");
  }

  const binary = resolveBinary(options);
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, {
      cwd: options.cwd,
      env: options.env,
      stdio: options.stdio ?? "inherit",
    });

    child.once("error", reject);
    child.once("exit", (code) => resolve(code ?? 1));
  });
}
