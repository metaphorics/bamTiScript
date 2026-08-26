import type {
  ResolveBinaryOptions as CliResolveBinaryOptions,
  RunOptions as CliRunOptions,
} from "bamti-cli";

export {
  ArtifactNotFoundError,
  UnsupportedPlatformError,
  artifactPackage,
  resolveBinary,
  resolveBinary as resolveArtifact,
} from "bamti-cli";
export type { ResolveBinaryOptions } from "bamti-cli";

export type RunOptions = CliRunOptions;

export function run(
  args?: readonly string[],
  options?: RunOptions,
): Promise<number>;

export type JsonPrimitive = boolean | number | string | null;
export type JsonValue =
  | JsonPrimitive
  | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };
export type RpcParams = Readonly<Record<string, JsonValue>>;
export interface FileSystemDescriptor {
  readonly kind: "os";
  readonly root: string;
}

export interface RequestOptions {
  signal?: AbortSignal;
}

export interface SpawnOptions {
  cwd?: string;
  env?: Readonly<Record<string, string | undefined>>;
  stdio: readonly ["pipe", "pipe", "pipe"];
}

export interface ReadableStreamLike {
  on(event: "data", listener: (chunk: Uint8Array) => void): this;
  on(event: "error", listener: (error: Error) => void): this;
}

export interface WritableStreamLike {
  write(chunk: Uint8Array): boolean;
  end(): void;
  on(event: "error", listener: (error: Error) => void): this;
}

export interface ChildProcessLike {
  stdin: WritableStreamLike;
  stdout: ReadableStreamLike;
  stderr: ReadableStreamLike;
  exitCode: number | null;
  signalCode: string | null;
  once(event: "error", listener: (error: Error) => void): this;
  once(
    event: "exit",
    listener: (code: number | null, signal: string | null) => void,
  ): this;
  kill(signal?: string): boolean;
}

export interface TransportOptions {
  binary?: string;
  binaryOptions?: CliResolveBinaryOptions;
  cwd?: string;
  env?: Readonly<Record<string, string | undefined>>;
  args?: readonly string[];
  spawnChild?: (
    binary: string,
    args: readonly string[],
    options: SpawnOptions,
  ) => ChildProcessLike;
  maxInFlight?: number;
  maxRestarts?: number;
  maxHeaderBytes?: number;
  maxFrameBytes?: number;
  stderrBytes?: number;
  disposeTimeoutMs?: number;
}

export class BamtiError extends Error {}
export class ProtocolError extends BamtiError {}
export class DisposedError extends BamtiError {}
export class TransportBusyError extends BamtiError {
  readonly inFlight: number;
  readonly maxInFlight: number;
}
export class TransportCrashError extends BamtiError {
  readonly code: number | null;
  readonly signal: string | null;
  readonly stderr: string;
}
export class RestartLimitError extends BamtiError {
  readonly restarts: number;
  readonly maxRestarts: number;
}
export class ServiceRequestError extends BamtiError {
  readonly method: string;
  readonly code: string | number | null;
  readonly data?: JsonValue;
}

export class Transport {
  constructor(options?: TransportOptions);
  readonly generation: number;
  readonly inFlight: number;
  readonly disposed: boolean;
  ready(): number;
  stderrTail(): string;
  request<Result extends JsonValue = JsonValue>(
    method: string,
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  dispose(): Promise<void>;
  [Symbol.asyncDispose](): Promise<void>;
}

export interface Service {
  open<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  update<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  close<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  snapshot<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  completions<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  definition<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  references<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  rename<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  diagnostics<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
}

export interface SessionOptions {
  root?: string;
  filesystem?: FileSystemDescriptor;
  transport?: Transport;
  transportOptions?: TransportOptions;
}

export class Session implements Service {
  constructor(options?: SessionOptions);
  readonly transport: Transport;
  request<Result extends JsonValue = JsonValue>(
    method: string,
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  open<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  update<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  close<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  snapshot<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  completions<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  definition<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  references<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  rename<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  diagnostics<Result extends JsonValue = JsonValue>(
    params?: RpcParams,
    options?: RequestOptions,
  ): Promise<Result>;
  dispose(): Promise<void>;
  [Symbol.asyncDispose](): Promise<void>;
}

export function createSession(options?: SessionOptions): Session;
