# bamti

`bamti` is the in-process Node.js interface for the BamTS compiler. It embeds the compiler directly into Node.js 24 or later via native Node-API bindings (`bamts-napi`) and atomic cancellation (`bamts-cancel`), returning a promise with the CLI exit code.

```js
import { run } from "bamti";

const exitCode = await run(["check", "input.ts"], {
  cwd: process.cwd(),
  env: process.env,
  stdio: "inherit",
});
```

`stdio` accepts `"inherit"`, `"ignore"`, or `"pipe"`. The public API returns only the exit code, so `"pipe"` captures and discards compiler output.

## Architecture and target design

- **In-process job vs standalone CLI**: `bamti` runs the compiler in-process via Node-API bindings. Use `bamti-cli` when you need the standalone `bamts` executable binary or resolution.
- **Node.js requirement**: Requires Node.js 24 or later.
- **Five target platform packages**: Designed to load optional platform packages (`@bamti/bamti-linux-x64-gnu`, `@bamti/bamti-linux-arm64-gnu`, `@bamti/bamti-darwin-x64`, `@bamti/bamti-darwin-arm64`, `@bamti/bamti-win32-x64-msvc`).
- **Fail-closed optional artifact loading**: Inspects host parameters, resolves the optional target package, and verifies manifest structure, binary SHA-256 digest, and release metadata. If missing or invalid, loading fails closed with `NativeArtifactNotFoundError` or `NativeArtifactLoadError`. It never downloads binaries, compiles from source, or falls back to the CLI executable at runtime.
- **Cancellation and teardown ownership**: Atomic cancellation token handling is owned by `bamts-cancel`. Native N-API environment isolate boundary and memory teardown are owned by private crate `bamts-napi`.

## Release status note

Currently published `0.1.0` packages on npm do not contain prebuilt native binary artifacts. The native addon implementation is present in the source repository but is not yet published to npm. Real five-target release validation remains blocked by GitHub billing and is unverified.
