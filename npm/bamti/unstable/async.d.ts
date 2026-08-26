import type { JsonValue, RequestOptions, RpcParams, SessionOptions } from "../index.js";
import type { ServiceApi } from "./proto.js";

export interface AsyncService extends ServiceApi, AsyncDisposable {
  request<Result extends JsonValue = JsonValue>(
    method: string,
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  dispose(): Promise<void>;
}

export function createAsyncService(options?: SessionOptions): AsyncService;
export { Session, createSession } from "../index.js";
