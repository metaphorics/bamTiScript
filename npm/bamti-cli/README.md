# bamti-cli

`bamti-cli` provides the `bamts` command. At runtime its small JavaScript shim
selects an optional package for the current operating system and CPU, then starts
the native executable from that package. It does not use a postinstall script.

## Pre-release status

These npm packages are prepared for a future release and are not published. The
Rust workspace does not yet define the `bamts` binary, so there is no working
platform artifact to install today. The shim deliberately fails with an actionable
error when its artifact is absent; it does not pretend that a compiler is present.

When artifacts exist, the package will support Linux x64 and arm64, macOS x64 and
arm64, and Windows x64. An unsupported platform or a skipped optional dependency
will report the package name needed to fix the installation.

The programmatic API is also available:

```js
import { resolveBinary, run } from "bamti-cli";

const binary = resolveBinary();
const exitCode = await run(["input.ts"]);
```
