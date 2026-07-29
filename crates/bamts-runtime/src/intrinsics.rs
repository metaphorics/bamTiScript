use std::collections::BTreeMap;
use std::marker::PhantomData;

use bamts_bytecode::EcmaString;
use bamts_native::{Decoded, Value};

use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyMap, ThrowOrigin};

#[path = "builtins/mod.rs"]
mod builtins;

#[path = "regexp.rs"]
mod regexp;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BuiltinId(usize);

#[derive(Clone, Copy, Debug)]
pub(crate) enum BuiltinOutcome {
    Value(Value),
    Call {
        callee: Value,
        this_value: Value,
        argument_start: usize,
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
    symbol_to_string_tag: Option<Value>,
    symbol_prototype: Option<Value>,
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
            symbol_to_string_tag: None,
            symbol_prototype: None,
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
            crate::PropertyKey::Named(EcmaString::from_utf8("prototype")),
            crate::Property::Data {
                value: prototype,
                writable: false,
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
        let HeapEntry::NativeFunction { id, .. } = heap[index] else {
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
}

pub(crate) struct Intrinsics<H: Host> {
    pub(crate) globals: BTreeMap<EcmaString, Value>,
    pub(crate) object_prototype: Value,
    pub(crate) function_prototype: Value,
    pub(crate) array_prototype: Value,
    pub(crate) string_prototype: Value,
    pub(crate) number_prototype: Value,
    pub(crate) boolean_prototype: Value,
    pub(crate) builtins: BuiltinTable<H>,
    function_call: Value,
    object_to_string: Value,
}

impl<H: Host> Intrinsics<H> {
    pub(crate) fn initialize(heap: &mut Vec<HeapEntry>) -> Self {
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
        builtins::install(heap, &mut globals, &mut builtins);
        crate::host_objects::install(heap, &mut globals, &mut builtins);

        let function_call_key = globals
            .keys()
            .find(|key| key.eq_ascii("\0Function.prototype.call"))
            .cloned()
            .expect("core builtins install Function.prototype.call");
        let function_call = globals
            .remove(&function_call_key)
            .expect("key remains present");
        let object_to_string_key = globals
            .keys()
            .find(|key| key.eq_ascii("\0Object.prototype.toString"))
            .cloned()
            .expect("core builtins install Object.prototype.toString");
        let object_to_string = globals
            .remove(&object_to_string_key)
            .expect("key remains present");

        Self {
            globals,
            object_prototype,
            function_prototype,
            array_prototype,
            string_prototype,
            number_prototype,
            boolean_prototype,
            builtins,
            function_call,
            object_to_string,
        }
    }

    pub(crate) fn global(&self, name: &str) -> Option<Value> {
        debug_assert!(name.is_ascii());
        self.globals
            .iter()
            .find_map(|(candidate, value)| candidate.eq_ascii(name).then_some(*value))
    }

    pub(crate) fn regexp_prototype(&self) -> Value {
        self.global("\0RegExp.prototype")
            .expect("RegExp builtins install their prototype")
    }

    pub(crate) fn error_prototype(&self, id: BuiltinId) -> Value {
        self.builtins
            .error_prototypes
            .iter()
            .find_map(|(candidate, prototype)| (*candidate == id).then_some(*prototype))
            .expect("every error builtin has a realm prototype")
    }

    pub(crate) fn function_call(&self) -> Value {
        self.function_call
    }

    pub(crate) fn object_to_string(&self) -> Value {
        self.object_to_string
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
    let name_value = push(heap, HeapEntry::String(EcmaString::from_utf8(name)));
    let mut properties = PropertyMap::default();
    properties.insert(
        crate::PropertyKey::Named(EcmaString::from_utf8("length")),
        crate::Property::Data {
            value: crate::number_value(f64::from(length)),
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    properties.insert(
        crate::PropertyKey::Named(EcmaString::from_utf8("name")),
        crate::Property::Data {
            value: name_value,
            writable: false,
            enumerable: false,
            configurable: true,
        },
    );
    push(
        heap,
        HeapEntry::NativeFunction {
            id,
            properties,
            bound_this: None,
            extensible: true,
        },
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
        let handler = self.intrinsics.builtins.get(id).handler;
        let previous = self.current_builtin_id.replace(id);
        let outcome = handler(self, this_value, arguments, constructing);
        self.current_builtin_id = previous;
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

    pub(crate) fn to_boolean(&self, value: Value) -> bool {
        self.truthy(value)
    }

    pub fn same_value_zero(&self, left: Value, right: Value) -> bool {
        match (left.decode(), right.decode()) {
            (Some(Decoded::Number(a)), Some(Decoded::Number(b))) => {
                a == b || (a.is_nan() && b.is_nan())
            }
            (Some(Decoded::Number(a)), Some(Decoded::Int32(b)))
            | (Some(Decoded::Int32(b)), Some(Decoded::Number(a))) => a == f64::from(b),
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
            vec![Constant::String(EcmaString::from_utf8("<test>"))],
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
            .allocate(HeapEntry::String(EcmaString::from_utf8("test")))
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
                PropertyKey::Named(EcmaString::from_utf8(key)),
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
        let HeapEntry::NativeFunction { id, .. } = machine.heap[index] else {
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

    #[test]
    fn collections_symbols_errors_regexp_and_date_match_node_24_observables() {
        let module = module();
        let mut host = TestHost;
        let mut machine = Machine::new(&module, &mut host, Limits::default());

        let symbol = machine.intrinsics.global("Symbol").unwrap();
        let symbol_for = machine.get_named_property(symbol, "for").unwrap();
        let key_text = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("shared")))
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
            .allocate(HeapEntry::String(EcmaString::from_utf8("^(a|b)\\.js$")))
            .unwrap();
        let regexp = construct_builtin(&mut machine, "RegExp", &[pattern]);
        let test = machine.get_named_property(regexp, "test").unwrap();
        let input = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("b.js")))
            .unwrap();
        assert_eq!(
            machine.call_value(test, regexp, &[input]).unwrap(),
            Value::TRUE
        );

        let message = machine
            .allocate(HeapEntry::String(EcmaString::from_utf8("boom")))
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
        let to_iso = machine.get_named_property(date, "toISOString").unwrap();
        let iso = machine.call_value(to_iso, date, &[]).unwrap();
        assert!(
            machine
                .to_string(iso)
                .unwrap()
                .eq_ascii("1970-01-01T00:00:00.000Z")
        );
    }
}
