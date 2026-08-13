import assert from "node:assert/strict";
import { test } from "node:test";

import {
  NATIVE_RELEASE_TARGETS,
  createNativeBuildPlan,
} from "../scripts/native-build-plan.mjs";

const input = {
  sourceCommit: "0123456789abcdef0123456789abcdef01234567",
  sourceVersion: "0.2.0",
  cargoLockSha256: "a".repeat(64),
  workflowRevision: "b".repeat(64),
  toolchain: "1.97.1",
  builderRevision: "c".repeat(64),
  targets: [...NATIVE_RELEASE_TARGETS].reverse(),
};

test("canonical native build plans are ordered and deterministic", () => {
  const first = createNativeBuildPlan(input);
  const second = createNativeBuildPlan({ ...input, targets: [...NATIVE_RELEASE_TARGETS] });

  assert.deepEqual(first, second);
  assert.deepEqual(first.plan.targets, NATIVE_RELEASE_TARGETS);
  assert.equal(first.canonicalPlan, `${JSON.stringify(first.plan)}\n`);
  assert.match(first.buildSetId, /^[0-9a-f]{64}$/);
});

test("native build plans reject incomplete or inexact target sets", () => {
  assert.throws(
    () => createNativeBuildPlan({ ...input, builderRevision: "" }),
    /builderRevision must be a non-empty string/,
  );
  assert.throws(
    () => createNativeBuildPlan({ ...input, targets: NATIVE_RELEASE_TARGETS.slice(1) }),
    /targets must be the exact native release target set/,
  );
});
