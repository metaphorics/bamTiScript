# bamti-cli

`bamti-cli` provides the `bamts` command. At runtime its small JavaScript shim
selects an optional package for the current operating system and CPU, then starts
the native executable from that package. It does not use a postinstall script.

## Pre-release status

Version 0.1.0 is published on npm, but the platform artifact packages referenced
by its optional dependencies are unavailable. Build the CLI from source to use it
today. The JavaScript shim reports the missing package when an artifact is absent;
it does not fall back to an unrelated executable.

The package manifests reserve artifacts for Linux x64 and arm64, macOS x64 and
arm64, and Windows x64. These targets describe the planned package layout, not a
published support guarantee.

The package also defines this programmatic API:

```js
import { resolveBinary, run } from "bamti-cli";

const binary = resolveBinary();
const exitCode = await run(["input.ts"]);
```
