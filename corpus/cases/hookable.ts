// BamTS corpus driver for `hookable` (unjs/hookable @ b77477c).
//
// Imports the checked-in local source surface `src/hookable.ts` (which pulls
// `src/utils.ts` and `src/types.ts`) and exercises the awaitable hook system's
// deterministic, side-effect-observable behaviors: registration order, serial
// vs parallel calling, `hookOnce`, `deprecateHook` redirect, nested-namespace
// flattening via `addHooks`, `beforeEach`/`afterEach` spies, `removeHook`, and
// the minimal `HookableCore` variant.
//
// No dependency install, no network, no tsc/Babel/SWC/build. Runs under Node's
// raw TypeScript support (erasable syntax only). Output goes to stdout via
// `process.stdout.write` so the corpus oracle can compare bytes + exit code.

import { Hookable, HookableCore, createHooks } from "../projects/hookable/src/hookable.ts";

const lines: string[] = [];
const line = (label: string, value: unknown): void => {
  lines.push(label + "=" + JSON.stringify(value));
};

// Silence the library's deterministic deprecation `console.warn` so the oracle
// only observes stdout. Restored immediately after the redirecting registration.
const originalWarn = console.warn;
console.warn = (): void => {};

// 1. Serial execution preserves registration order across async + sync hooks.
{
  const log: string[] = [];
  const hooks = createHooks();
  hooks.hook("run", async (n: number) => {
    await Promise.resolve();
    log.push("a:" + n);
  });
  hooks.hook("run", (n: number) => {
    log.push("b:" + n);
  });
  await hooks.callHook("run", 42);
  line("serial", log);
}

// 2. hookOnce fires exactly once across repeated calls.
{
  const log: string[] = [];
  const hooks = createHooks();
  hooks.hookOnce("once", () => {
    log.push("fired");
  });
  void hooks.callHook("once");
  void hooks.callHook("once");
  await Promise.resolve();
  line("once", log);
}

// 3. deprecateHook redirects registration of the old name onto the new name;
//    calling the new name fires both the original and the redirected hook.
console.warn = originalWarn; // restore before the redirecting registration
{
  const log: string[] = [];
  const hooks = createHooks();
  hooks.hook("newName", () => {
    log.push("original");
  });
  hooks.deprecateHook("oldName", "newName");
  console.warn = (): void => {}; // silence deprecation notice for this reg
  hooks.hook("oldName", () => {
    log.push("via-deprecated");
  });
  console.warn = originalWarn;
  void hooks.callHook("newName");
  await Promise.resolve();
  line("deprecate_redirect", log);
}

// 4. addHooks flattens nested namespaces into dotted hook names.
{
  const log: string[] = [];
  const hooks = createHooks();
  hooks.addHooks({
    flat: () => {
      log.push("flat");
    },
    ns: {
      sub: () => {
        log.push("ns:sub");
      },
      deep: {
        x: () => {
          log.push("ns:deep:x");
        },
      },
    },
  });
  void hooks.callHook("flat");
  void hooks.callHook("ns:sub");
  void hooks.callHook("ns:deep:x");
  await Promise.resolve();
  line("nested", log);
}

// 5. beforeEach / afterEach spies bracket each hook call with the event name.
{
  const log: string[] = [];
  const hooks = createHooks();
  hooks.beforeEach((event: { name: string }) => {
    log.push("before:" + event.name);
  });
  hooks.afterEach((event: { name: string }) => {
    log.push("after:" + event.name);
  });
  hooks.hook("ev", () => {
    log.push("body");
  });
  void hooks.callHook("ev");
  await Promise.resolve();
  line("spies", log);
}

// 6. removeHook drops a specific registered callback while keeping the rest.
{
  const log: string[] = [];
  const hooks = createHooks();
  const keep = (): void => {
    log.push("keep");
  };
  const drop = (): void => {
    log.push("drop");
  };
  hooks.hook("x", keep);
  hooks.hook("x", drop);
  hooks.removeHook("x", drop);
  void hooks.callHook("x");
  await Promise.resolve();
  line("remove", log);
}

// 7. callHookParallel runs all hooks and resolves to their settled results.
{
  const log: string[] = [];
  const hooks = createHooks();
  hooks.hook("par", () => {
    log.push("p1");
    return 1;
  });
  hooks.hook("par", () => {
    log.push("p2");
    return 2;
  });
  const result = hooks.callHookParallel("par");
  const settled = result instanceof Promise ? await result : result;
  line("parallel", { log, settled });
}

// 8. HookableCore: minimal variant supports hook + callHook with removal.
{
  const log: string[] = [];
  const core = new HookableCore();
  const cb = (): void => {
    log.push("core-fired");
  };
  core.hook("core", cb);
  void core.callHook("core");
  core.removeHook("core", cb);
  void core.callHook("core");
  await Promise.resolve();
  line("core", log);
}

// 9. Hookable instance exposes bound hook/callHook/callHookWith for destructuring.
{
  const hooks = createHooks<Record<string, (...a: unknown[]) => void>>();
  const log: string[] = [];
  const { hook, callHook } = hooks;
  hook("d", () => {
    log.push("destructured");
  });
  void callHook("d");
  await Promise.resolve();
  line("destructure", log);
}

process.stdout.write(lines.join("\n") + "\n");
