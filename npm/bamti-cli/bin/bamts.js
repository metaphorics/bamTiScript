#!/usr/bin/env node

import { spawn } from "node:child_process";
import { resolveBinary } from "../index.js";

let binary;
try {
  binary = resolveBinary();
} catch (error) {
  console.error(`bamts: ${error.message}`);
  console.error("The BamTS binary is pre-release. Build or install the matching platform artifact before running bamts.");
  process.exitCode = 1;
}

if (binary) {
  const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
  child.once("error", (error) => {
    console.error(`bamts: failed to start ${binary}: ${error.message}`);
    process.exitCode = 1;
  });
  child.once("exit", (code) => {
    process.exitCode = code ?? 1;
  });
}
