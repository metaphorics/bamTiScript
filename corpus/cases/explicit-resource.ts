// Pins explicit resource management: using/await using LIFO dispose,
// loop-head for-(await)-using, nullish skip, sync-dispose fallback, and
// nested SuppressedError chains. A missed disposer or wrong suppression
// order changes the trace. Interpreter/JIT/AOT must match Node 24.

export {};
(async () => {
  const out: string[] = [];

  function res(name: string) {
    return {
      name,
      [Symbol.dispose]() {
        out.push(`dispose:${name}`);
      },
    };
  }

  function asyncRes(name: string) {
    return {
      name,
      async [Symbol.asyncDispose]() {
        out.push(`async-dispose:${name}`);
      },
    };
  }

  {
    using a = res("a");
    using b = res("b");
    out.push(`use:${a.name},${b.name}`);
  }
  out.push("block.after");

  {
    using outer = res("outer");
    {
      using inner = res("inner");
      out.push(`nested.use:${outer.name},${inner.name}`);
    }
    out.push("nested.after-inner");
  }
  out.push("nested.after-outer");

  for (using r of [res("i0"), res("i1")]) {
    out.push(`loop.use:${r.name}`);
  }
  out.push("loop.after");

  function returning(): string {
    try {
      {
        using r = res("return");
        out.push("return:body");
        return "value";
      }
    } finally {
      out.push("return:finally");
    }
  }
  out.push(`return:result:${returning()}`);

  {
    await using a = asyncRes("aa");
    await using b = asyncRes("ab");
    out.push(`async.use:${a.name},${b.name}`);
  }
  out.push("async.block.after");

  {
    const rejected = Promise.reject(new Error("ignored:fallback"));
    rejected.catch(() => {});
    await using fallback = {
      name: "ignored-reject",
      [Symbol.dispose]() {
        out.push("dispose:ignored-reject");
        return rejected;
      },
    };
    out.push(`fallback.use:${fallback.name}`);
  }
  out.push("fallback.after");

  {
    const empty = null as never;
    await using nullable = empty;
    out.push("nullish.use");
  }
  out.push("nullish.after");

  for (await using r of [asyncRes("ai0"), asyncRes("ai1")]) {
    out.push(`async-loop.use:${r.name}`);
  }
  out.push("async-loop.after");

  function bad(name: string) {
    return {
      [Symbol.dispose]() {
        throw new Error(name);
      },
    };
  }
  try {
    using a = bad("dispose-a");
    using b = bad("dispose-b");
    throw new Error("body");
  } catch (error) {
    const e = error as {
      constructor: { name: string };
      error?: { message: string };
      suppressed?: { error?: { message: string }; suppressed?: { message: string } };
    };
    out.push(
      `sync-suppressed:${e.constructor.name}:${e.error?.message}:${e.suppressed?.error?.message}:${e.suppressed?.suppressed?.message}`,
    );
  }

  function asyncBad(name: string) {
    return {
      async [Symbol.asyncDispose]() {
        out.push(`${name}:start`);
        queueMicrotask(() => {
          out.push(`${name}:checkpoint`);
        });
        await Promise.resolve();
        out.push(`${name}:throw`);
        throw new Error(name);
      },
    };
  }
  try {
    await using a = asyncBad("async-a");
    await using b = asyncBad("async-b");
    throw new Error("async-body");
  } catch (error) {
    const e = error as {
      constructor: { name: string };
      error?: { message: string };
      suppressed?: {
        constructor: { name: string };
        error?: { message: string };
        suppressed?: { message: string };
      };
    };
    out.push(
      `async-suppressed:${e.constructor.name}:${e.error?.message}:${e.suppressed?.constructor.name}:${e.suppressed?.error?.message}:${e.suppressed?.suppressed?.message}`,
    );
  }

  process.stdout.write(out.join("\n") + "\n");
})();
