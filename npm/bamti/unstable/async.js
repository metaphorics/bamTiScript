import { SERVICE_METHODS, createSession } from "../internal/session.js";

export function createAsyncService(options) {
  const session = createSession(options);
  return Object.assign(
    Object.fromEntries(
      SERVICE_METHODS.map((operation) => [
        operation,
        (params = {}, requestOptions) =>
          session.request(`service/${operation}`, { ...params, async: true }, requestOptions),
      ]),
    ),
    {
      request: session.request.bind(session),
      dispose: session.dispose.bind(session),
      [Symbol.asyncDispose]: session[Symbol.asyncDispose].bind(session),
    },
  );
}

export { Session, createSession } from "../internal/session.js";
