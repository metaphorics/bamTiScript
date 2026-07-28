//! The checker-visible names supplied by the target's intrinsic environment.
//!
//! This is deliberately a closed inventory. A failed lookup remains a checker
//! error; adding a name here is the explicit compatibility decision.

/// Names installed before source declarations are bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GlobalEnvironment {
    values: &'static [&'static str],
    types: &'static [&'static str],
}

impl GlobalEnvironment {
    #[must_use]
    pub(super) const fn standard() -> Self {
        Self {
            values: STANDARD_VALUES,
            types: STANDARD_TYPES,
        }
    }

    #[must_use]
    pub(super) const fn values(self) -> &'static [&'static str] {
        self.values
    }

    #[must_use]
    pub(super) const fn types(self) -> &'static [&'static str] {
        self.types
    }
}

// ECMAScript globals, plus the Web-compatible APIs the target accepts and the
// Node host bindings its runtime actually installs (`console` and `process`).
const STANDARD_VALUES: &[&str] = &[
    "undefined",
    "NaN",
    "Infinity",
    "eval",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "Object",
    "Function",
    "Boolean",
    "Symbol",
    "Error",
    "AggregateError",
    "EvalError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "TypeError",
    "URIError",
    "Number",
    "BigInt",
    "Date",
    "String",
    "RegExp",
    "Array",
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "BigInt64Array",
    "BigUint64Array",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "ArrayBuffer",
    "SharedArrayBuffer",
    "DataView",
    "Atomics",
    "JSON",
    "Math",
    "Promise",
    "Proxy",
    "Reflect",
    "FinalizationRegistry",
    "WeakRef",
    "Intl",
    "globalThis",
    "structuredClone",
    "console",
    "process",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "queueMicrotask",
    "URL",
    "URLSearchParams",
    "TextEncoder",
    "TextDecoder",
    "TextEncoderStream",
    "TextDecoderStream",
];

// These are the library names the checker can recognize in a type position.
// The current type algebra intentionally models their details as recovery
// types, but their presence remains explicit and separate from value bindings.
const STANDARD_TYPES: &[&str] = &[
    "Array",
    "ReadonlyArray",
    "Readonly",
    "Partial",
    "Required",
    "Record",
    "Pick",
    "Omit",
    "Exclude",
    "Extract",
    "NonNullable",
    "ReturnType",
    "Parameters",
    "ConstructorParameters",
    "InstanceType",
    "ThisParameterType",
    "OmitThisParameter",
    "ThisType",
    "Awaited",
    "Uppercase",
    "Lowercase",
    "Capitalize",
    "Uncapitalize",
    "Promise",
    "PromiseLike",
    "Map",
    "ReadonlyMap",
    "Set",
    "ReadonlySet",
    "WeakMap",
    "WeakSet",
    "Date",
    "RegExp",
    "Error",
    "AggregateError",
    "EvalError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "TypeError",
    "URIError",
    "Object",
    "Function",
    "CallableFunction",
    "NewableFunction",
    "Iterable",
    "Iterator",
    "AsyncIterable",
    "AsyncIterator",
    "ArrayLike",
    "ArrayBuffer",
    "ArrayBufferLike",
    "ArrayBufferView",
    "DataView",
    "SharedArrayBuffer",
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "BigInt64Array",
    "BigUint64Array",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "URL",
    "URLSearchParams",
    "TextEncoder",
    "TextDecoder",
];
