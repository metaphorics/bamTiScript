export interface CliTarget {
  readonly selector: string;
  readonly target: string;
  readonly package: string;
  readonly entry: string;
  readonly os: string;
  readonly cpu: string;
  readonly artifactKind: string;
  readonly runner: string;
}

export const CLI_TARGETS: readonly CliTarget[];

export class UnsupportedPlatformError extends Error {}

export class ArtifactNotFoundError extends Error {}

export class ArtifactLoadError extends Error {}

export interface ResolveBinaryOptions {
  platform?: string;
  arch?: string;
  nodeVersion?: string;
  table?: object;
  facadeManifest?: object;
  resolvePackage?: (specifier: string) => string;
  realpath?: (path: string) => string;
  readFile?: (path: string) => Buffer;
  access?: (path: string, mode?: number) => void;
  mkdtemp?: (prefix: string) => string;
  writeFile?: (path: string, data: Buffer, options?: object) => void;
  chmod?: (path: string, mode: number) => void;
}

export interface RunOptions extends ResolveBinaryOptions {
  cwd?: string;
  env?: Record<string, string | undefined>;
  stdio?: "inherit" | "ignore" | "pipe";
  spawn?: (
    command: string,
    args: readonly string[],
    options: object,
  ) => { once(event: "error", listener: (error: Error) => void): void; once(event: "exit", listener: (code: number | null) => void): void };
}

export const ARTIFACTS: Map<string, string>;

export function artifactPackage(platform?: string, arch?: string): string;
export function resolveBinary(options?: ResolveBinaryOptions): string;
export function run(args?: readonly string[], options?: RunOptions): Promise<number>;
