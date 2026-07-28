import { existsSync } from "node:fs";
import { chmod, copyFile, cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";

const npmRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(npmRoot, "..");
const outputDirectory = resolve(npmRoot, "dist");
const platformArtifacts = new Map([
  ["x86_64-unknown-linux-gnu", { directory: "cli-linux-x64", binary: "bamts" }],
  ["aarch64-unknown-linux-gnu", { directory: "cli-linux-arm64", binary: "bamts" }],
  ["x86_64-apple-darwin", { directory: "cli-darwin-x64", binary: "bamts" }],
  ["aarch64-apple-darwin", { directory: "cli-darwin-arm64", binary: "bamts" }],
  ["x86_64-pc-windows-msvc", { directory: "cli-win32-x64", binary: "bamts.exe" }],
]);

function fail(message) {
  throw new Error(`bamti package build: ${message}`);
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, stdio: "inherit" });
  if (result.error) {
    fail(`could not start ${command}: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} failed with exit code ${result.status ?? "unknown"}.`);
  }
}

function parseTargets(argv) {
  if (argv.length === 2 && argv[0] === "--target") {
    return [argv[1]];
  }
  if (argv.length === 1 && argv[0] === "--all") {
    return [...platformArtifacts.keys()];
  }
  fail("use --target <rust-target-triple> or --all.");
}

function cargoTargetDirectory() {
  return process.env.CARGO_TARGET_DIR
    ? resolve(repositoryRoot, process.env.CARGO_TARGET_DIR)
    : join(repositoryRoot, "target");
}

async function packageTarget(target) {
  const artifact = platformArtifacts.get(target);
  if (!artifact) {
    fail(`unsupported target ${target}. Supported targets: ${[...platformArtifacts.keys()].join(", ")}.`);
  }

  run("cargo", ["build", "--release", "--target", target, "-p", "bamts-cli"], repositoryRoot);

  const binary = join(cargoTargetDirectory(), target, "release", artifact.binary);
  if (!existsSync(binary)) {
    fail(
      `cargo completed without ${binary}. The workspace must define a bamts binary target before an npm artifact can be packed.`,
    );
  }

  const stagingRoot = await mkdtemp(join(tmpdir(), "bamti-package-"));
  try {
    const packageDirectory = join(stagingRoot, artifact.directory);
    await cp(join(npmRoot, "artifacts", artifact.directory), packageDirectory, { recursive: true });
    const packagedBinary = join(packageDirectory, "bin", artifact.binary);
    await mkdir(dirname(packagedBinary), { recursive: true });
    await copyFile(binary, packagedBinary);
    if (process.platform !== "win32") {
      await chmod(packagedBinary, 0o755);
    }
    await mkdir(outputDirectory, { recursive: true });
    run(process.platform === "win32" ? "npm.cmd" : "npm", ["pack", "--pack-destination", outputDirectory], packageDirectory);
  } finally {
    await rm(stagingRoot, { recursive: true, force: true });
  }
}

for (const target of parseTargets(process.argv.slice(2))) {
  await packageTarget(target);
}
