export const visitSourceFile = (session, params, options) =>
  session.request("ast/visitor/visitSourceFile", params, options);
export const visitNode = (session, params, options) =>
  session.request("ast/visitor/visitNode", params, options);
