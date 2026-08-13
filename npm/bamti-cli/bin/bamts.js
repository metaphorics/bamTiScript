#!/usr/bin/env node

import { run } from "../index.js";

try {
  process.exitCode = await run(process.argv.slice(2), { stdio: "inherit" });
} catch (error) {
  console.error(`bamts: ${error.message}`);
  console.error(
    "The BamTS binary is pre-release. Build or install the matching platform artifact before running bamts.",
  );
  process.exitCode = 1;
}
