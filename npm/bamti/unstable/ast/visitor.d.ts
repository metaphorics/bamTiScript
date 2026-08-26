import type { JsonValue, RequestOptions, RpcParams, Session } from "../../index.js";

export function visitSourceFile<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function visitNode<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
