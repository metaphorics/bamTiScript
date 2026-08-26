import type { JsonValue, RequestOptions, RpcParams, Session } from "../../index.js";

export function textOfRange<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function nodeText<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function containsRange<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function containsPosition<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
export function narrowestContaining<Result extends JsonValue = JsonValue>(session: Session, params: RpcParams, options?: RequestOptions): Promise<Result>;
