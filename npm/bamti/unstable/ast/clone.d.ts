import type { JsonValue, RequestOptions, RpcParams, Session } from "../../index.js";

export function cloneNode<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function cloneNodeWithId<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
