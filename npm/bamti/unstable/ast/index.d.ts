import type { JsonValue, RequestOptions, RpcParams, Session } from "../../index.js";

export function nodeId<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function nodeRange<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function syntaxKind<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function nodeKind<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
