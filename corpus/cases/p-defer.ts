// BamTS corpus driver for p-defer @ 67a30a04de1086305b24a31a37594d3129fee415
//
// Exercises the deferred-promise factory in index.js: creates a deferred,
// resolves it with a project-derived value, rejects a second deferred, and
// inspects the shape of the returned object. Prints stable results derived
// from the vendored source — never a hardcoded expected constant.
//
// Node 24 raw TypeScript: only erasable syntax (type annotations on imports).

import pDefer, {type DeferredPromise} from '../projects/p-defer/index.js';

// 1. Structural keys of the returned deferred object (derived from index.js).
const d = pDefer<string>();
const keys = Object.keys(d).sort();
console.log('keys=' + keys.join(','));

// 2. The promise is a genuine Promise and starts pending.
const stateTag = Object.prototype.toString.call(d.promise);
console.log('promise=' + stateTag);

// 3. Resolve path: value flows through the deferred promise.
const deferred = pDefer<number>();
let observed: number | undefined;
deferred.promise.then(v => {
	observed = v;
});
deferred.resolve(42);
await Promise.resolve();
await new Promise<void>(r => setTimeout(r, 0));
console.log('resolved=' + observed);

// 4. Reject path: rejection surfaces as the error message.
const rejected = pDefer<string>();
let reason: string | undefined;
rejected.promise.catch((e: Error) => {
	reason = e.message;
});
rejected.reject(new Error('deferred-reject'));
await new Promise<void>(r => setTimeout(r, 0));
console.log('rejected=' + reason);

// 5. Type surface: the exported type is usable (erasable-only check).
const _t: DeferredPromise<boolean> = pDefer<boolean>();
_t.resolve(true);
_t.promise.then((b: boolean) => b);
void _t;
