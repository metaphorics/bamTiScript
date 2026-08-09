use std::collections::BTreeMap;
use std::marker::PhantomData;

use bamts_bytecode::{EcmaString, EcmaStringBuilder};
use bamts_native::{Decoded, Value};

use crate::{EvalFailure, HeapEntry, Host, Machine, NativeCallable, PropertyMap, ThrowOrigin};

#[path = "builtins/mod.rs"]
pub(crate) mod builtins;

#[path = "regexp.rs"]
mod regexp;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BuiltinId(usize);

#[derive(Clone, Debug)]
pub(crate) enum BuiltinOutcome {
    Value(Value),
    Call {
        callee: Value,
        this_value: Value,
        arguments: Vec<Value>,
    },
    GeneratorNext {
        generator: Value,
        resume_value: Value,
    },
    AsyncGeneratorNext {
        generator: Value,
        resume_value: Value,
    },
}

pub(crate) type BuiltinHandler<H> = fn(
    &mut Machine<'_, H>,
    this: Value,
    args: &[Value],
    constructing: bool,
) -> Result<BuiltinOutcome, EvalFailure>;

#[derive(Clone, Copy)]
pub(crate) struct BuiltinDef<H: Host> {
    pub(crate) name: &'static str,
    pub(crate) length: u32,
    pub(crate) handler: BuiltinHandler<H>,
}

pub(crate) struct BuiltinTable<H: Host> {
    defs: Vec<BuiltinDef<H>>,
    object_prototype: Value,
    function_prototype: Value,
    array_prototype: Value,
    string_prototype: Value,
    number_prototype: Value,
    boolean_prototype: Value,
    error_prototypes: Vec<(BuiltinId, Value)>,
    symbol_iterator: Option<Value>,
    symbol_async_iterator: Option<Value>,
    symbol_to_string_tag: Option<Value>,
    symbol_species: Option<Value>,
    symbol_dispose: Option<Value>,
    symbol_async_dispose: Option<Value>,
    symbol_unscopables: Option<Value>,
    symbol_prototype: Option<Value>,
    object_to_string: Option<Value>,
    regexp_prototype: Option<Value>,
    iterator_prototype: Option<Value>,
    async_iterator_prototype: Option<Value>,
    generator_prototype: Option<Value>,
    async_generator_prototype: Option<Value>,
    promise_resolver_targets: Option<(Value, Value)>,
    promise_all_target: Option<Value>,
    promise_prototype: Option<Value>,
    uint8array_prototype: Option<Value>,
    promise_capability_executor: Option<Value>,
    promise_finally_value: Option<Value>,
    promise_finally_throw: Option<Value>,
    promise_finally_return: Option<Value>,
    promise_finally_rethrow: Option<Value>,
    promise_then_fulfill: Option<Value>,
    promise_then_reject: Option<Value>,
    marker: PhantomData<fn() -> H>,
}

impl<H: Host> BuiltinTable<H> {
    fn new(
        object_prototype: Value,
        function_prototype: Value,
        array_prototype: Value,
        string_prototype: Value,
        number_prototype: Value,
        boolean_prototype: Value,
    ) -> Self {
        Self {
            defs: Vec::new(),
            object_prototype,
            function_prototype,
            array_prototype,
            string_prototype,
            number_prototype,
            boolean_prototype,
            error_prototypes: Vec::new(),
            symbol_iterator: None,
            symbol_async_iterator: None,
            symbol_to_string_tag: None,
            symbol_species: None,
            symbol_dispose: None,
            symbol_async_dispose: None,
            symbol_unscopables: None,
            symbol_prototype: None,
            object_to_string: None,
            regexp_prototype: None,
            iterator_prototype: None,
            async_iterator_prototype: None,
            generator_prototype: None,
            async_generator_prototype: None,
            promise_resolver_targets: None,
            promise_all_target: None,
            promise_prototype: None,
            uint8array_prototype: None,
            promise_capability_executor: None,
            promise_finally_value: None,
            promise_finally_throw: None,
            promise_finally_return: None,
            promise_finally_rethrow: None,
            promise_then_fulfill: None,
            promise_then_reject: None,
            marker: PhantomData,
        }
    }

    pub(crate) fn register(&mut self, def: BuiltinDef<H>) -> BuiltinId {
        let id = BuiltinId(self.defs.len());
        self.defs.push(def);
        id
    }

    pub(crate) fn get(&self, id: BuiltinId) -> &BuiltinDef<H> {
        self.defs
            .get(id.0)
            .expect("BuiltinId is minted by this realm's table")
    }

    pub(crate) fn object_prototype(&self) -> Value {
        self.object_prototype
    }

    pub(crate) fn function_prototype(&self) -> Value {
        self.function_prototype
    }

    pub(crate) fn array_prototype(&self) -> Value {
        self.array_prototype
    }

    pub(crate) fn string_prototype(&self) -> Value {
        self.string_prototype
    }

    pub(crate) fn number_prototype(&self) -> Value {
        self.number_prototype
    }

    pub(crate) fn boolean_prototype(&self) -> Value {
        self.boolean_prototype
    }

    pub(crate) fn set_symbol_iterator(&mut self, iterator: Value) {
        self.symbol_iterator = Some(iterator);
    }

    pub(crate) fn symbol_iterator(&self) -> Value {
        self.symbol_iterator.expect("Symbol builtins install first")
    }

    pub(crate) fn set_symbol_async_iterator(&mut self, iterator: Value) {
        self.symbol_async_iterator = Some(iterator);
    }

    pub(crate) fn symbol_async_iterator(&self) -> Value {
        self.symbol_async_iterator
            .expect("Symbol builtins install first")
    }

    pub(crate) fn set_symbol_dispose(&mut self, symbol: Value) {
        self.symbol_dispose = Some(symbol);
    }

    pub(crate) fn symbol_dispose(&self) -> Value {
        self.symbol_dispose.expect("Symbol builtins install first")
    }

    pub(crate) fn set_symbol_async_dispose(&mut self, symbol: Value) {
        self.symbol_async_dispose = Some(symbol);
    }

    pub(crate) fn symbol_async_dispose(&self) -> Value {
        self.symbol_async_dispose
            .expect("Symbol builtins install first")
    }

    pub(crate) fn set_symbol_unscopables(&mut self, symbol: Value) {
        self.symbol_unscopables = Some(symbol);
    }

    pub(crate) fn symbol_unscopables(&self) -> Value {
        self.symbol_unscopables
            .expect("Symbol builtins install first")
    }

    pub(crate) fn set_symbol_species(&mut self, symbol: Value) {
        self.symbol_species = Some(symbol);
    }

    pub(crate) fn symbol_species(&self) -> Value {
        self.symbol_species.expect("Symbol builtins install first")
    }

    pub(crate) fn set_symbol_to_string_tag(&mut self, symbol: Value) {
        self.symbol_to_string_tag = Some(symbol);
    }
    pub(crate) fn set_symbol_prototype(&mut self, prototype: Value) {
        self.symbol_prototype = Some(prototype);
    }

    pub(crate) fn symbol_prototype(&self) -> Value {
        self.symbol_prototype
            .expect("Symbol builtins install their prototype")
    }

    pub(crate) fn symbol_to_string_tag(&self) -> Value {
        self.symbol_to_string_tag
            .expect("Symbol builtins install first")
    }

    pub(crate) fn set_object_to_string(&mut self, function: Value) {
        self.object_to_string = Some(function);
    }

    pub(crate) fn object_to_string(&self) -> Value {
        self.object_to_string
            .expect("Object builtins install Object.prototype.toString")
    }

    pub(crate) fn set_regexp_prototype(&mut self, prototype: Value) {
        self.regexp_prototype = Some(prototype);
    }

    pub(crate) fn regexp_prototype(&self) -> Value {
        self.regexp_prototype
            .expect("RegExp builtins install their prototype")
    }

    pub(crate) fn set_iterator_prototype(&mut self, prototype: Value) {
        self.iterator_prototype = Some(prototype);
    }

    pub(crate) fn iterator_prototype(&self) -> Value {
        self.iterator_prototype
            .expect("iterator builtins install their prototype")
    }

    pub(crate) fn set_async_iterator_prototype(&mut self, prototype: Value) {
        self.async_iterator_prototype = Some(prototype);
    }

    pub(crate) fn async_iterator_prototype(&self) -> Value {
        self.async_iterator_prototype
            .expect("async iterator builtins install their prototype")
    }

    pub(crate) fn set_generator_prototype(&mut self, prototype: Value) {
        self.generator_prototype = Some(prototype);
    }

    pub(crate) fn generator_prototype(&self) -> Value {
        self.generator_prototype
            .expect("generator builtins install their prototype")
    }

    pub(crate) fn set_async_generator_prototype(&mut self, prototype: Value) {
        self.async_generator_prototype = Some(prototype);
    }

    pub(crate) fn async_generator_prototype(&self) -> Value {
        self.async_generator_prototype
            .expect("async generator builtins install their prototype")
    }

    pub(crate) fn set_promise_prototype(&mut self, prototype: Value) {
        self.promise_prototype = Some(prototype);
    }

    pub(crate) fn promise_prototype(&self) -> Value {
        self.promise_prototype
            .expect("Promise builtins install their prototype")
    }

    pub(crate) fn set_uint8array_prototype(&mut self, prototype: Value) {
        self.uint8array_prototype = Some(prototype);
    }

    pub(crate) fn uint8array_prototype(&self) -> Value {
        self.uint8array_prototype
            .expect("Uint8Array builtins install their prototype")
    }

    pub(crate) fn set_promise_capability_executor(&mut self, value: Value) {
        self.promise_capability_executor = Some(value);
    }

    pub(crate) fn promise_capability_executor(&self) -> Value {
        self.promise_capability_executor
            .expect("Promise builtins install capability executor")
    }

    pub(crate) fn set_promise_finally_value(&mut self, value: Value) {
        self.promise_finally_value = Some(value);
    }

    pub(crate) fn promise_finally_value(&self) -> Value {
        self.promise_finally_value
            .expect("Promise builtins install finally value")
    }

    pub(crate) fn set_promise_finally_throw(&mut self, value: Value) {
        self.promise_finally_throw = Some(value);
    }

    pub(crate) fn promise_finally_throw(&self) -> Value {
        self.promise_finally_throw
            .expect("Promise builtins install finally throw")
    }

    pub(crate) fn set_promise_finally_return(&mut self, value: Value) {
        self.promise_finally_return = Some(value);
    }

    pub(crate) fn promise_finally_return(&self) -> Value {
        self.promise_finally_return
            .expect("Promise builtins install finally return")
    }

    pub(crate) fn set_promise_finally_rethrow(&mut self, value: Value) {
        self.promise_finally_rethrow = Some(value);
    }

    pub(crate) fn promise_finally_rethrow(&self) -> Value {
        self.promise_finally_rethrow
            .expect("Promise builtins install finally rethrow")
    }

    pub(crate) fn set_promise_then_fulfill(&mut self, value: Value) {
        self.promise_then_fulfill = Some(value);
    }

    pub(crate) fn promise_then_fulfill(&self) -> Value {
        self.promise_then_fulfill
            .expect("Promise builtins install then fulfill")
    }

    pub(crate) fn set_promise_then_reject(&mut self, value: Value) {
        self.promise_then_reject = Some(value);
    }

    pub(crate) fn promise_then_reject(&self) -> Value {
        self.promise_then_reject
            .expect("Promise builtins install then reject")
    }

    pub(crate) fn set_promise_resolver_targets(&mut self, resolve: Value, reject: Value) {
        self.promise_resolver_targets = Some((resolve, reject));
    }

    pub(crate) fn promise_resolver_targets(&self) -> (Value, Value) {
        self.promise_resolver_targets
            .expect("Promise builtins install resolver targets")
    }

    pub(crate) fn set_promise_all_target(&mut self, fulfill: Value) {
        self.promise_all_target = Some(fulfill);
    }

    pub(crate) fn promise_all_target(&self) -> Value {
        self.promise_all_target
            .expect("Promise builtins install the all target")
    }

    pub(crate) fn set_constructor_prototype(
        &mut self,
        heap: &mut [HeapEntry],
        constructor: Value,
        prototype: Value,
    ) {
        let index = heap_index(constructor);
        let HeapEntry::NativeFunction { properties, .. } = &mut heap[index] else {
            panic!("builtin constructor is a native function");
        };
        properties.insert(
            crate::PropertyKey::Named(EcmaString::encode("prototype")),
            crate::Property::Data {
                value: prototype,
                writable: false,
                enumerable: false,
                configurable: false,
            },
        );
        let prototype_index = heap_index(prototype);
        let constructor_property = crate::Property::Data {
            value: constructor,
            writable: true,
            enumerable: false,
            configurable: true,
        };
        match &mut heap[prototype_index] {
            HeapEntry::Object { properties, .. } | HeapEntry::Array { properties, .. } => {
                properties.insert(
                    crate::PropertyKey::Named(EcmaString::encode("constructor")),
                    constructor_property,
                );
            }
            _ => panic!("builtin prototype must be an ordinary object or array"),
        }
    }

    pub(crate) fn set_function_prototype(
        &mut self,
        heap: &mut [HeapEntry],
        function: Value,
        prototype: Value,
    ) {
        let index = heap_index(function);
        let HeapEntry::NativeFunction { properties, .. } = &mut heap[index] else {
            panic!("builtin function is a native function");
        };
        properties.insert(
            crate::PropertyKey::Named(EcmaString::encode("prototype")),
            crate::Property::Data {
                value: prototype,
                writable: true,
                enumerable: false,
                configurable: false,
            },
        );
    }

    pub(crate) fn set_error_prototype(
        &mut self,
        heap: &mut [HeapEntry],
        constructor: Value,
        prototype: Value,
    ) {
        let index = heap_index(constructor);
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(id),
            ..
        } = heap[index]
        else {
            panic!("error constructor is a native function");
        };
        self.error_prototypes.push((id, prototype));
    }

    pub(crate) fn id_named(&self, name: &str) -> Option<BuiltinId> {
        self.defs
            .iter()
            .position(|definition| definition.name == name)
            .map(BuiltinId)
    }
    fn for_each_value(&self, mut visit: impl FnMut(Value)) {
        let Self {
            defs: _,
            object_prototype,
            function_prototype,
            array_prototype,
            string_prototype,
            number_prototype,
            boolean_prototype,
            error_prototypes,
            symbol_iterator,
            symbol_async_iterator,
            symbol_to_string_tag,
            symbol_species,
            symbol_dispose,
            symbol_async_dispose,
            symbol_unscopables,
            symbol_prototype,
            object_to_string,
            regexp_prototype,
            iterator_prototype,
            async_iterator_prototype,
            generator_prototype,
            async_generator_prototype,
            promise_resolver_targets,
            promise_all_target,
            promise_prototype,
            uint8array_prototype,
            promise_capability_executor,
            promise_finally_value,
            promise_finally_throw,
            promise_finally_return,
            promise_finally_rethrow,
            promise_then_fulfill,
            promise_then_reject,
            marker: _,
        } = self;

        for value in [
            *object_prototype,
            *function_prototype,
            *array_prototype,
            *string_prototype,
            *number_prototype,
            *boolean_prototype,
        ] {
            visit(value);
        }
        for (_, value) in error_prototypes {
            visit(*value);
        }
        for value in [
            *symbol_iterator,
            *symbol_async_iterator,
            *symbol_to_string_tag,
            *symbol_species,
            *symbol_dispose,
            *symbol_async_dispose,
            *symbol_unscopables,
            *symbol_prototype,
            *object_to_string,
            *regexp_prototype,
            *iterator_prototype,
            *async_iterator_prototype,
            *generator_prototype,
            *async_generator_prototype,
            *promise_all_target,
            *promise_prototype,
            *uint8array_prototype,
            *promise_capability_executor,
            *promise_finally_value,
            *promise_finally_throw,
            *promise_finally_return,
            *promise_finally_rethrow,
            *promise_then_fulfill,
            *promise_then_reject,
        ]
        .into_iter()
        .flatten()
        {
            visit(value);
        }
        if let Some((resolve, reject)) = *promise_resolver_targets {
            visit(resolve);
            visit(reject);
        }
    }
}

pub(crate) struct Intrinsics<H: Host> {
    pub(crate) globals: BTreeMap<EcmaString, Value>,
    pub(crate) symbol_registry: BTreeMap<EcmaString, Value>,
    pub(crate) object_prototype: Value,
    pub(crate) function_prototype: Value,
    pub(crate) array_prototype: Value,
    pub(crate) string_prototype: Value,
    pub(crate) number_prototype: Value,
    pub(crate) boolean_prototype: Value,
    pub(crate) builtins: BuiltinTable<H>,
}

impl<H: Host> Intrinsics<H> {
    pub(crate) fn initialize(heap: &mut Vec<HeapEntry>, timers_available: bool) -> Self {
        let object_prototype = push(
            heap,
            HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: None,
                extensible: true,
                boxed_primitive: None,
            },
        );
        let function_prototype = ordinary_prototype(heap, object_prototype);
        let array_prototype = push(
            heap,
            HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(object_prototype),
                extensible: true,
                length_writable: true,
            },
        );
        let string_prototype = ordinary_prototype(heap, object_prototype);
        let number_prototype = ordinary_prototype(heap, object_prototype);
        let boolean_prototype = ordinary_prototype(heap, object_prototype);
        let mut globals = BTreeMap::new();
        let mut builtins = BuiltinTable::new(
            object_prototype,
            function_prototype,
            array_prototype,
            string_prototype,
            number_prototype,
            boolean_prototype,
        );
        builtins::install(heap, &mut globals, &mut builtins, timers_available);
        crate::host_objects::install(heap, &mut globals, &mut builtins);

        Self {
            globals,
            symbol_registry: BTreeMap::new(),
            object_prototype,
            function_prototype,
            array_prototype,
            string_prototype,
            number_prototype,
            boolean_prototype,
            builtins,
        }
    }

    pub(crate) fn global(&self, name: &str) -> Option<Value> {
        debug_assert!(name.is_ascii());
        // The previous scan used `eq_ascii`, which returns false for every
        // candidate when `name` is non-ASCII, so a non-ASCII lookup always
        // yielded `None`. Preserve that contract by bailing out before the
        // keyed lookup, which would otherwise match a non-ASCII global.
        if !name.is_ascii() {
            return None;
        }
        // `EcmaString::encode` encodes ASCII as the identical u16 code
        // units that `eq_ascii` compared against, and BTreeMap keys are
        // unique, so `get` returns the same single match the scan did — in
        // O(log n) instead of walking every global on each construction.
        self.globals.get(&EcmaString::encode(name)).copied()
    }

    pub(crate) fn regexp_prototype(&self) -> Value {
        self.builtins.regexp_prototype()
    }

    pub(crate) fn error_prototype(&self, id: BuiltinId) -> Value {
        self.builtins
            .error_prototypes
            .iter()
            .find_map(|(candidate, prototype)| (*candidate == id).then_some(*prototype))
            .expect("every error builtin has a realm prototype")
    }

    pub(crate) fn object_to_string(&self) -> Value {
        self.builtins.object_to_string()
    }
    pub(crate) fn for_each_value(&self, mut visit: impl FnMut(Value)) {
        for value in self.globals.values().chain(self.symbol_registry.values()) {
            visit(*value);
        }
        visit(self.object_prototype);
        visit(self.function_prototype);
        visit(self.array_prototype);
        visit(self.string_prototype);
        visit(self.number_prototype);
        visit(self.boolean_prototype);
        self.builtins.for_each_value(visit);
    }
}

fn ordinary_prototype(heap: &mut Vec<HeapEntry>, object_prototype: Value) -> Value {
    push(
        heap,
        HeapEntry::Object {
            properties: PropertyMap::default(),
            prototype: Some(object_prototype),
            extensible: true,
            boxed_primitive: None,
        },
    )
}

pub(crate) fn native_function(
    heap: &mut Vec<HeapEntry>,
    id: BuiltinId,
    name: &'static str,
    length: u32,
) -> Value {
    let name_value = push(heap, HeapEntry::String(EcmaString::encode(name)));
    let mut properties = PropertyMap::default();
    properties.insert(
        crate::PropertyKey::Named(EcmaString::encode("length")),
        crate::Property::Data {
            value: crate::number_value(f64::from(length)),
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        crate::PropertyKey::Named(EcmaString::encode("name")),
        crate::Property::Data {
            value: name_value,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    push(
        heap,
        HeapEntry::native_function(NativeCallable::Builtin(id), properties, None),
    )
}

pub(crate) fn push(heap: &mut Vec<HeapEntry>, entry: HeapEntry) -> Value {
    heap.push(entry);
    let slot = u32::try_from(heap.len()).expect("intrinsic heap fits in a u32 slot");
    Value::heap_ref(
        bamts_native::SlotId::from_parts(crate::RUNTIME_HEAP_SEGMENT, slot)
            .expect("intrinsic slot is nonzero"),
    )
}
fn heap_index(value: Value) -> usize {
    let Some(Decoded::HeapRef(id)) = value.decode() else {
        panic!("intrinsic value is a heap reference");
    };
    id.slot() as usize - 1
}

impl<'a, H: Host> Machine<'a, H> {
    pub(crate) fn call_builtin(
        &mut self,
        id: BuiltinId,
        this_value: Value,
        arguments: &[Value],
        constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        self.call_builtin_with_new_target(id, this_value, arguments, constructing, Value::UNDEFINED)
    }

    pub(crate) fn call_builtin_with_new_target(
        &mut self,
        id: BuiltinId,
        this_value: Value,
        arguments: &[Value],
        constructing: bool,
        new_target: Value,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let handler = self.intrinsics.builtins.get(id).handler;
        let previous_id = self.current_builtin_id.replace(id);
        let previous_new_target = std::mem::replace(&mut self.current_new_target, new_target);
        let outcome = handler(self, this_value, arguments, constructing);
        self.current_builtin_id = previous_id;
        self.current_new_target = previous_new_target;
        outcome
    }

    fn object_to_string_tag(&self, value: Value) -> Result<&'static str, EvalFailure> {
        match value.decode() {
            Some(Decoded::Undefined | Decoded::Uninitialized | Decoded::Hole) | None => {
                Ok("Undefined")
            }
            Some(Decoded::Null) => Ok("Null"),
            Some(Decoded::Boolean(_)) => Ok("Boolean"),
            Some(Decoded::Number(_) | Decoded::Int32(_)) => Ok("Number"),
            Some(Decoded::HeapRef(_)) => {
                let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)? else {
                    return Ok("Object");
                };
                Ok(match &self.heap[index] {
                    HeapEntry::String(_) => "String",
                    HeapEntry::Array { .. } => "Array",
                    HeapEntry::Function { .. } | HeapEntry::NativeFunction { .. } => "Function",
                    HeapEntry::RegExp { .. } => "RegExp",
                    HeapEntry::BigInt(_) => "BigInt",
                    HeapEntry::PrivateName { .. } => "Symbol",
                    HeapEntry::Date { .. } => "Date",
                    HeapEntry::Object { .. } if self.is_error_object(index)? => "Error",
                    _ => "Object",
                })
            }
        }
    }

    fn is_error_object(&self, mut index: usize) -> Result<bool, EvalFailure> {
        for _ in 0..=self.heap.len() {
            let value = Value::heap_ref(
                bamts_native::SlotId::from_parts(
                    crate::RUNTIME_HEAP_SEGMENT,
                    u32::try_from(index + 1).expect("heap index fits in u32"),
                )
                .expect("heap index is nonzero"),
            );
            if self
                .intrinsics
                .builtins
                .error_prototypes
                .iter()
                .any(|(_, prototype)| *prototype == value)
            {
                return Ok(true);
            }
            match self.prototype_index(index)? {
                Some(next) => index = next,
                None => return Ok(false),
            }
        }
        Ok(false)
    }

    pub fn ordinary_number_to_string(number: f64) -> String {
        crate::format_number(number)
    }

    pub(crate) fn to_string(&self, value: Value) -> Result<EcmaString, EvalFailure> {
        self.value_to_string(value, 0)
    }

    pub(crate) fn string_constructor_text(
        &mut self,
        value: Value,
    ) -> Result<EcmaString, EvalFailure> {
        if let Some(index) = self.runtime_slot(value).map_err(EvalFailure::Runtime)?
            && let HeapEntry::Symbol { description } = &self.heap[index]
        {
            let mut text =
                EcmaStringBuilder::with_capacity(description.len_units().saturating_add(8));
            text.push_utf8("Symbol(");
            for &unit in description.as_units() {
                text.push_unit(unit);
            }
            text.push_unit(u16::from(b')'));
            return Ok(text.finish());
        }

        if !self.is_object(value) {
            return self.to_string(value);
        }

        for name in ["toString", "valueOf"] {
            let method = self.get_named_property(value, name)?;
            if !self.is_callable(method)? {
                continue;
            }
            let primitive = self.call_value(method, value, &[])?;
            if !self.is_object(primitive) {
                return self.to_string(primitive);
            }
        }

        Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "cannot convert object to primitive without invoking user code",
        }))
    }
    pub(crate) fn to_boolean(&self, value: Value) -> bool {
        self.truthy(value)
    }

    pub fn same_value_zero(&self, left: Value, right: Value) -> bool {
        match (left.decode(), right.decode()) {
            (Some(Decoded::Number(a)), Some(Decoded::Number(b))) => {
                a == b || (a.is_nan() && b.is_nan())
            }
            (Some(Decoded::Number(a)), Some(Decoded::Int32(b)))
            | (Some(Decoded::Int32(b)), Some(Decoded::Number(a))) => a == f64::from(b as i32),
            _ => self.strict_equal(left, right),
        }
    }

    pub(crate) fn to_primitive(&self, value: Value) -> Result<Value, EvalFailure> {
        if !self.is_object(value) {
            return Ok(value);
        }
        Err(EvalFailure::Throw(ThrowOrigin::TypeError {
            operation: "cannot convert object to primitive without invoking user code",
        }))
    }
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        Constant, ConstantId, Function, FunctionFlags, FunctionId, Instruction, Module, ModuleId,
        Program, ProgramModule, Verified,
    };

    use super::*;
    use crate::{Limits, Property, PropertyKey};

    #[derive(Default)]
    struct TestHost;
    impl Host for TestHost {}

    fn module() -> Program<Verified> {
        let code = Module::new(
            vec![Constant::String(EcmaString::encode("<test>"))],
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![Instruction::Halt],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .expect("valid test module");
        Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("valid test program")
    }

    fn call_static(
        machine: &mut Machine<'_, TestHost>,
        constructor: &str,
        method: &str,
        arguments: &[Value],
    ) -> Value {
        let constructor = machine
            .intrinsics
            .global(constructor)
            .expect("global exists");
        let method = machine
            .get_named_property(constructor, method)
            .expect("method exists");
        machine
            .call_value(method, constructor, arguments)
            .expect("builtin call succeeds")
    }

    #[test]
    fn builtin_table_root_walker_visits_every_cached_callback() {
        let mut table = BuiltinTable::<TestHost>::new(
            Value::int32(1),
            Value::int32(2),
            Value::int32(3),
            Value::int32(4),
            Value::int32(5),
            Value::int32(6),
        );
        let roots = [
            Value::int32(101),
            Value::int32(102),
            Value::int32(103),
            Value::int32(104),
            Value::int32(105),
            Value::int32(106),
            Value::int32(107),
            Value::int32(108),
        ];
        table.uint8array_prototype = Some(roots[0]);
        table.promise_capability_executor = Some(roots[1]);
        table.promise_finally_value = Some(roots[2]);
        table.promise_finally_throw = Some(roots[3]);
        table.promise_finally_return = Some(roots[4]);
        table.promise_finally_rethrow = Some(roots[5]);
        table.promise_then_fulfill = Some(roots[6]);
        table.promise_then_reject = Some(roots[7]);

        let mut visited = Vec::new();
        table.for_each_value(|value| visited.push(value));

        for root in roots {
            assert!(
                visited.contains(&root),
                "cached root {root:?} was not traced"
            );
        }
    }

    #[test]
    fn corpus_value_builtin_oracles_match_node_24_bytes() {
        // Byte-exact outputs captured with Node v24.18.0. The labels name the
        // corpus programs whose observable operation each row exercises.
        let expected = [
            ("destr: JSON.stringify parsed object", "{\"test\":123}"),
            ("dot-prop: Object.hasOwn", "true"),
            ("defu: Object.assign key order", "1,2,b,a"),
            ("valita: Array.isArray", "true"),
        ];
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let object = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        machine
            .set_data_property(object, "test", Value::int32(123))
            .unwrap();
        let json = machine.intrinsics.global("JSON").unwrap();
        let stringify = machine.get_named_property(json, "stringify").unwrap();
        let json_text = machine.call_value(stringify, json, &[object]).unwrap();

        let test_key = machine
            .allocate(HeapEntry::String(EcmaString::encode("test")))
            .unwrap();
        let has_own = call_static(&mut machine, "Object", "hasOwn", &[object, test_key]);

        let ordered = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        for (key, value) in [("b", 1), ("2", 2), ("a", 3), ("1", 4)] {
            let index = machine.runtime_slot(ordered).unwrap().unwrap();
            let HeapEntry::Object { properties, .. } = &mut machine.heap[index] else {
                unreachable!()
            };
            properties.insert(
                PropertyKey::Named(EcmaString::encode(key)),
                Property::Data {
                    value: Value::int32(value),
                    writable: true,
                    enumerable: true,
                    configurable: true,
                },
            );
        }
        let keys = call_static(&mut machine, "Object", "keys", &[ordered]);
        let array = machine
            .allocate(HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();
        let is_array = call_static(&mut machine, "Array", "isArray", &[array]);

        let actual = [
            machine.to_string(json_text).unwrap(),
            machine.to_string(has_own).unwrap(),
            machine.to_string(keys).unwrap(),
            machine.to_string(is_array).unwrap(),
        ];
        for ((label, expected), actual) in expected.into_iter().zip(actual) {
            assert!(actual.eq_ascii(expected), "{label}: {actual:?}");
        }
    }

    fn construct_builtin(
        machine: &mut Machine<'_, TestHost>,
        name: &str,
        arguments: &[Value],
    ) -> Value {
        let constructor = machine.intrinsics.global(name).expect("global exists");
        let index = machine.runtime_slot(constructor).unwrap().unwrap();
        let HeapEntry::NativeFunction {
            callable: NativeCallable::Builtin(id),
            ..
        } = machine.heap[index]
        else {
            panic!("constructor is native")
        };
        let BuiltinOutcome::Value(value) = machine
            .call_builtin(id, Value::UNDEFINED, arguments, true)
            .unwrap()
        else {
            panic!("constructor returns a value")
        };
        value
    }

    fn next_value(machine: &mut Machine<'_, TestHost>, iterator: Value) -> (Value, bool) {
        let next = machine.get_named_property(iterator, "next").unwrap();
        let result = machine.call_value(next, iterator, &[]).unwrap();
        let value = machine.get_named_property(result, "value").unwrap();
        let done = machine.get_named_property(result, "done").unwrap();
        (value, machine.to_boolean(done))
    }

    #[test]
    fn collections_symbols_errors_regexp_and_date_match_node_24_observables() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let symbol = machine.intrinsics.global("Symbol").unwrap();
        let symbol_for = machine.get_named_property(symbol, "for").unwrap();
        let key_text = machine
            .allocate(HeapEntry::String(EcmaString::encode("shared")))
            .unwrap();
        let first = machine.call_value(symbol_for, symbol, &[key_text]).unwrap();
        let second = machine.call_value(symbol_for, symbol, &[key_text]).unwrap();
        assert_eq!(first, second, "Symbol.for registry identity");

        let map = construct_builtin(&mut machine, "Map", &[]);
        let set = machine.get_named_property(map, "set").unwrap();
        machine
            .call_value(set, map, &[Value::int32(2), Value::int32(20)])
            .unwrap();
        machine
            .call_value(set, map, &[Value::int32(1), Value::int32(10)])
            .unwrap();
        let keys = machine.get_named_property(map, "keys").unwrap();
        let iterator = machine.call_value(keys, map, &[]).unwrap();
        let next = machine.get_named_property(iterator, "next").unwrap();
        let first_result = machine.call_value(next, iterator, &[]).unwrap();
        let second_result = machine.call_value(next, iterator, &[]).unwrap();
        assert_eq!(
            machine.get_named_property(first_result, "value").unwrap(),
            Value::int32(2)
        );
        assert_eq!(
            machine.get_named_property(second_result, "value").unwrap(),
            Value::int32(1)
        );

        let pattern = machine
            .allocate(HeapEntry::String(EcmaString::encode("^(a|b)\\.js$")))
            .unwrap();
        let regexp = construct_builtin(&mut machine, "RegExp", &[pattern]);
        let test = machine.get_named_property(regexp, "test").unwrap();
        let input = machine
            .allocate(HeapEntry::String(EcmaString::encode("b.js")))
            .unwrap();
        assert_eq!(
            machine.call_value(test, regexp, &[input]).unwrap(),
            Value::TRUE
        );

        let message = machine
            .allocate(HeapEntry::String(EcmaString::encode("boom")))
            .unwrap();
        let error = construct_builtin(&mut machine, "TypeError", &[message]);
        let error_message = machine.get_named_property(error, "message").unwrap();
        assert!(machine.to_string(error_message).unwrap().eq_ascii("boom"));
        let stack = machine.get_named_property(error, "stack").unwrap();
        let stack = machine
            .to_string(stack)
            .unwrap()
            .to_utf8_strict()
            .expect("error stack is well-formed UTF-16");
        assert!(stack.starts_with("TypeError: boom"));

        let date = construct_builtin(&mut machine, "Date", &[Value::int32(0)]);
        let object_to_string = machine.intrinsics.object_to_string();
        let date_tag = machine.call_value(object_to_string, date, &[]).unwrap();
        assert!(
            machine
                .string_value(date_tag)
                .is_some_and(|text| text.eq_ascii("[object Date]"))
        );
        let to_iso = machine.get_named_property(date, "toISOString").unwrap();
        let iso = machine.call_value(to_iso, date, &[]).unwrap();
        assert!(
            machine
                .to_string(iso)
                .unwrap()
                .eq_ascii("1970-01-01T00:00:00.000Z")
        );
    }

    #[test]
    fn realm_handles_never_enter_public_globals() {
        let module = module();
        let mut host = TestHost;
        let machine = Machine::new(&module, &mut host, Limits::default());
        assert!(
            machine
                .intrinsics
                .globals
                .keys()
                .all(|name| name.as_units().first() != Some(&0))
        );

        let global_this = machine
            .intrinsics
            .global("globalThis")
            .expect("globalThis is installed");
        let keys = machine
            .own_property_keys(global_this)
            .expect("globalThis is an object");
        assert!(keys.into_iter().all(|key| {
            key.as_string()
                .is_none_or(|name| name.as_units().first() != Some(&0))
        }));
    }

    #[test]
    fn date_state_is_typed_and_unforgeable() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let date = construct_builtin(&mut machine, "Date", &[Value::int32(0)]);
        assert!(machine.own_property_keys(date).unwrap().is_empty());
        let get_time = machine.get_named_property(date, "getTime").unwrap();

        machine
            .set_data_property(date, "\0Date.value", Value::int32(99))
            .unwrap();
        assert_eq!(
            machine.call_value(get_time, date, &[]).unwrap(),
            Value::int32(0)
        );

        let derived = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(date),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        assert!(machine.call_value(get_time, derived, &[]).is_err());

        let structured_clone = machine.intrinsics.global("structuredClone").unwrap();
        let clone = machine
            .call_value(structured_clone, Value::UNDEFINED, &[date])
            .unwrap();
        assert_eq!(
            machine.call_value(get_time, clone, &[]).unwrap(),
            Value::int32(0)
        );
        assert!(machine.own_property_keys(clone).unwrap().is_empty());

        let pair = machine
            .allocate(HeapEntry::Array {
                elements: vec![date, date],
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();
        let pair_clone = machine
            .call_value(structured_clone, Value::UNDEFINED, &[pair])
            .unwrap();
        let pair_index = machine.runtime_slot(pair_clone).unwrap().unwrap();
        let HeapEntry::Array { elements, .. } = &machine.heap[pair_index] else {
            panic!("cloned pair remains an array")
        };
        assert_eq!(elements[0], elements[1]);
    }

    #[test]
    fn builtin_iterators_keep_typed_live_state() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let array = machine
            .allocate(HeapEntry::Array {
                elements: vec![Value::HOLE, Value::int32(1)],
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.array_prototype),
                extensible: true,
                length_writable: true,
            })
            .unwrap();

        let values = machine.get_named_property(array, "values").unwrap();
        let values_iterator = machine.call_value(values, array, &[]).unwrap();
        assert!(
            machine
                .own_property_keys(values_iterator)
                .unwrap()
                .is_empty()
        );
        machine
            .set_data_property(values_iterator, "\0iterator.index", Value::int32(99))
            .unwrap();
        assert_eq!(
            next_value(&mut machine, values_iterator),
            (Value::UNDEFINED, false)
        );
        assert_eq!(
            next_value(&mut machine, values_iterator),
            (Value::int32(1), false)
        );
        assert_eq!(
            next_value(&mut machine, values_iterator),
            (Value::UNDEFINED, true)
        );
        machine
            .set_data_property(array, "2", Value::int32(2))
            .unwrap();
        assert_eq!(
            next_value(&mut machine, values_iterator),
            (Value::UNDEFINED, true)
        );

        let keys = machine.get_named_property(array, "keys").unwrap();
        let keys_iterator = machine.call_value(keys, array, &[]).unwrap();
        machine
            .set_data_property(array, "3", Value::int32(3))
            .unwrap();
        for expected in 0..4 {
            assert_eq!(
                next_value(&mut machine, keys_iterator),
                (Value::int32(expected), false)
            );
        }
        assert_eq!(
            next_value(&mut machine, keys_iterator),
            (Value::UNDEFINED, true)
        );

        let entries = machine.get_named_property(array, "entries").unwrap();
        let entries_iterator = machine.call_value(entries, array, &[]).unwrap();
        let (first_entry, done) = next_value(&mut machine, entries_iterator);
        assert!(!done);
        let entry_index = machine.runtime_slot(first_entry).unwrap().unwrap();
        let HeapEntry::Array { elements, .. } = &machine.heap[entry_index] else {
            panic!("array entries yield pair arrays")
        };
        assert_eq!(elements, &[Value::int32(0), Value::UNDEFINED]);

        let forged = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(machine.intrinsics.object_prototype),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        machine
            .set_data_property(forged, "\0iterator.source", array)
            .unwrap();
        machine
            .set_data_property(forged, "\0iterator.index", Value::int32(0))
            .unwrap();
        let next = machine.get_named_property(values_iterator, "next").unwrap();
        assert!(machine.call_value(next, forged, &[]).is_err());
    }

    #[test]
    fn collections_hide_state_and_keep_iterator_positions() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());
        let map = construct_builtin(&mut machine, "Map", &[]);
        let set = machine.get_named_property(map, "set").unwrap();
        for (key, value) in [(1, 10), (2, 20), (3, 30)] {
            machine
                .call_value(set, map, &[Value::int32(key), Value::int32(value)])
                .unwrap();
        }
        assert!(machine.own_property_keys(map).unwrap().is_empty());

        let derived = machine
            .allocate(HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(map),
                extensible: true,
                boxed_primitive: None,
            })
            .unwrap();
        let get = machine.get_named_property(map, "get").unwrap();
        assert!(
            machine
                .call_value(get, derived, &[Value::int32(1)])
                .is_err()
        );

        machine
            .set_data_property(map, "\0collection.keys", Value::UNDEFINED)
            .unwrap();
        assert_eq!(
            machine.get_named_property(map, "size").unwrap(),
            Value::int32(3)
        );

        let keys = machine.get_named_property(map, "keys").unwrap();
        let iterator = machine.call_value(keys, map, &[]).unwrap();
        assert_eq!(next_value(&mut machine, iterator), (Value::int32(1), false));
        let delete = machine.get_named_property(map, "delete").unwrap();
        assert_eq!(
            machine.call_value(delete, map, &[Value::int32(1)]).unwrap(),
            Value::TRUE
        );
        assert_eq!(next_value(&mut machine, iterator), (Value::int32(2), false));

        let clear = machine.get_named_property(map, "clear").unwrap();
        machine.call_value(clear, map, &[]).unwrap();
        machine
            .call_value(set, map, &[Value::int32(4), Value::int32(40)])
            .unwrap();
        assert_eq!(next_value(&mut machine, iterator), (Value::int32(4), false));
        assert_eq!(next_value(&mut machine, iterator), (Value::UNDEFINED, true));
        machine
            .call_value(set, map, &[Value::int32(5), Value::int32(50)])
            .unwrap();
        assert_eq!(next_value(&mut machine, iterator), (Value::UNDEFINED, true));

        machine
            .call_value(set, map, &[Value::int32(9), map])
            .unwrap();
        let structured_clone = machine.intrinsics.global("structuredClone").unwrap();
        let clone = machine
            .call_value(structured_clone, Value::UNDEFINED, &[map])
            .unwrap();
        let cloned_get = machine.get_named_property(clone, "get").unwrap();
        assert_eq!(
            machine
                .call_value(cloned_get, clone, &[Value::int32(9)])
                .unwrap(),
            clone
        );

        let churn = construct_builtin(&mut machine, "Map", &[]);
        let churn_keys = machine.get_named_property(churn, "keys").unwrap();
        let churn_iterator = machine.call_value(churn_keys, churn, &[]).unwrap();
        for key in 0..1_024 {
            machine
                .call_value(set, churn, &[Value::int32(key), Value::int32(key)])
                .unwrap();
            assert_eq!(
                machine
                    .call_value(delete, churn, &[Value::int32(key)])
                    .unwrap(),
                Value::TRUE
            );
        }
        machine
            .call_value(set, churn, &[Value::int32(2_048), Value::int32(2_048)])
            .unwrap();
        assert_eq!(
            next_value(&mut machine, churn_iterator),
            (Value::int32(2_048), false)
        );
        let churn_index = machine.runtime_slot(churn).unwrap().unwrap();
        let HeapEntry::Collection {
            entries,
            next_order,
            ..
        } = &machine.heap[churn_index]
        else {
            panic!("Map owns typed collection storage")
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, Value::int32(2_048));
        assert_eq!(*next_order, 1_025);
    }

    fn constructor_name(machine: &mut Machine<'_, TestHost>, constructor: Value) -> EcmaString {
        let name = machine
            .get_named_property(constructor, "name")
            .expect("constructor has name");
        machine.to_string(name).expect("constructor name is string")
    }

    fn instance_constructor_name(
        machine: &mut Machine<'_, TestHost>,
        instance: Value,
    ) -> EcmaString {
        let constructor = machine
            .get_named_property(instance, "constructor")
            .expect("instance resolves constructor");
        constructor_name(machine, constructor)
    }

    #[test]
    fn object_prototype_constructor_identity() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let object = machine.intrinsics.global("Object").expect("Object exists");
        let prototype = machine.intrinsics.object_prototype;
        assert_eq!(
            machine
                .get_named_property(prototype, "constructor")
                .expect("Object.prototype.constructor exists"),
            object,
            "Object.prototype.constructor must reference Object"
        );
        assert!(
            constructor_name(&mut machine, object).eq_ascii("Object"),
            "Object constructor name must be Object"
        );
    }

    #[test]
    fn error_and_range_error_prototype_constructor_identity() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        for name in ["Error", "RangeError"] {
            let constructor = machine
                .intrinsics
                .global(name)
                .unwrap_or_else(|| panic!("{name} exists"));
            let prototype = machine
                .get_named_property(constructor, "prototype")
                .unwrap_or_else(|_| panic!("{name}.prototype exists"));
            assert_eq!(
                machine
                    .get_named_property(prototype, "constructor")
                    .unwrap_or_else(|_| panic!("{name}.prototype.constructor exists")),
                constructor,
                "{name}.prototype.constructor must reference {name}"
            );
            assert!(
                constructor_name(&mut machine, constructor).eq_ascii(name),
                "{name} constructor name must match"
            );
        }
    }

    #[test]
    fn error_instances_resolve_constructor_name_through_own_prototype() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        for name in ["Error", "RangeError"] {
            let instance = construct_builtin(&mut machine, name, &[]);
            assert!(
                instance_constructor_name(&mut machine, instance).eq_ascii(name),
                "{name} instance constructor.name must resolve through its prototype"
            );
        }
    }
}
