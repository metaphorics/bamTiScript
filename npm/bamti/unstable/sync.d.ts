import type { JsonValue, RequestOptions, RpcParams, SessionOptions } from "../index.js";
import type { ServiceApi } from "./proto.js";

export interface SyncService extends ServiceApi {
  request<Result extends JsonValue = JsonValue>(
    method: string,
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  dispose(): Promise<void>;
}

export function createSyncService(options?: SessionOptions): SyncService;
