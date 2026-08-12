//! The checker-visible names supplied by the target's intrinsic environment.
//!
//! This is deliberately a closed inventory. A failed lookup remains a checker
//! error; adding a name here is the explicit compatibility decision.

/// Module-scoped host bindings selected by compiler options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModuleEnvironment {
    Standard,
    CommonJs,
}

/// Names installed before source declarations are bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GlobalEnvironment {
    values: &'static [&'static str],
    types: &'static [&'static str],
    module: ModuleEnvironment,
}

impl GlobalEnvironment {
    #[must_use]
    pub(super) const fn standard() -> Self {
        Self {
            values: STANDARD_VALUES,
            types: STANDARD_TYPES,
            module: ModuleEnvironment::Standard,
        }
    }

    #[must_use]
    pub(super) const fn commonjs() -> Self {
        Self {
            values: STANDARD_VALUES,
            types: STANDARD_TYPES,
            module: ModuleEnvironment::CommonJs,
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

    #[must_use]
    pub(super) const fn module_values(self) -> &'static [&'static str] {
        match self.module {
            ModuleEnvironment::Standard => &[],
            ModuleEnvironment::CommonJs => COMMONJS_WRAPPER_VALUES,
        }
    }
}

const COMMONJS_WRAPPER_VALUES: &[&str] =
    &["module", "exports", "require", "__filename", "__dirname"];

// `Function` is the sole declaration-only value. It permits library source
// checking without claiming that the runtime installs the constructor.
const STANDARD_VALUES: &[&str] = &[
    "undefined",
    "NaN",
    "Infinity",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "decodeURIComponent",
    "encodeURIComponent",
    "unescape",
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
    "Date",
    "String",
    "RegExp",
    "Array",
    "Uint8Array",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "Atomics",
    "JSON",
    "Math",
    "Promise",
    "SuppressedError",
    "global",
    "globalThis",
    "structuredClone",
    "console",
    "process",
    "setTimeout",
    "clearTimeout",
    "setInterval",
    "clearInterval",
    "queueMicrotask",
];

// These are the library names the checker can recognize in a type position.
// The current type algebra intentionally models their details as recovery
// types, but their presence remains explicit and separate from value bindings.
const STANDARD_TYPES: &[&str] = &[
    "Boolean",
    "Number",
    "String",
    "Symbol",
    "BigInt",
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
    "SuppressedError",
    "Object",
    "Function",
    "CallableFunction",
    "NewableFunction",
    "Iterable",
    "Iterator",
    "AsyncIterable",
    "IterableIterator",
    "AsyncIterableIterator",
    "AsyncIterator",
    "Generator",
    "AsyncGenerator",
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
    "AbortSignal",
    "URL",
    "URLSearchParams",
    "TextEncoder",
    "TextDecoder",
];
