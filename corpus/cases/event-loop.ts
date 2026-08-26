// Pins event-loop quiescence: a leftover macrotask or drained-too-early
// microtask reorders stdout against Node 24. Interpreter/JIT/AOT must keep
// sync, then Promise jobs, then the timer, then the timer's nested microtask.

export {};

process.stdout.write("sync\n");
Promise.resolve().then(() => {
  process.stdout.write("micro\n");
});
setTimeout(() => {
  process.stdout.write("timer\n");
  Promise.resolve().then(() => {
    process.stdout.write("timer-micro\n");
  });
}, 1);
