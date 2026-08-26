import { resolve } from "node:path";

export function osFileSystem(root = process.cwd()) {
  return Object.freeze({ kind: "os", root: resolve(root) });
}

