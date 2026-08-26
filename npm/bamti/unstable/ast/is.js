export const nodeIs = (session, params, options) =>
  session.request("ast/is", params, options);
