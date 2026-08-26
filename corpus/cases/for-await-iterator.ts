// Pins for-await unwrapping versus IteratorResult thenables, plus
// iterator.return on break/throw/labeled exits. Assimilating a thenable
// result or skipping close changes the trace. Interpreter/JIT/AOT must
// match Node 24.

export {};
(async () => {
  const out: string[] = [];

  for await (const v of [Promise.resolve(42)]) {
    out.push(`promise.unwrapped=${v === 42}`);
  }

  let thenableCounter = 0;
  const thenableIterator = {
    [Symbol.iterator]() {
      let step = 0;
      return {
        next() {
          if (step++ === 0) {
            return {
              value: "raw",
              done: false,
              then(resolve: (value: string) => void, _reject: unknown) {
                thenableCounter += 1;
                resolve("assimilated");
              },
            };
          }
          return { value: undefined, done: true };
        },
      };
    },
  };
  for await (const v of thenableIterator) {
    out.push(`thenable.yield=${v}`);
    out.push(`thenable.counter=${thenableCounter}`);
  }

  let rejectionCloseCount = 0;
  const rejectingIterator = {
    [Symbol.iterator]() {
      let step = 0;
      return {
        next() {
          if (step++ === 0) {
            return { value: Promise.reject("value-rejection"), done: false };
          }
          return { value: undefined, done: true };
        },
        return() {
          rejectionCloseCount = rejectionCloseCount + 1;
          return { value: undefined, done: true };
        },
      };
    },
  };
  try {
    for await (const _v of rejectingIterator) {}
  } catch (error) {
    out.push(`rejection.reason=${error === "value-rejection"}`);
  }
  out.push(`rejection.close-count=${rejectionCloseCount}`);

  let stepFailureCloseCount = 0;
  const failingStepIterator = {
    [Symbol.iterator]() {
      return {
        next() {
          throw "step-failure";
        },
        return() {
          stepFailureCloseCount = stepFailureCloseCount + 1;
          return { value: undefined, done: true };
        },
      };
    },
  };
  try {
    for await (const _v of failingStepIterator) {}
  } catch (error) {
    out.push(`step-failure.reason=${error === "step-failure"}`);
  }
  out.push(`step-failure.close-count=${stepFailureCloseCount}`);

  const trace: string[] = [];
  function sync(
    tag: string,
    values: unknown[],
    closeError?: unknown,
    closeResult?: unknown,
  ) {
    let index = 0;
    return {
      [Symbol.iterator]() {
        return this;
      },
      next() {
        return index < values.length
          ? { value: values[index++], done: false }
          : { value: undefined, done: true };
      },
      return() {
        trace.push(`${tag}:return`);
        if (closeError !== undefined) throw closeError;
        if (closeResult !== undefined) return closeResult;
        return { value: undefined, done: true };
      },
    };
  }

  for (const value of sync("break", [1])) {
    trace.push(`break:body:${value}`);
    break;
  }
  for (const value of sync("normal", [1, 2])) {
    trace.push(`normal:body:${value}`);
  }
  for (const value of sync("continue", [1, 2])) {
    trace.push(`continue:body:${value}`);
    continue;
  }
  for (const value of [1]) {
    trace.push(`absent:body:${value}`);
    break;
  }
  try {
    for (const value of sync("primitive", [1], undefined, 1)) {
      trace.push(`primitive:body:${value}`);
      break;
    }
  } catch {
    trace.push("primitive:catch");
  }
  function returnFromLoop(): string {
    for (const value of sync("function", [1])) {
      try {
        trace.push(`function:body:${value}`);
        return "returned";
      } finally {
        trace.push("function:finally");
      }
    }
    return "fallthrough";
  }
  trace.push(`function:result:${returnFromLoop()}`);
  try {
    const bindingValue = {
      get value() {
        trace.push("binding:get");
        throw "binding-error";
      },
    };
    for (const { value } of sync("binding", [bindingValue])) {
      trace.push(`binding:body:${value}`);
    }
  } catch {
    trace.push("binding:catch");
  }
  exit: {
    for (const _value of sync("label-break", [1])) {
      try {
        trace.push("label-break:body");
        break exit;
      } finally {
        trace.push("label-break:finally");
      }
    }
  }
  outer: for (let i = 0; i < 2; i++) {
    for (const _value of sync(`label:${i}`, [i])) {
      try {
        trace.push(`label:body:${i}`);
        continue outer;
      } finally {
        trace.push(`label:finally:${i}`);
      }
    }
  }
  try {
    for (const _value of sync("throw", [1], "close-error")) {
      try {
        trace.push("throw:body");
        throw "body-error";
      } finally {
        trace.push("throw:finally");
      }
    }
  } catch (error) {
    trace.push(`throw:catch:${error}`);
  }
  for await (const value of [1]) {
    trace.push(`async-absent:body:${value}`);
    break;
  }
  const asyncPrimitive = {
    [Symbol.asyncIterator]() {
      return {
        next() {
          return Promise.resolve({ value: 1, done: false });
        },
        return() {
          trace.push("async-primitive:return");
          return Promise.resolve(1);
        },
      };
    },
  };
  try {
    for await (const value of asyncPrimitive) {
      trace.push(`async-primitive:body:${value}`);
      break;
    }
  } catch {
    trace.push("async-primitive:catch");
  }
  const asyncIterable = {
    [Symbol.asyncIterator]() {
      let started = false;
      return {
        next() {
          if (started) return Promise.resolve({ value: undefined, done: true });
          started = true;
          return Promise.resolve({ value: 1, done: false });
        },
        return() {
          trace.push("async:return");
          return Promise.resolve().then(() => {
            trace.push("async:return-reject");
            throw "async-close-error";
          });
        },
      };
    },
  };
  try {
    for await (const value of asyncIterable) {
      try {
        trace.push(`async:body:${value}`);
        throw "async-body-error";
      } finally {
        trace.push("async:finally");
      }
    }
  } catch (error) {
    trace.push(`async:catch:${error}`);
  }

  process.stdout.write(out.concat(trace).join("\n") + "\n");
})();
