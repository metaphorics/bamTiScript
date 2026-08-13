export interface RunOptions {
  cwd?: string;
  env?: Record<string, string | undefined>;
  stdio?: "inherit" | "ignore" | "pipe";
}

export function run(
  args?: readonly string[],
  options?: RunOptions,
): Promise<number>;
