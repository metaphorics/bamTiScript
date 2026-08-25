import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const NATIVE_RELEASE_TARGETS = Object.freeze([
  "aarch64-apple-darwin",
  "aarch64-unknown-linux-gnu",
  "x86_64-apple-darwin",
  "x86_64-pc-windows-msvc",
  "x86_64-unknown-linux-gnu",
]);

const STRING_FIELDS = [
  "sourceCommit",
  "sourceVersion",
  "cargoLockSha256",
  "workflowRevision",
  "toolchain",
  "builderRevision",
];
const INPUT_FIELDS = new Set([...STRING_FIELDS, "targets"]);

function invalid(message) {
  throw new Error(`invalid native build plan: ${message}`);
}

export function createNativeBuildPlan(input) {
  if (input === null || typeof input !== "object" || Array.isArray(input)) {
    invalid("input must be an object");
  }
  const keys = Object.keys(input);
  if (keys.some((key) => !INPUT_FIELDS.has(key)) || keys.length !== INPUT_FIELDS.size) {
    invalid("input must contain exactly the build-plan fields");
  }
  for (const field of STRING_FIELDS) {
    if (typeof input[field] !== "string" || input[field] === "") {
      invalid(`${field} must be a non-empty string`);
    }
  }
  if (!Array.isArray(input.targets)) {
    invalid("targets must be an array");
  }
  const targets = [...new Set(input.targets)].sort();
  if (
    targets.length !== NATIVE_RELEASE_TARGETS.length ||
    targets.some((target, index) => target !== NATIVE_RELEASE_TARGETS[index])
  ) {
    invalid("targets must be the exact native release target set");
  }

  const plan = {
    sourceCommit: input.sourceCommit,
    sourceVersion: input.sourceVersion,
    cargoLockSha256: input.cargoLockSha256,
    workflowRevision: input.workflowRevision,
    toolchain: input.toolchain,
    builderRevision: input.builderRevision,
    targets,
  };
  const canonicalPlan = `${JSON.stringify(plan)}\n`;
  const buildSetId = createHash("sha256").update(canonicalPlan).digest("hex");
  return { plan, canonicalPlan, buildSetId };
}

async function main(argv) {
  if (argv.length !== 2) {
    throw new Error("usage: native-build-plan.mjs <input.json> <canonical-plan.json>");
  }
  const input = JSON.parse(await readFile(resolve(argv[0]), "utf8"));
  const { canonicalPlan, buildSetId } = createNativeBuildPlan(input);
  await writeFile(resolve(argv[1]), canonicalPlan);
  process.stdout.write(`${buildSetId}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
