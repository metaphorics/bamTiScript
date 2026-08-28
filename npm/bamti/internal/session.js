import { DisposedError, abortError } from "./errors.js";
import { Transport } from "./transport.js";

export const SERVICE_METHODS = [
  "open",
  "update",
  "close",
  "snapshot",
  "completions",
  "definition",
  "quickInfo",
  "references",
  "rename",
  "diagnostics",
];

export class Session {
  #disposed = false;
  #initializedGeneration = 0;
  #initializing = null;

  constructor({ root, filesystem, transport, transportOptions } = {}) {
    if (root !== undefined && filesystem !== undefined) {
      throw new TypeError("Session options must not specify both root and filesystem.");
    }
    if (
      filesystem !== undefined &&
      (filesystem === null || filesystem.kind !== "os" || typeof filesystem.root !== "string")
    ) {
      throw new TypeError("Session filesystem must be an OS filesystem descriptor.");
    }
    this.transport = transport ?? new Transport(transportOptions);
    this.initializeParams = { root: filesystem?.root ?? root ?? process.cwd() };

    for (const operation of SERVICE_METHODS) {
      this[operation] = (params = {}, options) =>
        this.request(`service/${operation}`, params, options);
    }
  }

  async #initialize() {
    if (this.#disposed) throw new DisposedError("session");
    const generation = this.transport.ready();
    if (this.#initializedGeneration === generation) return;

    if (!this.#initializing || this.#initializing.generation !== generation) {
      const promise = this.transport
        .request("initialize", this.initializeParams)
        .then((result) => {
          if (this.transport.generation === generation) {
            this.#initializedGeneration = generation;
          }
          return result;
        })
        .finally(() => {
          if (this.#initializing?.promise === promise) this.#initializing = null;
        });
      this.#initializing = { generation, promise };
    }
    await this.#initializing.promise;
  }

  async request(method, params = {}, options) {
    if (options?.signal?.aborted) throw abortError(options.signal.reason);
    for (;;) {
      await this.#initialize();
      if (this.transport.ready() === this.#initializedGeneration) {
        return this.transport.request(method, params, options);
      }
    }
  }

  async dispose() {
    if (this.#disposed) return;
    this.#disposed = true;
    await this.transport.dispose();
  }

  async [Symbol.asyncDispose]() {
    await this.dispose();
  }
}

export function createSession(options) {
  return new Session(options);
}

export function serviceMethods(session) {
  return Object.fromEntries(
    SERVICE_METHODS.map((operation) => [
      operation,
      (params = {}, options) => session.request(`service/${operation}`, params, options),
    ]),
  );
}
