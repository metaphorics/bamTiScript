export const cloneNode = (session, params, options) =>
  session.request("ast/clone", params, options);
export const cloneNodeWithId = (session, params, options) =>
  session.request("ast/cloneWithId", params, options);
