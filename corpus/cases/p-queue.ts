import lowerBound from '../projects/p-queue/source/lower-bound.ts';

// Deterministic corpus driver for p-queue.
//
// Exercises lowerBound — the binary-search insertion-point function ported
// from C++ std::lower_bound — which is the algorithmic core that PriorityQueue
// uses to keep its internal sorted array ordered. lower-bound.ts is the only
// self-contained runtime surface in this repository: it has zero imports and
// depends solely on checked-in local source. The main entry (index.ts) and
// PriorityQueue (priority-queue.ts) pull in eventemitter3 and p-timeout at
// runtime, or reference sibling .js specifiers that Node's raw TypeScript
// support cannot resolve, so they are excluded.
//
// All output is a pure function of input — no timing, randomness, network,
// or state.

const out: string[] = [];
const emit = (s: string) => out.push(s);

// ---------------------------------------------------------------------------
// Basic: find insertion index in a descending-sorted array (priority order).
// The comparator (a, b) => b - a makes lowerBound find the leftmost position
// where `value` can be inserted while keeping descending order.
// ---------------------------------------------------------------------------
emit('--- descending priority order ---');
const desc = [100, 80, 80, 50, 20, 10];
emit(`array: [${desc.join(', ')}]`);

const cases: Array<[number, string]> = [
	[200, 'highest (prepend)'],
	[100, 'equal to head'],
	[80, 'equal to middle duplicate'],
	[60, 'between 80 and 50'],
	[50, 'equal to 50'],
	[5, 'lowest (append)'],
];

for (const [val, label] of cases) {
	const idx = lowerBound(desc, val, (a, b) => b - a);
	emit(`insert ${String(val).padStart(3)} at idx ${idx}  // ${label}`);
}

// ---------------------------------------------------------------------------
// Ascending order (standard lower_bound semantics): comparator (a,b)=>a-b.
// ---------------------------------------------------------------------------
emit('--- ascending numeric order ---');
const asc = [1, 3, 5, 7, 9, 11];
emit(`array: [${asc.join(', ')}]`);

const ascCases: Array<[number, string]> = [
	[0, 'before head'],
	[1, 'equal to head'],
	[4, 'between 3 and 5'],
	[6, 'between 5 and 7'],
	[11, 'equal to tail'],
	[20, 'after tail'],
];

for (const [val, label] of ascCases) {
	const idx = lowerBound(asc, val, (a, b) => a - b);
	emit(`insert ${String(val).padStart(2)} at idx ${idx}  // ${label}`);
}

// ---------------------------------------------------------------------------
// Duplicates: lowerBound returns the leftmost valid insertion point.
// ---------------------------------------------------------------------------
emit('--- duplicates (leftmost insertion) ---');
const dups = [10, 10, 10, 10];
for (const v of [10, 11, 9]) {
	const idx = lowerBound(dups, v, (a, b) => a - b);
	emit(`insert ${v} into [${dups.join(', ')}] at idx ${idx}`);
}

// ---------------------------------------------------------------------------
// Empty and single-element arrays.
// ---------------------------------------------------------------------------
emit('--- edge: empty and single ---');
emit(`empty array: idx ${lowerBound([], 5, (a, b) => a - b)}`);
emit(`single [5] insert 3: idx ${lowerBound([5], 3, (a, b) => a - b)}`);
emit(`single [5] insert 5: idx ${lowerBound([5], 5, (a, b) => a - b)}`);
emit(`single [5] insert 9: idx ${lowerBound([5], 9, (a, b) => a - b)}`);

// ---------------------------------------------------------------------------
// Simulate PriorityQueue's actual usage: maintain a sorted array by repeated
// insertion using lowerBound to find the position, then splice.
// ---------------------------------------------------------------------------
emit('--- simulated priority-queue insertion ---');
const pq: Array<{priority: number; label: string}> = [];
const insert = (priority: number, label: string) => {
	const element = {priority, label};
	const idx = lowerBound(pq, element, (a, b) => b.priority - a.priority);
	pq.splice(idx, 0, element);
};

insert(1, 'a');
insert(5, 'b');
insert(3, 'c');
insert(5, 'd');
insert(1, 'e');
insert(10, 'f');
insert(0, 'g');

emit(`size: ${pq.length}`);
emit(`order: ${pq.map(e => `${e.label}@${e.priority}`).join(' ')}`);
emit(`priorities: ${pq.map(e => e.priority).join(',')}`);

// Verify sorted descending by priority
let sortedOk = true;
for (let i = 1; i < pq.length; i++) {
	if (pq[i - 1].priority < pq[i].priority) {
		sortedOk = false;
		break;
	}
}
emit(`sorted descending: ${sortedOk}`);

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------
process.stdout.write(out.join('\n') + '\n');
