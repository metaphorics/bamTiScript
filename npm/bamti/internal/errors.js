/**
 * Typed failures raised by the `bamti` host surface.
 *
 * Every error here describes a *host transport* condition: the artifact could
 * not be located, the framing was violated, the compiler process died, or the
 * session was disposed. Compiler diagnostics are ordinary result data and never
 * appear as an exception; the Rust service owns that distinction and this
 * package does not reinterpret it.
 */

/** Base class for every failure this package raises. */
export class BamtiError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = "BamtiError";
  }
}

/** The peer violated the Content-Length framing contract. */
export class ProtocolError extends BamtiError {
  constructor(message, options) {
    super(message, options);
    this.name = "ProtocolError";
  }
}

/** The in-flight request bound was reached; the caller must retry later. */
export class TransportBusyError extends BamtiError {
  constructor(inFlight, maxInFlight) {
    super(
      `bamti transport already has ${inFlight} of ${maxInFlight} permitted in-flight requests.`,
    );
    this.name = "TransportBusyError";
    this.inFlight = inFlight;
    this.maxInFlight = maxInFlight;
  }
}

/** The compiler process exited while requests were outstanding. */
export class TransportCrashError extends BamtiError {
  constructor({ code = null, signal = null, stderr = "", cause } = {}) {
    super(
      `bamti compiler process exited unexpectedly (code ${code ?? "none"}, signal ${signal ?? "none"}).` +
        (stderr ? ` Captured stderr: ${stderr}` : ""),
      cause ? { cause } : undefined,
    );
    this.name = "TransportCrashError";
    this.code = code;
    this.signal = signal;
    this.stderr = stderr;
  }
}

/** The process died more often than the finite restart bound allows. */
export class RestartLimitError extends BamtiError {
  constructor(restarts, maxRestarts, options) {
    super(
      `bamti compiler process failed ${restarts} times; the transport permits at most ${maxRestarts} restarts.`,
      options,
    );
    this.name = "RestartLimitError";
    this.restarts = restarts;
    this.maxRestarts = maxRestarts;
  }
}

/** The transport or session was disposed. */
export class DisposedError extends BamtiError {
  constructor(what = "transport") {
    super(`bamti ${what} has been disposed.`);
    this.name = "DisposedError";
  }
}

/** The Rust service rejected a request. Carries the service error payload. */
export class ServiceRequestError extends BamtiError {
  constructor(method, { code = null, message = "", data = undefined } = {}) {
    super(`bamti ${method} failed: ${message || `service error ${code ?? "unknown"}`}`);
    this.name = "ServiceRequestError";
    this.method = method;
    this.code = code;
    this.data = data;
  }
}

/** The standard abort rejection, so `AbortSignal` users can compare `name`. */
export function abortError(reason) {
  if (reason instanceof DOMException && reason.name === "AbortError") return reason;
  const error = new DOMException("The operation was aborted.", "AbortError");
  if (reason !== undefined) Object.defineProperty(error, "cause", { value: reason });
  return error;
}
