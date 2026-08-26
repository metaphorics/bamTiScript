# bamti

`bamti` is the Node.js 24 interface for the BamTS compiler. The package root runs CLI invocations in-process through the native Node-API addon. Persistent TypeScript service and public AST requests share one framed `bamti-cli --api` child process so project state survives across requests.

```js
import { run } from "bamti";

const exitCode = await run(["--noEmit", "input.ts"], {
  cwd: process.cwd(),
  env: process.env,
  stdio: "inherit",
});
```

`stdio` accepts `"inherit"`, `"ignore"`, or `"pipe"`. The public API returns only the exit code, so `"pipe"` captures and discards compiler output.

For persistent compiler state, create a session and dispose it when finished:

```js
import { createSession } from "bamti";

await using session = createSession({ root: process.cwd() });
await session.open({ path: "input.ts", text: "const answer = 42;", version: 1 });
const diagnostics = await session.diagnostics({ path: "input.ts" });
```

The package exports the root, `unstable/sync`, `unstable/async`, `unstable/fs`, `unstable/proto`, `unstable/ast`, six AST operation subpaths, and `package.json`.

## Architecture and target design

- **Native execution vs persistent service**: root `run()` uses the direct native addon loader. Session and unstable service/AST exports use the standalone `bamts --api` protocol through `bamti-cli`; they do not add a second native loader or fall back from native execution.
- **Node.js requirement**: Requires Node.js 24 or later.
- **Five target platform packages**: Loads optional platform packages (`@bamti/bamti-linux-x64-gnu`, `@bamti/bamti-linux-arm64-gnu`, `@bamti/bamti-darwin-x64`, `@bamti/bamti-darwin-arm64`, `@bamti/bamti-win32-x64-msvc`).
- **Fail-closed native loading**: Inspects host parameters, resolves the target package, and verifies manifest structure, binary SHA-256 digest, and release metadata. If missing or invalid, loading fails closed with `NativeArtifactNotFoundError` or `NativeArtifactLoadError`. It never downloads binaries, compiles from source, or falls back to the CLI executable.
- **Cancellation and teardown**: Root invocations and persistent requests accept `AbortSignal`. Persistent requests forward cancellation over the API protocol and dispose their child process when the session closes.

## Release status note

Currently published `0.1.0` packages on npm do not contain prebuilt native binary artifacts. The native addon implementation is present in the source repository but is not yet published to npm. Real five-target release validation remains blocked by GitHub billing and is unverified.
