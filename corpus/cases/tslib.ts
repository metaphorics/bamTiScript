// BamTS corpus driver for tslib @ 12bd8a74b320e3acfaba36b0ecb0e14964a9165b
// Exercises TypeScript runtime helper functions from tslib.es6.mjs
// Uses Node 24 raw TypeScript; no external dependencies or Node built-ins.

import {
  __extends,
  __assign,
  __rest,
  __decorate,
  __param,
  __propKey,
  __setFunctionName,
  __values,
  __read,
  __spreadArray,
  __makeTemplateObject,
  __importStar,
  __importDefault,
  __classPrivateFieldGet,
  __classPrivateFieldSet,
  __classPrivateFieldIn,
  __addDisposableResource,
  __disposeResources,
  __rewriteRelativeImportExtension,
} from "../projects/tslib/tslib.es6.mjs";

function emit(label: string, val: unknown): void {
  process.stdout.write(`${label}: ${JSON.stringify(val)}\n`);
}

// 1. __extends - Class inheritance setup
interface InstanceRecord {
  baseProp?: string;
  derivedProp?: string;
}

function BaseClass(this: InstanceRecord): void {
  this.baseProp = "base-value";
}
function DerivedClass(this: InstanceRecord): void {
  BaseClass.call(this);
  this.derivedProp = "derived-value";
}

__extends(DerivedClass, BaseClass);

const derivedInstance: InstanceRecord = {};
DerivedClass.call(derivedInstance);
emit("extends-baseProp", derivedInstance.baseProp);
emit("extends-derivedProp", derivedInstance.derivedProp);
emit("extends-proto-link", Object.getPrototypeOf(DerivedClass.prototype) === BaseClass.prototype);

// 2. __assign - Object copy / assign polyfill
const assigned = __assign({ a: 1, b: 2 }, { b: 20, c: 30 });
emit("assign", assigned);

// 3. __rest - Exclude specified property keys
const restResult = __rest({ name: "Alice", age: 30, role: "admin" }, ["age"]);
emit("rest", restResult);

// 4. __decorate & __param - Property and parameter decorators
interface Decoratable {
  isDecorated?: boolean;
  paramIndex?: number;
}
function markDecorated(target: Decoratable): void {
  target.isDecorated = true;
}
const decoratedObj: Decoratable = {};
__decorate([markDecorated], decoratedObj, "action", null);
emit("decorate", decoratedObj.isDecorated);

const paramDecorator = __param(1, (target: Decoratable, _key: string | symbol, index: number) => {
  target.paramIndex = index;
});
const paramObj: Decoratable = {};
paramDecorator(paramObj, "execute");
emit("param", paramObj.paramIndex);

// 5. __propKey - Property key normalization
emit("propKey-number", __propKey(42));
emit("propKey-string", __propKey("fooKey"));

// 6. __setFunctionName - Function name modifier
function sampleFunction(): void {}
__setFunctionName(sampleFunction, "compute", "get");
emit("setFunctionName", sampleFunction.name);

// 7. __values & __read - Iteration helpers
const iteratedValues: number[] = [];
for (const v of __values([100, 200, 300])) {
  if (typeof v === "number") {
    iteratedValues.push(v);
  }
}
emit("values", iteratedValues);
emit("read-sliced", __read([1, 2, 3, 4, 5], 3));

// 8. __spreadArray - Array spreading
const spreadResult = __spreadArray(["a", "b"], ["c", "d"], true);
emit("spreadArray", spreadResult);

// 9. __makeTemplateObject - Tagged template helper
const templateObj = __makeTemplateObject(["select ", " from table"], ["select ", " from table"]);
emit("makeTemplateObject-cooked", templateObj);
emit("makeTemplateObject-raw", templateObj.raw);

// 10. __importStar & __importDefault - ES/CJS interop helpers
const starResult = __importStar({ x: 10, default: "default-export" });
emit("importStar-keys", Object.keys(starResult).sort());

const defaultResultModule = __importDefault({ __esModule: true, default: "module-default" });
emit("importDefault-esModule", defaultResultModule);

const defaultResultCJS = __importDefault({ val: "cjs-value" });
emit("importDefault-cjs", defaultResultCJS);

// 11. Private field helpers (__classPrivateFieldGet / Set / In)
const fieldReceiver = {};
const fieldState = new WeakMap<object, number>();
fieldState.set(fieldReceiver, 42);

emit("privateFieldGet", __classPrivateFieldGet(fieldReceiver, fieldState, "f"));
__classPrivateFieldSet(fieldReceiver, fieldState, 999, "f");
emit("privateFieldSet", __classPrivateFieldGet(fieldReceiver, fieldState, "f"));
emit("privateFieldIn", __classPrivateFieldIn(fieldState, fieldReceiver));

// 12. __addDisposableResource & __disposeResources - Explicit Resource Management
const disposalLog: string[] = [];
const resourceEnv = { stack: [], error: void 0, hasError: false };
const disposable = {
  [Symbol.dispose]() {
    disposalLog.push("disposed-cleanly");
  },
};
__addDisposableResource(resourceEnv, disposable, false);
__disposeResources(resourceEnv);
emit("disposalLog", disposalLog);

// 13. __rewriteRelativeImportExtension - Import extension rewriting
emit("rewriteExt-ts", __rewriteRelativeImportExtension("./module.ts", false));
emit("rewriteExt-tsx", __rewriteRelativeImportExtension("./component.tsx", false));
emit("rewriteExt-tsx-preserve", __rewriteRelativeImportExtension("./component.tsx", true));
emit("rewriteExt-mts", __rewriteRelativeImportExtension("./script.mts", false));
emit("rewriteExt-cts", __rewriteRelativeImportExtension("./script.cts", false));
