// BamTS corpus driver for yocto-queue @ b07eac099753833b29d06c614149904445739776
// Exercises the Queue FIFO data structure from checked-in local source only.
// Node 24 raw TypeScript — erasable syntax only (type annotations, generic args).

import Queue from "../projects/yocto-queue/index.js";

type Step = {
	name: string;
	result: unknown;
};

const steps: Step[] = [];

function record(name: string, result: unknown): void {
	steps.push({name, result});
}

// 1. Empty-queue semantics: dequeue/peek on a fresh queue return undefined.
const empty = new Queue<number>();
record("dequeue.empty", empty.dequeue());
record("peek.empty", empty.peek());
record("size.empty", empty.size);

// 2. FIFO order across mixed values.
const q = new Queue<string>();
for (const v of ["a", "b", "c", "d"]) {
	q.enqueue(v);
}
record("size.afterEnqueue4", q.size);
record("peek.front", q.peek());

// 3. Dequeue preserves insertion order (FIFO).
const order: string[] = [];
let head = q.dequeue();
while (head !== undefined) {
	order.push(head);
	head = q.dequeue();
}
record("dequeue.order", order.join(","));
record("size.afterDrainByDequeue", q.size);
record("dequeue.emptyAgain", q.dequeue());

// 4. Iterator yields front-to-back without removing.
const it = new Queue<number>();
it.enqueue(10);
it.enqueue(20);
it.enqueue(30);
const snapshot = [...it];
record("iterator.snapshot", snapshot.join(","));
record("iterator.sizeUnchanged", it.size);

// 5. drain() empties the queue and yields in order.
const d = new Queue<number>();
d.enqueue(1);
d.enqueue(2);
d.enqueue(3);
const drained: number[] = [];
for (const item of d.drain()) {
	drained.push(item);
}
record("drain.values", drained.join(","));
record("drain.sizeAfter", d.size);
record("drain.isEmpty", [...d].length);

// 6. peek reflects the head; advancing the head changes peek.
const p = new Queue<string>();
p.enqueue("x");
p.enqueue("y");
record("peek.first", p.peek());
p.dequeue();
record("peek.afterDequeue", p.peek());

// 7. clear() resets size to 0 and empties the structure.
const c = new Queue<number>();
c.enqueue(5);
c.enqueue(6);
c.enqueue(7);
record("clear.sizeBefore", c.size);
c.clear();
record("clear.sizeAfter", c.size);
record("clear.peekAfter", c.peek());
record("clear.dequeueAfter", c.dequeue());

// 8. Reuse after clear: queue functions normally again.
c.enqueue(100);
c.enqueue(200);
record("reuse.dequeueFirst", c.dequeue());
record("reuse.size", c.size);

// Emit stable, project-derived results.
for (const step of steps) {
	const value = step.result;
	const text =
		value === undefined
			? "undefined"
			: Array.isArray(value)
				? value.join(",")
				: String(value);
	console.log(`${step.name}=${text}`);
}
