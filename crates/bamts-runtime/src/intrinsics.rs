use std::collections::BTreeMap;

use bamts_native::{Decoded, Value};

use crate::{EvalFailure, HeapEntry, Host, Machine, PropertyMap, ThrowOrigin};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinId {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Error,
    EvalError,
    RangeError,
    ReferenceError,
    SyntaxError,
    TypeError,
    UriError,
    FunctionCall,
    ObjectPrototypeToString,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BuiltinOutcome {
    Value(Value),
    Call {
        callee: Value,
        this_value: Value,
        argument_start: usize,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Intrinsics {
    globals: BTreeMap<String, Value>,
    pub(crate) object_prototype: Value,
    pub(crate) function_prototype: Value,
    pub(crate) array_prototype: Value,
    error_prototypes: [(BuiltinId, Value); 7],
    function_call: Value,
    object_to_string: Value,
    constructor_prototypes: Vec<(BuiltinId, Value)>,
}

impl Intrinsics {
    pub(crate) fn initialize(heap: &mut Vec<HeapEntry>) -> Self {
        let object_prototype = push(
            heap,
            HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: None,
            },
        );
        let function_prototype = push(
            heap,
            HeapEntry::Object {
                properties: PropertyMap::default(),
                prototype: Some(object_prototype),
            },
        );
        let array_prototype = push(
            heap,
            HeapEntry::Array {
                elements: Vec::new(),
                properties: PropertyMap::default(),
                prototype: Some(object_prototype),
            },
        );
        let string_prototype = ordinary_prototype(heap, object_prototype);
        let number_prototype = ordinary_prototype(heap, object_prototype);
        let boolean_prototype = ordinary_prototype(heap, object_prototype);

        let error_prototypes = [
            BuiltinId::Error,
            BuiltinId::EvalError,
            BuiltinId::RangeError,
            BuiltinId::ReferenceError,
            BuiltinId::SyntaxError,
            BuiltinId::TypeError,
            BuiltinId::UriError,
        ]
        .map(|id| (id, ordinary_prototype(heap, object_prototype)));

        let constructor_specs = [
            (BuiltinId::Object, "Object", 1, object_prototype),
            (BuiltinId::Array, "Array", 1, array_prototype),
            (BuiltinId::String, "String", 1, string_prototype),
            (BuiltinId::Number, "Number", 1, number_prototype),
            (BuiltinId::Boolean, "Boolean", 1, boolean_prototype),
            (BuiltinId::Error, "Error", 1, error_prototypes[0].1),
            (BuiltinId::EvalError, "EvalError", 1, error_prototypes[1].1),
            (
                BuiltinId::RangeError,
                "RangeError",
                1,
                error_prototypes[2].1,
            ),
            (
                BuiltinId::ReferenceError,
                "ReferenceError",
                1,
                error_prototypes[3].1,
            ),
            (
                BuiltinId::SyntaxError,
                "SyntaxError",
                1,
                error_prototypes[4].1,
            ),
            (BuiltinId::TypeError, "TypeError", 1, error_prototypes[5].1),
            (BuiltinId::UriError, "URIError", 1, error_prototypes[6].1),
        ];
        let mut globals = BTreeMap::new();
        let mut constructor_prototypes = Vec::with_capacity(constructor_specs.len());
        for (id, name, length, prototype) in constructor_specs {
            let value = native_function(heap, id, name, length);
            globals.insert(name.to_owned(), value);
            constructor_prototypes.push((id, prototype));
        }
        let function_call = native_function(heap, BuiltinId::FunctionCall, "call", 1);
        let object_to_string =
            native_function(heap, BuiltinId::ObjectPrototypeToString, "toString", 0);

        Self {
            globals,
            object_prototype,
            function_prototype,
            array_prototype,
            error_prototypes,
            function_call,
            object_to_string,
            constructor_prototypes,
        }
    }

    pub(crate) fn global(&self, name: &str) -> Option<Value> {
        self.globals.get(name).copied()
    }

    pub(crate) fn constructor_prototype(&self, id: BuiltinId) -> Option<Value> {
        self.constructor_prototypes
            .iter()
            .find_map(|(candidate, prototype)| (*candidate == id).then_some(*prototype))
    }

    pub(crate) fn error_prototype(&self, id: BuiltinId) -> Value {
        self.error_prototypes
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
        },
    )
}

fn native_function(
    heap: &mut Vec<HeapEntry>,
    id: BuiltinId,
    name: &'static str,
    length: u32,
) -> Value {
    push(
        heap,
        HeapEntry::NativeFunction {
            id,
            name,
            length,
            bound_this: None,
        },
    )
}

fn push(heap: &mut Vec<HeapEntry>, entry: HeapEntry) -> Value {
    heap.push(entry);
    let slot = u32::try_from(heap.len()).expect("intrinsic heap fits in a u32 slot");
    Value::heap_ref(
        bamts_native::SlotId::from_parts(crate::RUNTIME_HEAP_SEGMENT, slot)
            .expect("intrinsic slot is nonzero"),
    )
}

impl<'a, H: Host> Machine<'a, H> {
    pub(crate) fn call_builtin(
        &mut self,
        id: BuiltinId,
        this_value: Value,
        arguments: &[Value],
        constructing: bool,
    ) -> Result<BuiltinOutcome, EvalFailure> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let value = match id {
            BuiltinId::FunctionCall => {
                return Ok(BuiltinOutcome::Call {
                    callee: this_value,
                    this_value: first,
                    argument_start: usize::from(!arguments.is_empty()),
                });
            }
            BuiltinId::ObjectPrototypeToString => {
                let tag = self.object_to_string_tag(this_value)?;
                self.allocate(HeapEntry::String(format!("[object {tag}]")))
                    .map_err(EvalFailure::Runtime)?
            }
            BuiltinId::Object => {
                if self.is_object(first) {
                    first
                } else {
                    self.allocate(HeapEntry::Object {
                        properties: PropertyMap::default(),
                        prototype: Some(self.intrinsics.object_prototype),
                    })
                    .map_err(EvalFailure::Runtime)?
                }
            }
            BuiltinId::Array => {
                let elements = if arguments.len() == 1 {
                    match first.decode() {
                        Some(Decoded::Int32(length)) => vec![Value::HOLE; length as usize],
                        Some(Decoded::Number(length)) if length >= 0.0 && length.fract() == 0.0 => {
                            vec![Value::HOLE; length as usize]
                        }
                        _ => arguments.to_vec(),
                    }
                } else {
                    arguments.to_vec()
                };
                self.allocate(HeapEntry::Array {
                    elements,
                    properties: PropertyMap::default(),
                    prototype: Some(self.intrinsics.array_prototype),
                })
                .map_err(EvalFailure::Runtime)?
            }
            BuiltinId::String => {
                let text = if arguments.is_empty() {
                    String::new()
                } else {
                    self.to_string(first)?
                };
                self.allocate(HeapEntry::String(text))
                    .map_err(EvalFailure::Runtime)?
            }
            BuiltinId::Number => {
                if arguments.is_empty() {
                    Value::int32(0)
                } else {
                    self.to_number(first)?
                }
            }
            BuiltinId::Boolean => Value::boolean(self.to_boolean(first)),
            error @ (BuiltinId::Error
            | BuiltinId::EvalError
            | BuiltinId::RangeError
            | BuiltinId::ReferenceError
            | BuiltinId::SyntaxError
            | BuiltinId::TypeError
            | BuiltinId::UriError) => {
                let prototype = self.intrinsics.error_prototype(error);
                let object = self
                    .allocate(HeapEntry::Object {
                        properties: PropertyMap::default(),
                        prototype: Some(prototype),
                    })
                    .map_err(EvalFailure::Runtime)?;
                if !arguments.is_empty() {
                    let message = self.to_string(first)?;
                    let message = self
                        .allocate(HeapEntry::String(message))
                        .map_err(EvalFailure::Runtime)?;
                    let index = self
                        .runtime_slot(object)
                        .map_err(EvalFailure::Runtime)?
                        .expect("fresh object");
                    self.set_own_data(
                        index,
                        crate::PropertyKey::Named("message".to_owned()),
                        message,
                    )?;
                }
                object
            }
        };

        if constructing
            && matches!(
                id,
                BuiltinId::String | BuiltinId::Number | BuiltinId::Boolean
            )
        {
            let prototype = self
                .intrinsics
                .constructor_prototype(id)
                .expect("primitive constructor has a prototype");
            return self
                .allocate(HeapEntry::Object {
                    properties: PropertyMap::default(),
                    prototype: Some(prototype),
                })
                .map(BuiltinOutcome::Value)
                .map_err(EvalFailure::Runtime);
        }
        Ok(BuiltinOutcome::Value(value))
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
                    HeapEntry::Function { .. }
                    | HeapEntry::NativeFunction { .. }
                    | HeapEntry::HostFunction(_) => "Function",
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

    pub(crate) fn to_string(&self, value: Value) -> Result<String, EvalFailure> {
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
