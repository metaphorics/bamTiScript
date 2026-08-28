import { SERVICE_METHODS, createSession } from "../internal/session.js";

export function createSerialService(options) {
  const session = createSession(options);
  let tail = Promise.resolve();
  const enqueue = (method, params = {}, requestOptions) => {
    const result = tail.then(() => session.request(method, params, requestOptions));
    tail = result.catch(() => undefined);
    return result;
  };
  return Object.assign(
    Object.fromEntries(
      SERVICE_METHODS.map((operation) => [
        operation,
        (params = {}, requestOptions) =>
          enqueue(`service/${operation}`, params, requestOptions),
      ]),
    ),
    {
      request: enqueue,
      dispose: () => {
        const result = tail.then(() => session.dispose());
        tail = result.catch(() => undefined);
        return result;
      },
    },
  );
}
