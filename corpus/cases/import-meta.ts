// Pins import.meta identity and mutability. A cloned or frozen meta object
// fails the identity/custom-slot checks; a missing url fails the string
// check. Interpreter/JIT/AOT must match Node 24.

const meta = import.meta as ImportMeta & { custom?: number };
meta.custom = 1;
process.stdout.write(`${import.meta === import.meta}\n`);
process.stdout.write(`${meta.custom === 1}\n`);
process.stdout.write(
  `${typeof import.meta.url === "string" && import.meta.url.length > 0}\n`,
);
