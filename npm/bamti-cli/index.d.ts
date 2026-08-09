export class UnsupportedPlatformError extends Error {}

export class ArtifactNotFoundError extends Error {}

export interface ResolveBinaryOptions {
  platform?: string;
  arch?: string;
  resolvePackage?: (specifier: string) => string;
}

export interface RunOptions extends ResolveBinaryOptions {
  cwd?: string;
  env?: Record<string, string | undefined>;
  stdio?: "inherit" | "ignore" | "pipe";
}

export const ARTIFACTS: Map<string, string>;

export function artifactPackage(platform?: string, arch?: string): string;
export function resolveBinary(options?: ResolveBinaryOptions): string;
export function run(args?: readonly string[], options?: RunOptions): Promise<number>;
