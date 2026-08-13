import { loadNativeAddon } from "./native-loader.js";

const STDIO = new Set(["inherit", "ignore", "pipe"]);

function osString(value, label) {
  if (typeof value !== "string") {
    throw new TypeError(`${label} must be a string.`);
  }
  if (value.includes("\0")) {
    throw new TypeError(`${label} must not contain NUL bytes.`);
  }
  return Buffer.from(value, "utf8");
}

function environmentEntries(environment, platform) {
  if (environment === null || typeof environment !== "object") {
    throw new TypeError("bamti run() env must be an object.");
  }
  const entries = [];
  for (const key of Object.keys(environment)) {
    const raw = environment[key];
    if (raw === undefined) continue;
    if (key.includes("\0")) {
      throw new TypeError("bamti run() environment names must not contain NUL bytes.");
    }
    const value = String(raw);
    if (value.includes("\0")) {
      throw new TypeError(`bamti run() environment value for ${key} must not contain NUL bytes.`);
    }
    entries.push({ key, value });
  }
  if (platform === "win32") {
    entries.sort((left, right) =>
      left.key < right.key ? -1 : left.key > right.key ? 1 : 0,
    );
    const seen = new Set();
    return entries.filter(({ key }) => {
      const folded = key.toUpperCase();
      if (seen.has(folded)) return false;
      seen.add(folded);
      return true;
    });
  }
  return entries;
}

export function encodeRequest(args, options, processObject = process) {
  if (options === null || typeof options !== "object") {
    throw new TypeError("bamti run() options must be an object.");
  }
  const cwd = options.cwd === undefined ? processObject.cwd() : options.cwd;
  const environment = options.env === undefined ? processObject.env : options.env;
  const stdio = options.stdio ?? "inherit";
  if (!STDIO.has(stdio)) {
    throw new TypeError('bamti run() stdio must be "inherit", "ignore", or "pipe".');
  }
  return {
    request: {
      args: args.map((argument, index) =>
        osString(argument, `bamti run() argument ${index}`),
      ),
      cwd: osString(cwd, "bamti run() cwd"),
      env: environmentEntries(environment, processObject.platform).map(({ key, value }) =>
        Buffer.from(`${key}=${value}`, "utf8"),
      ),
    },
    stdio,
  };
}

function write(stream, bytes) {
  if (bytes.length === 0) return Promise.resolve(true);
  return new Promise((resolve) => {
    let settled = false;
    const finish = (success) => {
      if (settled) return;
      settled = true;
      stream.off("error", onError);
      resolve(success);
    };
    const onError = () => finish(false);
    stream.once("error", onError);
    try {
      stream.write(bytes, (error) => finish(error === undefined || error === null));
    } catch {
      finish(false);
    }
  });
}

export async function deliverOutcome(outcome, stdio, processObject = process) {
  if (stdio !== "inherit") return outcome.exitCode;
  if (!(await write(processObject.stderr, outcome.stderr))) return 1;
  if (!(await write(processObject.stdout, outcome.stdout))) return 1;
  return outcome.exitCode;
}

export function runNative(args = [], options = {}, dependencies = {}) {
  if (!Array.isArray(args)) {
    throw new TypeError("bamti run() expects an array of command-line arguments.");
  }
  const addon = (dependencies.loadNativeAddon ?? loadNativeAddon)();
  const processObject = dependencies.process ?? process;
  return new Promise((resolve, reject) => {
    try {
      const { request, stdio } = encodeRequest(args, options, processObject);
      resolve(
        Promise.resolve(addon.run(request)).then((outcome) =>
          deliverOutcome(outcome, stdio, processObject),
        ),
      );
    } catch (error) {
      reject(error);
    }
  });
}
