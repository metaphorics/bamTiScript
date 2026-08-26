#!/usr/bin/env node

import { pathToFileURL } from "node:url";
import { run as runCli } from "bamti-cli";

export async function runExecutable(
  args = process.argv.slice(2),
  dependencies = {},
) {
  const processObject = dependencies.process ?? process;
  const runner = dependencies.runCli ?? runCli;
  try {
    processObject.exitCode = await runner(args, { stdio: "inherit" });
  } catch (error) {
    const detail = error instanceof Error ? (error.stack ?? error.message) : String(error);
    processObject.stderr.write(`${detail}\n`);
    processObject.exitCode = 1;
  }
  return processObject.exitCode;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  void runExecutable();
}
