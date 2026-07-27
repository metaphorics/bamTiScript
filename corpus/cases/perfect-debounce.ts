// BamTS corpus driver for perfect-debounce @ 430686fdcb590b838909eac66fbd99997c09ef12
// Exercises debounce options, pending state, synchronous flush, cancel,
// leading/trailing execution modes, and context binding from checked-in local source.
// Node 24 raw TypeScript — erasable syntax only.

import { debounce } from "../projects/perfect-debounce/src/index.ts";

const out: string[] = [];

// 1. Options & Parameter Validation
out.push("--- 1. Options Validation ---");
for (const invalidWait of [NaN, Infinity, -Infinity]) {
	try {
		debounce(async () => {}, invalidWait);
		out.push(`fail: ${invalidWait} did not throw`);
	} catch (err: unknown) {
		const e = err as Error;
		out.push(`pass: wait ${invalidWait} throws ${e.name}: ${e.message}`);
	}
}

// 2. Trailing Debounce & Flush
out.push("\n--- 2. Trailing Debounce & Flush ---");
const trailingCalls: string[] = [];
const trailingFn = debounce(async (val: string) => {
	trailingCalls.push(val);
	return `result:${val}`;
}, 50);

out.push(`isPending before calls: ${trailingFn.isPending()}`);
trailingFn("arg-1");
trailingFn("arg-2");
trailingFn("arg-3");
out.push(`isPending after calls: ${trailingFn.isPending()}`);

const trailingRes = await trailingFn.flush();
out.push(`flush result: ${trailingRes}`);
out.push(`isPending after flush: ${trailingFn.isPending()}`);
out.push(`executed calls: ${trailingCalls.join(", ")}`);

const emptyFlush = trailingFn.flush();
out.push(`empty flush result: ${emptyFlush}`);

// 3. Cancellation
out.push("\n--- 3. Cancellation ---");
const cancelCalls: string[] = [];
const cancelFn = debounce(async (val: string) => {
	cancelCalls.push(val);
	return val;
}, 50);

cancelFn("pending-call");
out.push(`isPending before cancel: ${cancelFn.isPending()}`);
cancelFn.cancel();
out.push(`isPending after cancel: ${cancelFn.isPending()}`);

const cancelFlush = cancelFn.flush();
out.push(`flush after cancel: ${cancelFlush}`);
out.push(`cancelCalls length: ${cancelCalls.length}`);

// 4. Leading Edge Execution
out.push("\n--- 4. Leading Edge Execution ---");
const leadingCalls: string[] = [];
const leadingFn = debounce(
	async (val: string) => {
		leadingCalls.push(val);
		return `lead-res:${val}`;
	},
	50,
	{ leading: true, trailing: false },
);

const leadPromise = leadingFn("immediate-arg");
out.push(`leading calls immediate: ${leadingCalls.join(", ")}`);
out.push(`isPending after leading call: ${leadingFn.isPending()}`);

const leadResult = await leadPromise;
out.push(`lead promise result: ${leadResult}`);

leadingFn.cancel();
out.push(`isPending after cancel: ${leadingFn.isPending()}`);

// 5. Leading + Trailing Combination
out.push("\n--- 5. Leading + Trailing Combination ---");
const comboCalls: string[] = [];
const comboFn = debounce(
	async (val: string) => {
		comboCalls.push(val);
		return `combo-res:${val}`;
	},
	50,
	{ leading: true, trailing: true },
);

const leadingRes = await comboFn("lead-val");
out.push(`leading edge result: ${leadingRes}`);

comboFn("trail-val-1");
comboFn("trail-val-2");
out.push(`isPending before flush: ${comboFn.isPending()}`);

const trailingResCombo = await comboFn.flush();
out.push(`trailing flush result: ${trailingResCombo}`);
out.push(`combo calls history: ${comboCalls.join(", ")}`);
out.push(`isPending after flush: ${comboFn.isPending()}`);

// 6. Context (this) Binding
out.push("\n--- 6. Context Binding ---");
interface CounterService {
	multiplier: number;
	compute(val: number): Promise<number>;
}

const service: CounterService = {
	multiplier: 10,
	async compute(val: number) {
		return val * this.multiplier;
	},
};

// 6a. Direct call in leading mode preserves invocation receiver
const leadingMethod = debounce(service.compute, 50, {
	leading: true,
	trailing: false,
});
const directRes = await leadingMethod.call(service, 5);
out.push(`leading receiver result: ${directRes}`);
leadingMethod.cancel();

// 6b. Explicitly bound function works across flush
const boundMethod = debounce(service.compute.bind(service), 50);
boundMethod(7);
const boundRes = await boundMethod.flush();
out.push(`bound method flush result: ${boundRes}`);

// 7. Concurrent Call Promise Locking
out.push("\n--- 7. Concurrent Call Promise Locking ---");
let gateResolve: () => void = () => {};
const gatePromise = new Promise<void>((r) => {
	gateResolve = r;
});

const asyncGateFn = debounce(
	async (v: string) => {
		await gatePromise;
		return `gate:${v}`;
	},
	50,
	{ leading: true },
);

const firstCallPromise = asyncGateFn("lock-test");
const secondCallPromise = asyncGateFn("lock-test-2");
out.push(`isPending during execution: ${asyncGateFn.isPending()}`);

gateResolve();
const [res1, res2] = await Promise.all([firstCallPromise, secondCallPromise]);
out.push(`first promise result: ${res1}`);
out.push(`second promise result: ${res2}`);
asyncGateFn.cancel();

process.stdout.write(out.join("\n") + "\n");
