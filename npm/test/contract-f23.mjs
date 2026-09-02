// F2.3 Contract: public AST, is, factory, utils, scanner, visitor, and clone
// export behavior.
//
// Asserts all 7 ast subpaths import and expose the expected 19 operations,
// and that each operation is a function that dispatches to the session transport.

import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const npmRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

let pass = 0;
const fail = [];

function check(name, fn) {
  try {
    fn();
    pass++;
  } catch (err) {
    fail.push(`${name}: ${err.message}`);
  }
}

// --- ast (index) subpath: 4 operations ---
const ast = await import("bamti/unstable/ast");
check("ast.nodeId is function", () => assert.ok(typeof ast.nodeId === "function"));
check("ast.nodeRange is function", () => assert.ok(typeof ast.nodeRange === "function"));
check("ast.syntaxKind is function", () => assert.ok(typeof ast.syntaxKind === "function"));
check("ast.nodeKind is function", () => assert.ok(typeof ast.nodeKind === "function"));

// --- is subpath: 1 operation ---
const isMod = await import("bamti/unstable/ast/is");
check("is.nodeIs is function", () => assert.ok(typeof isMod.nodeIs === "function"));

// --- factory subpath: 4 operations ---
const factory = await import("bamti/unstable/ast/factory");
check("factory.createNode is function", () => assert.ok(typeof factory.createNode === "function"));
check("factory.updateNode is function", () => assert.ok(typeof factory.updateNode === "function"));
check("factory.asNode is function", () => assert.ok(typeof factory.asNode === "function"));
check("factory.intoOwned is function", () => assert.ok(typeof factory.intoOwned === "function"));

// --- utils subpath: 5 operations ---
const utils = await import("bamti/unstable/ast/utils");
check("utils.textOfRange is function", () => assert.ok(typeof utils.textOfRange === "function"));
check("utils.nodeText is function", () => assert.ok(typeof utils.nodeText === "function"));
check("utils.containsRange is function", () => assert.ok(typeof utils.containsRange === "function"));
check("utils.containsPosition is function", () => assert.ok(typeof utils.containsPosition === "function"));
check("utils.narrowestContaining is function", () => assert.ok(typeof utils.narrowestContaining === "function"));

// --- scanner subpath: 1 operation ---
const scanner = await import("bamti/unstable/ast/scanner");
check("scanner.scan is function", () => assert.ok(typeof scanner.scan === "function"));

// --- visitor subpath: 2 operations ---
const visitor = await import("bamti/unstable/ast/visitor");
check("visitor.visitSourceFile is function", () => assert.ok(typeof visitor.visitSourceFile === "function"));
check("visitor.visitNode is function", () => assert.ok(typeof visitor.visitNode === "function"));

// --- clone subpath: 2 operations ---
const clone = await import("bamti/unstable/ast/clone");
check("clone.cloneNode is function", () => assert.ok(typeof clone.cloneNode === "function"));
check("clone.cloneNodeWithId is function", () => assert.ok(typeof clone.cloneNodeWithId === "function"));

// --- Total export count: 19 operations ---
check("total 19 operations across 7 modules", () => {
  const all = [
    ast.nodeId, ast.nodeRange, ast.syntaxKind, ast.nodeKind,
    isMod.nodeIs,
    factory.createNode, factory.updateNode, factory.asNode, factory.intoOwned,
    utils.textOfRange, utils.nodeText, utils.containsRange, utils.containsPosition, utils.narrowestContaining,
    scanner.scan,
    visitor.visitSourceFile, visitor.visitNode,
    clone.cloneNode, clone.cloneNodeWithId,
  ];
  assert.equal(all.length, 19, `expected 19 operations, got ${all.length}`);
  for (const fn of all) {
    assert.ok(typeof fn === "function", "all operations must be functions");
  }
});

// --- Dispatch probe: each operation calls session.request with the right method ---
check("dispatch probe: 19 ast method names", () => {
  const calls = [];
  const fakeSession = {
    request(method, params, options) {
      calls.push(method);
      return Promise.resolve(null);
    },
  };
  ast.nodeId(fakeSession, {});
  ast.nodeRange(fakeSession, {});
  ast.syntaxKind(fakeSession, {});
  ast.nodeKind(fakeSession, {});
  isMod.nodeIs(fakeSession, {});
  factory.createNode(fakeSession, {});
  factory.updateNode(fakeSession, {});
  factory.asNode(fakeSession, {});
  factory.intoOwned(fakeSession, {});
  utils.textOfRange(fakeSession, {});
  utils.nodeText(fakeSession, {});
  utils.containsRange(fakeSession, {});
  utils.containsPosition(fakeSession, {});
  utils.narrowestContaining(fakeSession, {});
  scanner.scan(fakeSession, {});
  visitor.visitSourceFile(fakeSession, {});
  visitor.visitNode(fakeSession, {});
  clone.cloneNode(fakeSession, {});
  clone.cloneNodeWithId(fakeSession, {});

  const expected = [
    "ast/id", "ast/range", "ast/syntaxKind", "ast/nodeKind",
    "ast/is",
    "ast/factory/create", "ast/factory/update", "ast/factory/asNode", "ast/factory/intoOwned",
    "ast/utils/textOfRange", "ast/utils/nodeText", "ast/utils/containsRange", "ast/utils/containsPosition", "ast/utils/narrowestContaining",
    "ast/scanner/scan",
    "ast/visitor/visitSourceFile", "ast/visitor/visitNode",
    "ast/clone", "ast/cloneWithId",
  ];
  assert.equal(calls.length, 19, `got ${calls.length} calls`);
  assert.deepEqual(calls, expected, `method mismatch: ${JSON.stringify(calls)}`);
});

// --- Report ---
const total = pass + fail.length;
process.stdout.write(`F2.3_CONTRACT ${pass}/${total} pass\n`);
if (fail.length > 0) {
  for (const f of fail) process.stdout.write(`FAIL: ${f}\n`);
  process.exit(1);
}
