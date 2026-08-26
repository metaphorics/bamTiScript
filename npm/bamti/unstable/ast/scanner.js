export const scan = (session, params, options) =>
  session.request("ast/scanner/scan", params, options);
