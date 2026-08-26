export const nodeId = (session, params, options) =>
  session.request("ast/id", params, options);
export const nodeRange = (session, params, options) =>
  session.request("ast/range", params, options);
export const syntaxKind = (session, params, options) =>
  session.request("ast/syntaxKind", params, options);
export const nodeKind = (session, params, options) =>
  session.request("ast/nodeKind", params, options);
