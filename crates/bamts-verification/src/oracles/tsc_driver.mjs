#!/usr/bin/env node
/**
 * bamti TypeScript oracle driver.
 *
 * Original public-API / protocol adapter. Speaks `bamti.oracle.tsc/v1` on an
 * exact argv surface and never parses human tsc diagnostics. Unit tests inject
 * a fake process boundary and do not execute this file.
 *
 * Argv (exact):
 *   --protocol bamti.oracle.tsc/v1 --request-file <path>
 *
 * stdout: one DriverResponse JSON object
 * stderr: evidence only
 */

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join, posix } from "node:path";
import { createHash } from "node:crypto";
import { pathToFileURL } from "node:url";

const PROTOCOL = "bamti.oracle.tsc/v1";

function fail(message, code = 2) {
  process.stderr.write(JSON.stringify({ protocol: PROTOCOL, error: message }) + "\n");
  process.exit(code);
}

function parseArgv(argv) {
  if (
    argv.length !== 4 ||
    argv[0] !== "--protocol" ||
    argv[1] !== PROTOCOL ||
    argv[2] !== "--request-file"
  ) {
    fail("exact argv required: --protocol bamti.oracle.tsc/v1 --request-file <path>");
  }
  return argv[3];
}

function sha256Hex(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function requireObject(value, label) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    fail(`${label} must be an object`);
  }
  return value;
}

function loadRequest(path) {
  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    fail(`cannot read request file: ${error.message}`);
  }
  let request;
  try {
    request = JSON.parse(text);
  } catch (error) {
    fail(`request is not JSON: ${error.message}`);
  }
  requireObject(request, "request");
  if (request.protocol !== PROTOCOL) {
    fail(`unsupported protocol ${JSON.stringify(request.protocol)}`);
  }
  if (request.phase !== "parse" && request.phase !== "check") {
    fail("phase must be parse or check");
  }
  if (!Array.isArray(request.files) || request.files.length === 0) {
    fail("files must be a nonempty array");
  }
  if (!Array.isArray(request.observables) || request.observables.length === 0) {
    fail("observables must be a nonempty array");
  }
  return request;
}

function diagnosticFromApi(entry, phase) {
  const fileName = entry.fileName ?? null;
  const hasFile = typeof fileName === "string" && fileName.length > 0;
  const diagnostic = {
    code: entry.code,
    category: categoryName(entry.category),
    message: entry.text,
    phase,
  };
  if (hasFile) {
    diagnostic.file = posix.normalize(fileName).replaceAll("\\", "/");
    diagnostic.span = { start: entry.pos, end: entry.end };
  }
  if (Array.isArray(entry.relatedInformation) && entry.relatedInformation.length > 0) {
    diagnostic.related = entry.relatedInformation.map((related) =>
      diagnosticFromApi(related, phase),
    );
  }
  if (Array.isArray(entry.messageChain) && entry.messageChain.length > 0) {
    diagnostic.message_chain = entry.messageChain.map((chain) =>
      diagnosticFromApi(chain, phase),
    );
  }
  return diagnostic;
}

function categoryName(category) {
  switch (category) {
    case 0:
      return "warning";
    case 1:
      return "error";
    case 2:
      return "suggestion";
    case 3:
      return "message";
    default:
      fail(`unknown diagnostic category ${category}`);
  }
}

function writeSetDigest(artifacts) {
  const rows = [];
  for (const kind of ["javascript", "declaration", "source_map", "trace", "build_info"]) {
    const table = artifacts[kind];
    if (!table) {
      continue;
    }
    for (const [path, digest] of Object.entries(table).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
      rows.push(`${kind}\0${path}\0${digest}`);
    }
  }
  return sha256Hex(Buffer.from(rows.join("\n"), "utf8"));
}

async function compileWithPublicApi(request, projectRoot) {
  let apiModule;
  try {
    apiModule = await import("typescript/unstable/sync");
  } catch (error) {
    fail(`typescript public API is unavailable: ${error.message}`);
  }
  const { API } = apiModule;
  const tsserverPath = process.env.BAMTS_STABLE_TSC;
  if (!tsserverPath) {
    fail("BAMTS_STABLE_TSC must name the pinned tsc binary; PATH lookup is forbidden");
  }
  const api = new API({ tsserverPath, cwd: projectRoot });
  const snapshot = api.updateSnapshot({
    openProjects: [join(projectRoot, "tsconfig.json")],
  });
  const project = snapshot.projects[0];
  const phase = request.phase;
  const syntactic = [...project.getSyntacticDiagnostics()];
  const semantic = phase === "check" ? [...project.getSemanticDiagnostics()] : [];
  const diagnostics = [
    ...syntactic.map((entry) => diagnosticFromApi(entry, "parse")),
    ...semantic.map((entry) => diagnosticFromApi(entry, "check")),
  ];
  const artifacts = {};
  const declared = new Set(request.observables);
  if (declared.has("javascript") || declared.has("declaration") || declared.has("source_map")
    || declared.has("trace") || declared.has("build_info") || declared.has("write_set")) {
    const emitted = project.emit();
    const kindOf = (fileName) => {
      if (fileName.endsWith(".d.ts")) return "declaration";
      if (fileName.endsWith(".d.ts.map") || fileName.endsWith(".js.map")) return "source_map";
      if (fileName.endsWith(".tsbuildinfo")) return "build_info";
      if (fileName.endsWith(".trace.json") || fileName.endsWith(".trace")) return "trace";
      return "javascript";
    };
    for (const file of emitted.outputFiles ?? []) {
      const kind = kindOf(file.name);
      if (!declared.has(kind) && !declared.has("write_set")) {
        continue;
      }
      artifacts[kind] ??= {};
      artifacts[kind][file.name] = sha256Hex(Buffer.from(file.text, "utf8"));
    }
    if (declared.has("write_set")) {
      artifacts.write_set = writeSetDigest(artifacts);
    }
  }
  snapshot.dispose?.();
  api.dispose?.();
  return { protocol: PROTOCOL, phase, diagnostics, artifacts };
}

function writeTsconfig(projectRoot, request) {
  const config = {
    compilerOptions: { ...(request.options ?? {}), skipLibCheck: true },
    files: request.files.map((file) => file.path),
  };
  mkdirSync(projectRoot, { recursive: true });
  writeFileSync(join(projectRoot, "tsconfig.json"), JSON.stringify(config), "utf8");
}

const requestPath = parseArgv(process.argv.slice(2));
const request = loadRequest(requestPath);
const projectRoot = dirname(requestPath);
writeTsconfig(projectRoot, request);

const response = await compileWithPublicApi(request, projectRoot);
process.stdout.write(JSON.stringify(response));
void pathToFileURL;
