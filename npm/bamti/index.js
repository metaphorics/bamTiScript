import { runNative } from "./native-runner.js";

export function run(args = [], options = {}) {
  return runNative(args, options);
}
