export const createNode = (session, params, options) =>
  session.request("ast/factory/create", params, options);
export const updateNode = (session, params, options) =>
  session.request("ast/factory/update", params, options);
export const asNode = (session, params, options) =>
  session.request("ast/factory/asNode", params, options);
export const intoOwned = (session, params, options) =>
  session.request("ast/factory/intoOwned", params, options);
