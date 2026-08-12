# bamti

`bamti` is the JavaScript library interface for the BamTS compiler. It exposes the
same `resolveBinary` and `run` functions as `bamti-cli`; `run` invokes the CLI in a
child process rather than embedding the compiler in Node.

## Pre-release status

Version 0.1.0 is published on npm. Its `bamti-cli` dependency resolves to a
JavaScript shim, but the required platform binary packages are unavailable.
Installing this package does not provide a working compiler today. Build `bamts`
from source with Cargo until the platform packages are published.

```js
import { run } from "bamti";

const exitCode = await run(["input.ts"]);
```
