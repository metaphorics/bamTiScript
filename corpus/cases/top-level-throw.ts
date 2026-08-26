// Pins top-level throw as a process outcome: empty stdout and a non-zero
// exit. Swallowing the throw or printing it to stdout would match neither
// Node 24 nor Interpreter/JIT/AOT.

export {};

throw new Error("boom");
