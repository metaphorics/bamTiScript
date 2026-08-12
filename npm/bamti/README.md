# bamti

`bamti` is the JavaScript library interface for the BamTS compiler. It exposes the
same `resolveBinary` and `run` functions as `bamti-cli`; `run` invokes the CLI in a
child process rather than embedding the compiler in Node.

## Pre-release status

This package is prepared for a future npm release and is not published. The Rust
workspace defines the `bamts` binary, but the platform artifact packages do not
ship yet. Installing this source package does not provide a working compiler.
After the native artifacts are published, `run(args)` will execute the matching
platform binary.

```js
import { run } from "bamti";

const exitCode = await run(["input.ts"]);
```
