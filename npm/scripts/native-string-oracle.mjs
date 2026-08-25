import { mkdtempSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const loneHigh = "\ud800";
const loneLow = "\udc00";
const astral = "\ud83d\ude00";
const composed = "\u00e9";
const decomposed = "e\u0301";

function codeUnits(value) {
  return Array.from({ length: value.length }, (_, index) => value.charCodeAt(index));
}

function errorRecord(error) {
  if (!error) {
    return null;
  }
  return { name: error.name, code: error.code, message: error.message };
}

function thrown(label, run) {
  try {
    const result = run();
    return {
      label,
      threw: false,
      spawnError: errorRecord(result.error),
      status: result.status,
    };
  } catch (error) {
    return { label, threw: true, error: errorRecord(error) };
  }
}

const childSource = String.raw`
import { basename } from "node:path";
const units = (value) => Array.from({ length: value.length }, (_, index) => value.charCodeAt(index));
const environment = Object.entries(process.env)
  .filter(([key]) => key.includes("BAMTS") || key.toLowerCase() === "path")
  .sort(([left], [right]) => left.localeCompare(right))
  .map(([key, value]) => ({ key: units(key), value: units(value) }));
process.stdout.write(JSON.stringify({
  argv: process.argv.slice(1).map(units),
  cwd: units(basename(process.cwd())),
  environment,
}));
`;

function childArguments(values) {
  return ["--input-type=module", "-e", childSource, ...values];
}

function spawnObservation(cwd, env, args) {
  try {
    const result = spawnSync(process.execPath, childArguments(args), {
      cwd,
      env,
      encoding: "utf8",
    });
    let child = null;
    let parseError = null;
    if (result.status === 0) {
      try {
        child = JSON.parse(result.stdout);
      } catch (error) {
        parseError = errorRecord(error);
      }
    }
    return {
      status: result.status,
      signal: result.signal,
      spawnError: errorRecord(result.error),
      stderr: result.stderr || null,
      parseError,
      child,
    };
  } catch (error) {
    return { threw: true, error: errorRecord(error) };
  }
}

const root = mkdtempSync(join(tmpdir(), "bamti-native-string-oracle-"));
try {
  const argumentVectors = [
    `high-${loneHigh}`,
    `low-${loneLow}`,
    `astral-${astral}`,
    "",
    "with space",
    'quote"inside',
    "trailing\\",
    'slash\\"quote',
  ];
  const environment = {
    [`BAMTS_HIGH_KEY_${loneHigh}`]: "high-key-value",
    [`BAMTS_LOW_KEY_${loneLow}`]: "low-key-value",
    [`BAMTS_ASTRAL_KEY_${astral}`]: "astral-key-value",
    BAMTS_HIGH_VALUE: `high-${loneHigh}`,
    BAMTS_LOW_VALUE: `low-${loneLow}`,
    BAMTS_ASTRAL_VALUE: `astral-${astral}`,
    BAMTS_EMPTY: "",
    BAMTS_UNDEFINED: undefined,
    "BAMTS_EQ=KEY": "equals-key-value",
    "=BAMTS_LEADING_EQ": "leading-equals-value",
    PATH: "upper-path",
    Path: "mixed-path",
  };

  const directoryVectors = [
    { label: "lone-high", name: `cwd-${loneHigh}` },
    { label: "lone-low", name: `cwd-${loneLow}` },
    { label: "astral", name: `cwd-${astral}` },
    { label: "composed", name: `cwd-${composed}` },
    { label: "decomposed", name: `cwd-${decomposed}` },
  ];
  const directories = [];
  for (const vector of directoryVectors) {
    const path = join(root, vector.name);
    let creationError = null;
    try {
      mkdirSync(path);
    } catch (error) {
      creationError = errorRecord(error);
    }
    directories.push({
      label: vector.label,
      requested: codeUnits(vector.name),
      creationError,
      spawn: creationError ? null : spawnObservation(path, environment, argumentVectors),
    });
  }

  const rejection = [
    thrown("argument-nul", () => spawnSync(process.execPath, ["-e", "", "a\u0000b"])),
    thrown("cwd-nul", () => spawnSync(process.execPath, ["-e", ""], { cwd: `${root}\u0000x` })),
    thrown("environment-key-equals", () =>
      spawnSync(process.execPath, ["-e", ""], { env: { "A=B": "value" } }),
    ),
    thrown("environment-key-nul", () =>
      spawnSync(process.execPath, ["-e", ""], { env: { "A\u0000B": "value" } }),
    ),
    thrown("environment-value-nul", () =>
      spawnSync(process.execPath, ["-e", ""], { env: { A: "value\u0000tail" } }),
    ),
  ];

  let directoryEntries = null;
  let directoryEntriesError = null;
  try {
    directoryEntries = readdirSync(root).map(codeUnits);
  } catch (error) {
    directoryEntriesError = errorRecord(error);
  }

  const report = {
    node: process.version,
    platform: process.platform,
    arch: process.arch,
    requestedArguments: argumentVectors.map(codeUnits),
    requestedEnvironment: Object.entries(environment).map(([key, value]) => ({
      key: codeUnits(key),
      value: value === undefined ? null : codeUnits(value),
    })),
    directories,
    directoryEntries,
    directoryEntriesError,
    rejection,
  };
  process.stdout.write(`BAMTS_NATIVE_STRING_ORACLE=${JSON.stringify(report)}\n`);
} finally {
  rmSync(root, { recursive: true, force: true });
}
