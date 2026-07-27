// Corpus driver for pathe — exercises the zero-dependency leaf modules
// `_glob.ts` (glob compiler/matcher) and `_internal.ts` (Windows path
// normaliser) that ship in the vendored source tree.
//
// These are the only source files with no extensionless relative imports,
// so they resolve cleanly under Node 24's native TypeScript stripping with
// explicit `.ts` specifiers and no special flags.

import { matchGlob } from "../projects/pathe/src/_glob.ts";
import { normalizeWindowsPath } from "../projects/pathe/src/_internal.ts";

function emit(label: string, value: unknown): void {
  process.stdout.write(`${label}=${typeof value === "string" ? value : String(value)}\n`);
}

// --- Glob matching (ported zeptomatch engine) ---

// Star: matches any characters except path separators
emit("star-js", matchGlob("*.js", "a.js"));
emit("star-noext", matchGlob("*.js", "abcd"));
emit("star-wrongext", matchGlob("*.js", "a.md"));
emit("star-nested", matchGlob("*.js", "a/b.js"));

// Double-star (globstar): matches across directory boundaries
emit("globstar-ts", matchGlob("**/*.ts", "src/a/b.ts"));
emit("globstar-nojs", matchGlob("**/*.ts", "src/a/b.js"));

// Globstar with suffix anchor
emit("globstar-suffix", matchGlob("*/**foo", "foo/barfoo"));

// Brace expansion
emit("brace-a", matchGlob("{a,b}.js", "a.js"));
emit("brace-b", matchGlob("{a,b}.js", "b.js"));
emit("brace-c", matchGlob("{a,b}.js", "c.js"));

// Character classes
emit("class-cat", matchGlob("[abc]at", "cat"));
emit("class-bat", matchGlob("[abc]at", "bat"));
emit("class-dat", matchGlob("[abc]at", "dat"));

// Negation (odd number of leading !)
emit("neg-bar", matchGlob("!foo", "bar"));
emit("neg-foo", matchGlob("!foo", "foo"));

// Double negation (even number of leading !)
emit("dneg-foo", matchGlob("!!foo", "foo"));

// Multiple globs (array = OR)
emit("array-md", matchGlob(["*.md", "*.js"], "foo.md"));
emit("array-js", matchGlob(["*.md", "*.js"], "foo.js"));
emit("array-txt", matchGlob(["*.md", "*.js"], "foo.txt"));

// Numeric brace range
emit("range-1", matchGlob("file{1..3}.txt", "file1.txt"));
emit("range-2", matchGlob("file{1..3}.txt", "file2.txt"));
emit("range-4", matchGlob("file{1..3}.txt", "file4.txt"));

// --- Windows path normalisation ---

emit("norm-drive-upper", normalizeWindowsPath("C:\\Users\\foo\\bar"));
emit("norm-drive-lower", normalizeWindowsPath("c:/users/foo/bar"));
emit("norm-unc", normalizeWindowsPath("\\\\server\\share\\file"));
emit("norm-empty", normalizeWindowsPath(""));
emit("norm-posix", normalizeWindowsPath("/usr/local/bin"));
