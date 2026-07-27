// Citty corpus driver — exercises parseRawArgs from the vendored _parser.ts,
// the project's self-contained argument parser built on node:util.
// No external packages, no network, no env-dependent output. Deterministic.

import { parseRawArgs } from "../projects/citty/src/_parser.ts";

function summarize(label: string, argv: Record<string, unknown>): void {
  const keys = Object.keys(argv).filter((k) => k !== "_").sort();
  const pairs = keys.map((k) => `${k}=${JSON.stringify(argv[k])}`);
  const positionals = (argv._ as string[]).map((s) => JSON.stringify(s)).join(",");
  process.stdout.write(`${label}|pos=[${positionals}]|${pairs.join("|")}\n`);
}

// 1. Booleans + string option
summarize(
  "bools",
  parseRawArgs(["--verbose", "--name", "alice", "file.txt"], {
    boolean: ["verbose"],
    string: ["name"],
  }) as Record<string, unknown>,
);

// 2. Short alias
summarize(
  "short",
  parseRawArgs(["-n", "bob"], {
    string: ["name"],
    alias: { n: ["name"] },
  }) as Record<string, unknown>,
);

// 3. --no- negation
summarize(
  "negate",
  parseRawArgs(["--no-color"], {
    boolean: ["color"],
  }) as Record<string, unknown>,
);

// 4. Defaults applied when flag absent
summarize(
  "defaults",
  parseRawArgs([], {
    boolean: ["color"],
    string: ["mode"],
    default: { color: true, mode: "dev" },
  }) as Record<string, unknown>,
);

// 5. Alias propagation (value set on alias appears on main)
summarize(
  "alias-prop",
  parseRawArgs(["--port", "8080"], {
    string: ["port"],
    alias: { p: ["port"] },
  }) as Record<string, unknown>,
);

// 6. Double-dash terminator: everything after `--` is positional
summarize(
  "dashdash",
  parseRawArgs(["--name", "eve", "--", "--verbose", "--no-color"], {
    string: ["name"],
    boolean: ["verbose", "color"],
  }) as Record<string, unknown>,
);

// 7. Multiple positionals, no options
summarize(
  "positionals",
  parseRawArgs(["a", "b", "c"], {}) as Record<string, unknown>,
);

// 8. Boolean with explicit =false string coercion
summarize(
  "bool-string",
  parseRawArgs(["--color=false"], {
    boolean: ["color"],
  }) as Record<string, unknown>,
);
