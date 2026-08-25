import { createHash } from "node:crypto";
import { chmodSync, mkdtempSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";

const GENERATED_TABLE = "./native-release-table.json";
const NODE_ENGINE = ">=24";
const TABLE_VERSION = 1;
const RELEASE_STRING_KEYS = [
  "packageVersion",
  "sourceCommit",
  "buildSetId",
  "releaseId",
];
const RELEASE_NUMBER_KEYS = ["nativeAbi", "cliProtocol"];
const STAGED_ADDON_FILE = "bamti-native-addon.node";

export const NATIVE_TARGETS = Object.freeze([
  Object.freeze({
    selector: "linux-x64-gnu",
    target: "x86_64-unknown-linux-gnu",
    package: "@bamti/bamti-linux-x64-gnu",
    entry: "bamti.linux-x64-gnu.node",
    os: "linux",
    cpu: "x64",
    libc: "glibc",
    artifactKind: "native-addon",
  }),
  Object.freeze({
    selector: "linux-arm64-gnu",
    target: "aarch64-unknown-linux-gnu",
    package: "@bamti/bamti-linux-arm64-gnu",
    entry: "bamti.linux-arm64-gnu.node",
    os: "linux",
    cpu: "arm64",
    libc: "glibc",
    artifactKind: "native-addon",
  }),
  Object.freeze({
    selector: "darwin-x64",
    target: "x86_64-apple-darwin",
    package: "@bamti/bamti-darwin-x64",
    entry: "bamti.darwin-x64.node",
    os: "darwin",
    cpu: "x64",
    artifactKind: "native-addon",
  }),
  Object.freeze({
    selector: "darwin-arm64",
    target: "aarch64-apple-darwin",
    package: "@bamti/bamti-darwin-arm64",
    entry: "bamti.darwin-arm64.node",
    os: "darwin",
    cpu: "arm64",
    artifactKind: "native-addon",
  }),
  Object.freeze({
    selector: "win32-x64-msvc",
    target: "x86_64-pc-windows-msvc",
    package: "@bamti/bamti-win32-x64-msvc",
    entry: "bamti.win32-x64-msvc.node",
    os: "win32",
    cpu: "x64",
    artifactKind: "native-addon",
  }),
]);

const SUPPORTED_SELECTORS = NATIVE_TARGETS.map(({ selector }) => selector).join(", ");

export class UnsupportedPlatformError extends Error {
  constructor(platform, arch, libc) {
    const observedLibc = platform === "linux" ? (libc ?? "unknown") : "n/a";
    super(
      `Unsupported native platform ${platform}-${arch} (libc: ${observedLibc}); supported selectors: ${SUPPORTED_SELECTORS}`,
    );
    this.name = "UnsupportedPlatformError";
  }
}

export class NativeArtifactNotFoundError extends Error {
  constructor(packageName, version, cause) {
    super(
      `Native artifact ${packageName}@${version} is not installed. Reinstall with optional dependencies enabled or install ${packageName}@${version} exactly.`,
      { cause },
    );
    this.name = "NativeArtifactNotFoundError";
  }
}

export class NativeArtifactLoadError extends Error {
  constructor(message, cause) {
    super(message, { cause });
    this.name = "NativeArtifactLoadError";
  }
}

function linuxLibc(report = process.report?.getReport?.()) {
  const version = report?.header?.glibcVersionRuntime;
  return typeof version === "string" && version.length > 0 ? "glibc" : "unknown";
}

export function selectNativeTarget({ platform, arch, libc }) {
  const normalizedLibc = platform === "linux" ? (libc ?? "unknown") : undefined;
  const target = NATIVE_TARGETS.find(
    (candidate) =>
      candidate.os === platform &&
      candidate.cpu === arch &&
      (platform !== "linux" || candidate.libc === normalizedLibc),
  );
  if (!target) {
    throw new UnsupportedPlatformError(platform, arch, normalizedLibc);
  }
  return target;
}

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function equalRelease(actual, expected) {
  return (
    isRecord(actual) &&
    RELEASE_STRING_KEYS.every((key) => actual[key] === expected[key]) &&
    RELEASE_NUMBER_KEYS.every((key) => actual[key] === expected[key])
  );
}

function canonicalReleaseId(release) {
  return `bamti/${release.packageVersion}/${release.sourceCommit}/native-abi-${release.nativeAbi}/cli-protocol-${release.cliProtocol}/${release.buildSetId}`;
}

function validateRelease(release) {
  if (
    !isRecord(release) ||
    RELEASE_STRING_KEYS.some(
      (key) => typeof release[key] !== "string" || release[key] === "",
    ) ||
    RELEASE_NUMBER_KEYS.some(
      (key) => !Number.isSafeInteger(release[key]) || release[key] < 1,
    )
  ) {
    throw new Error("generated release table has an incomplete release tuple");
  }
  if (release.releaseId !== canonicalReleaseId(release)) {
    throw new Error("generated release table has a non-canonical releaseId");
  }
}

function validateTable(table, selected) {
  if (!isRecord(table) || table.version !== TABLE_VERSION || !Array.isArray(table.targets)) {
    throw new Error("generated native release table has an unsupported schema");
  }
  validateRelease(table.release);
  if (table.targets.length !== NATIVE_TARGETS.length) {
    throw new Error("generated native release table must contain exactly five targets");
  }
  const selectors = new Set();
  for (const row of table.targets) {
    if (!isRecord(row) || typeof row.selector !== "string" || selectors.has(row.selector)) {
      throw new Error("generated native release table contains an invalid or duplicate selector");
    }
    selectors.add(row.selector);
    const fixed = NATIVE_TARGETS.find(({ selector }) => selector === row.selector);
    if (!fixed) {
      throw new Error(`generated native release table contains unsupported selector ${row.selector}`);
    }
    for (const key of ["target", "package", "entry", "os", "cpu", "artifactKind"]) {
      if (row[key] !== fixed[key]) {
        throw new Error(`generated native release table has wrong ${key} for ${row.selector}`);
      }
    }
    if (row.libc !== fixed.libc) {
      throw new Error(`generated native release table has wrong libc for ${row.selector}`);
    }
    if (row.version !== table.release.packageVersion) {
      throw new Error(`generated native release table has wrong version for ${row.selector}`);
    }
    if (!/^[0-9a-f]{64}$/.test(row.sha256)) {
      throw new Error(`generated native release table has invalid sha256 for ${row.selector}`);
    }
  }
  if (NATIVE_TARGETS.some(({ selector }) => !selectors.has(selector))) {
    throw new Error("generated native release table is missing a supported selector");
  }
  return table.targets.find(({ selector }) => selector === selected.selector);
}

function sameArray(actual, expected) {
  return Array.isArray(actual) && actual.length === 1 && actual[0] === expected;
}

function validateManifest(manifest, row, release) {
  if (!isRecord(manifest)) {
    throw new Error("package metadata is not an object");
  }
  if (manifest.name !== row.package) throw new Error("package metadata name does not match release table");
  if (manifest.version !== row.version) throw new Error("package metadata version does not match release table");
  if (manifest.main !== row.entry) throw new Error("package metadata main does not match release table");
  if (!isRecord(manifest.engines) || manifest.engines.node !== NODE_ENGINE) {
    throw new Error(`package metadata must require Node ${NODE_ENGINE}`);
  }
  if (!sameArray(manifest.os, row.os) || !sameArray(manifest.cpu, row.cpu)) {
    throw new Error("package metadata host constraints do not match release table");
  }
  if (row.libc === undefined ? manifest.libc !== undefined : !sameArray(manifest.libc, row.libc)) {
    throw new Error("package metadata libc constraint does not match release table");
  }
  const metadata = manifest.bamtiNative;
  if (!isRecord(metadata)) throw new Error("package metadata is missing bamtiNative");
  for (const key of ["entry", "target", "artifactKind", "sha256"]) {
    if (metadata[key] !== row[key]) throw new Error(`package metadata ${key} does not match release table`);
  }
  if (!equalRelease(metadata.release, release)) {
    throw new Error("package metadata release tuple does not match release table");
  }
}

function containedPath(packageRoot, entry, realpath) {
  if (typeof entry !== "string" || entry === "" || isAbsolute(entry)) {
    throw new Error("native entry must be a non-empty relative path");
  }
  const candidate = resolve(packageRoot, entry);
  const lexical = relative(packageRoot, candidate);
  if (lexical === ".." || lexical.startsWith(`..${sep}`) || isAbsolute(lexical)) {
    throw new Error("native entry escapes its package root");
  }
  const realRoot = realpath(packageRoot);
  const realEntry = realpath(candidate);
  const physical = relative(realRoot, realEntry);
  if (physical === ".." || physical.startsWith(`..${sep}`) || isAbsolute(physical)) {
    throw new Error("native entry escapes its package root through a symlink");
  }
  return realEntry;
}

function validateAddon(addon, row, release) {
  if (!isRecord(addon) || typeof addon.releaseMetadata !== "function") {
    throw new Error("native addon must export releaseMetadata()");
  }
  if (typeof addon.run !== "function") {
    throw new Error("native addon must export run()");
  }
  const exports = Object.keys(addon).sort();
  if (
    exports.length !== 2 ||
    exports[0] !== "releaseMetadata" ||
    exports[1] !== "run"
  ) {
    throw new Error("native addon must export exactly releaseMetadata and run");
  }
  const metadata = addon.releaseMetadata();
  if (!isRecord(metadata)) throw new Error("native addon releaseMetadata() returned a non-object");
  if (
    !equalRelease(metadata, release) ||
    metadata.target !== row.target ||
    metadata.artifactKind !== row.artifactKind
  ) {
    throw new Error("native addon release metadata does not match release table");
  }
}

function parseJson(bytes, label) {
  try {
    return JSON.parse(Buffer.isBuffer(bytes) ? bytes.toString("utf8") : String(bytes));
  } catch (cause) {
    throw new Error(`${label} is not valid JSON`, { cause });
  }
}

function defaultStage(bytes) {
  const dir = mkdtempSync(join(tmpdir(), "bamti-native-XXXXXX"));
  chmodSync(dir, 0o700);
  const path = join(dir, STAGED_ADDON_FILE);
  writeFileSync(path, bytes, { flag: "wx", mode: 0o700 });
  return path;
}

function loadVerified({ table, selected, host, resolver, readFile, realpath, requireAddon, cache, stage = defaultStage }) {
  if (cache.value !== undefined) return cache.value;
  let resolvedTable;
  let row;
  try {
    resolvedTable =
      typeof table === "string"
        ? parseJson(readFile(resolver.resolve(table)), "generated native release table")
        : table;
    row = validateTable(resolvedTable, selected);
    const hostMajor = Number.parseInt(host.nodeVersion.split(".", 1)[0], 10);
    if (!Number.isInteger(hostMajor) || hostMajor < 24) {
      throw new Error(`Node ${NODE_ENGINE} is required`);
    }
    const napiVersion = Number.parseInt(host.napiVersion, 10);
    if (!Number.isInteger(napiVersion) || napiVersion < 10) {
      throw new Error("Node-API 10 or later is required");
    }
  } catch (cause) {
    throw new NativeArtifactLoadError("Native release table or host validation failed", cause);
  }

  let manifestPath;
  try {
    manifestPath = resolver.resolve(`${row.package}/package.json`);
  } catch (cause) {
    throw new NativeArtifactNotFoundError(row.package, row.version, cause);
  }
  if (!isAbsolute(manifestPath)) {
    throw new NativeArtifactLoadError(
      `Failed to load native artifact ${row.package}@${row.version}`,
      new Error("native package manifest path must be absolute"),
    );
  }

  let addon;
  try {
    const manifest = parseJson(readFile(manifestPath), `${row.package}/package.json`);
    validateManifest(manifest, row, resolvedTable.release);
    const addonPath = containedPath(dirname(manifestPath), row.entry, realpath);
    const bytes = readFile(addonPath);
    const digest = createHash("sha256").update(bytes).digest("hex");
    if (digest !== row.sha256) throw new Error("native addon SHA-256 does not match release table");
    const stagedPath = stage(bytes);
    addon = requireAddon(stagedPath);
    validateAddon(addon, row, resolvedTable.release);
  } catch (cause) {
    throw new NativeArtifactLoadError(`Failed to load native artifact ${row.package}@${row.version}`, cause);
  }
  cache.value = addon;
  return addon;
}

let productionCache;

function productionOptions() {
  const resolver = createRequire(import.meta.url);
  const platform = process.platform;
  return {
    table: GENERATED_TABLE,
    host: {
      platform,
      arch: process.arch,
      libc: platform === "linux" ? linuxLibc() : undefined,
      nodeVersion: process.versions.node,
      napiVersion: process.versions.napi,
    },
    resolver,
    readFile: readFileSync,
    realpath: realpathSync,
    requireAddon: resolver,
    stage: defaultStage,
    cache: {
      get value() {
        return productionCache;
      },
      set value(value) {
        productionCache = value;
      },
    },
  };
}

export function loadNativeAddon(options) {
  const dependencies = options === undefined ? productionOptions() : options;
  if (!isRecord(dependencies) || !isRecord(dependencies.host)) {
    throw new TypeError("loader options must provide explicit test dependencies");
  }
  const selected = selectNativeTarget(dependencies.host);
  return loadVerified({ ...dependencies, selected });
}
