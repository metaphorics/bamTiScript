import type { JsonValue, RequestOptions, RpcParams, Session } from "../../index.js";

export function nodeIs<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
