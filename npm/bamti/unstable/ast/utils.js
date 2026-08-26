export const textOfRange = (session, params, options) =>
  session.request("ast/utils/textOfRange", params, options);
export const nodeText = (session, params, options) =>
  session.request("ast/utils/nodeText", params, options);
export const containsRange = (session, params, options) =>
  session.request("ast/utils/containsRange", params, options);
export const containsPosition = (session, params, options) =>
  session.request("ast/utils/containsPosition", params, options);
export const narrowestContaining = (session, params, options) =>
  session.request("ast/utils/narrowestContaining", params, options);
