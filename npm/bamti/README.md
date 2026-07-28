# bamti

`bamti` is the JavaScript library interface for the BamTS compiler. It exposes the
same `resolveBinary` and `run` functions as `bamti-cli`; `run` invokes the CLI in a
child process rather than embedding the compiler in Node.

## Pre-release status

This package is prepared for a future npm release. It is not published, and the
Rust workspace does not yet define the `bamts` binary that the platform packages
will contain. Installing this source package does not provide a working compiler.
Once the binary exists and a platform artifact is installed, use `run(args)` to
execute it.

```js
import { run } from "bamti";

const exitCode = await run(["input.ts"]);
```
