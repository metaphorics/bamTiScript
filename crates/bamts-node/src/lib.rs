//! Deterministic Node-compatible host bindings for BamTS.
//!
//! This crate never launches Node or another JavaScript engine. Host objects
//! use stable numeric identities; the runtime materializes those identities as
//! local host-function and host-object heap entries.

use std::collections::{BTreeMap, BTreeSet};

use bamts_native::{Decoded, SlotId, Value};
use bamts_runtime::{Host, HostBinding, HostThrow};

const HOST_SEGMENT: u16 = 2;
const FIRST_DYNAMIC_ENTITY: u32 = 1_024;

/// Stable entity identities. Values below [`FIRST_DYNAMIC_ENTITY`] are part of
/// the bamts-node/runtime contract and must not depend on allocation order.
pub mod entity {
    pub const PROCESS: u32 = 1;
    pub const PROCESS_STDOUT: u32 = 2;
    pub const PROCESS_ENV: u32 = 3;
    pub const PROCESS_EXIT: u32 = 4;
    pub const PROCESS_GET_BUILTIN_MODULE: u32 = 5;
    pub const STDOUT_WRITE: u32 = 6;
    pub const PROCESS_VERSIONS: u32 = 7;

    pub const CONSOLE: u32 = 16;
    pub const CONSOLE_LOG: u32 = 17;
    pub const CONSOLE_WARN: u32 = 18;
    pub const CONSOLE_ERROR: u32 = 19;

    pub const JSON: u32 = 32;
    pub const JSON_STRINGIFY: u32 = 33;

    pub const SET_TIMEOUT: u32 = 48;

    pub const NODE_UTIL: u32 = 64;
    pub const UTIL_PARSE_ARGS: u32 = 65;
    pub const NODE_CRYPTO: u32 = 80;
    pub const CRYPTO_CREATE_HASH: u32 = 81;
    pub const NODE_VM: u32 = 96;
    pub const VM_RUN_IN_NEW_CONTEXT: u32 = 97;

    pub const GLOBAL_THIS: u32 = 112;
}

/// A deterministic module namespace owned by the host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleRecord {
    exports: BTreeMap<String, Value>,
}

impl ModuleRecord {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Value> {
        self.exports.get(name).copied()
    }

    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        self.exports.insert(name.into(), value);
    }

    pub fn delete(&mut self, name: &str) -> bool {
        self.exports.remove(name).is_some()
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.exports.contains_key(name)
    }
}

/// Concrete, deterministic Node-compatible host state.
///
/// Environment input is explicit rather than inherited from the embedding
/// process. This keeps corpus results independent of the machine running them.
pub struct NodeHost {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    env: BTreeMap<String, String>,
    entity_values: BTreeMap<(u32, String), Value>,
    deleted_properties: BTreeSet<(u32, String)>,
    modules: BTreeMap<String, u32>,
    module_records: BTreeMap<u32, ModuleRecord>,
    exports: ModuleRecord,
    next_entity: u32,
}

impl Default for NodeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeHost {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: 0,
            env: BTreeMap::new(),
            entity_values: BTreeMap::new(),
            deleted_properties: BTreeSet::new(),
            modules: BTreeMap::from([
                ("node:crypto".to_owned(), entity::NODE_CRYPTO),
                ("node:util".to_owned(), entity::NODE_UTIL),
                ("node:vm".to_owned(), entity::NODE_VM),
            ]),
            module_records: BTreeMap::new(),
            exports: ModuleRecord::default(),
            next_entity: FIRST_DYNAMIC_ENTITY,
        }
    }

    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn write_stdout(&mut self, bytes: &[u8]) {
        self.stdout.extend_from_slice(bytes);
    }

    pub fn write_stderr(&mut self, bytes: &[u8]) {
        self.stderr.extend_from_slice(bytes);
    }

    pub fn set_exit_code(&mut self, exit_code: i32) {
        self.exit_code = exit_code;
    }

    #[must_use]
    pub fn env(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    pub fn set_env(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.env.insert(name.into(), value.into());
    }

    pub fn remove_env(&mut self, name: &str) -> bool {
        self.env.remove(name).is_some()
    }

    #[must_use]
    pub fn exports(&self) -> &ModuleRecord {
        &self.exports
    }

    /// Defines or replaces an export in a deterministic host module record.
    /// Newly registered module namespace ids are monotonic from 1024.
    pub fn define_module_export(
        &mut self,
        specifier: impl Into<String>,
        name: impl Into<String>,
        value: Value,
    ) -> u32 {
        let specifier = specifier.into();
        let entity = match self.modules.get(&specifier) {
            Some(entity) => *entity,
            None => {
                let entity = self.allocate_entity();
                self.modules.insert(specifier, entity);
                entity
            }
        };
        self.module_records
            .entry(entity)
            .or_default()
            .set(name, value);
        entity
    }

    fn allocate_entity(&mut self) -> u32 {
        let entity = self.next_entity;
        self.next_entity = self
            .next_entity
            .checked_add(1)
            .expect("host entity id space exhausted");
        self.module_records.entry(entity).or_default();
        entity
    }

    fn is_object(&self, entity: u32) -> bool {
        matches!(
            entity,
            entity::PROCESS
                | entity::PROCESS_STDOUT
                | entity::PROCESS_ENV
                | entity::PROCESS_VERSIONS
                | entity::CONSOLE
                | entity::JSON
                | entity::NODE_UTIL
                | entity::NODE_CRYPTO
                | entity::NODE_VM
                | entity::GLOBAL_THIS
        ) || self.module_records.contains_key(&entity)
    }

    fn static_property(entity: u32, key: &str) -> Option<HostBinding> {
        match (entity, key) {
            (entity::PROCESS, "stdout") => Some(HostBinding::Object(entity::PROCESS_STDOUT)),
            (entity::PROCESS, "env") => Some(HostBinding::Object(entity::PROCESS_ENV)),
            (entity::PROCESS, "versions") => Some(HostBinding::Object(entity::PROCESS_VERSIONS)),
            (entity::PROCESS, "exit") => Some(HostBinding::Function(entity::PROCESS_EXIT)),
            // Deliberately absent: the ohash driver uses optional chaining and
            // falls back to the statically imported node:crypto module.
            (entity::PROCESS, "getBuiltinModule") => Some(HostBinding::Primitive(Value::UNDEFINED)),
            (entity::PROCESS_STDOUT, "write") => Some(HostBinding::Function(entity::STDOUT_WRITE)),
            (entity::CONSOLE, "log") => Some(HostBinding::Function(entity::CONSOLE_LOG)),
            (entity::CONSOLE, "warn") => Some(HostBinding::Function(entity::CONSOLE_WARN)),
            (entity::CONSOLE, "error") => Some(HostBinding::Function(entity::CONSOLE_ERROR)),
            (entity::JSON, "stringify") => Some(HostBinding::Function(entity::JSON_STRINGIFY)),
            (entity::NODE_UTIL, "parseArgs") => {
                Some(HostBinding::Function(entity::UTIL_PARSE_ARGS))
            }
            (entity::NODE_CRYPTO, "createHash") => {
                Some(HostBinding::Function(entity::CRYPTO_CREATE_HASH))
            }
            (entity::NODE_VM, "runInNewContext") => {
                Some(HostBinding::Function(entity::VM_RUN_IN_NEW_CONTEXT))
            }
            (entity::GLOBAL_THIS, "process") => Some(HostBinding::Object(entity::PROCESS)),
            _ => None,
        }
    }

    fn primitive_text(value: Value) -> Option<String> {
        match value.decode()? {
            Decoded::Number(number) => Some(if number.is_nan() {
                "NaN".to_owned()
            } else if number == f64::INFINITY {
                "Infinity".to_owned()
            } else if number == f64::NEG_INFINITY {
                "-Infinity".to_owned()
            } else {
                number.to_string()
            }),
            Decoded::Int32(bits) => Some((bits as i32).to_string()),
            Decoded::Undefined => Some("undefined".to_owned()),
            Decoded::Null => Some("null".to_owned()),
            Decoded::Boolean(value) => Some(value.to_string()),
            Decoded::HeapRef(id) if id.segment() == HOST_SEGMENT => {
                Some("[object Object]".to_owned())
            }
            Decoded::HeapRef(_) | Decoded::Hole | Decoded::Uninitialized => None,
        }
    }

    fn append_arguments(
        output: &mut Vec<u8>,
        arguments: &[Value],
        separator: &[u8],
    ) -> Result<(), HostThrow> {
        for (index, value) in arguments.iter().copied().enumerate() {
            if index != 0 {
                output.extend_from_slice(separator);
            }
            let text = Self::primitive_text(value).ok_or_else(undefined_throw)?;
            output.extend_from_slice(text.as_bytes());
        }
        Ok(())
    }

    fn entity_from_value(&self, value: Value) -> Result<u32, HostThrow> {
        let Some(id) = value.as_heap_ref() else {
            return Err(undefined_throw());
        };
        if id.segment() != HOST_SEGMENT {
            return Err(undefined_throw());
        }
        let entity = id.slot();
        if self.is_object(entity) || is_function(entity) {
            Ok(entity)
        } else {
            Err(undefined_throw())
        }
    }
}

fn undefined_throw() -> HostThrow {
    HostThrow {
        value: Value::UNDEFINED,
    }
}

fn is_function(entity: u32) -> bool {
    matches!(
        entity,
        entity::PROCESS_EXIT
            | entity::PROCESS_GET_BUILTIN_MODULE
            | entity::STDOUT_WRITE
            | entity::CONSOLE_LOG
            | entity::CONSOLE_WARN
            | entity::CONSOLE_ERROR
            | entity::JSON_STRINGIFY
            | entity::SET_TIMEOUT
            | entity::UTIL_PARSE_ARGS
            | entity::CRYPTO_CREATE_HASH
            | entity::VM_RUN_IN_NEW_CONTEXT
    )
}

fn foreign_value(entity: u32) -> Value {
    let id = SlotId::from_parts(HOST_SEGMENT, entity).expect("entity ids are nonzero");
    Value::heap_ref(id)
}

fn binding_value(binding: HostBinding) -> Value {
    match binding {
        HostBinding::Function(entity) | HostBinding::Object(entity) => foreign_value(entity),
        HostBinding::Primitive(value) => value,
    }
}

impl Host for NodeHost {
    fn resolve_global(&mut self, name: &str) -> Option<HostBinding> {
        match name {
            "process" => Some(HostBinding::Object(entity::PROCESS)),
            "console" => Some(HostBinding::Object(entity::CONSOLE)),
            "JSON" => Some(HostBinding::Object(entity::JSON)),
            "setTimeout" => Some(HostBinding::Function(entity::SET_TIMEOUT)),
            "globalThis" => Some(HostBinding::Object(entity::GLOBAL_THIS)),
            _ => None,
        }
    }

    fn entity_get(&mut self, entity: u32, key: &str) -> Result<HostBinding, HostThrow> {
        if !self.is_object(entity) && !is_function(entity) {
            return Err(undefined_throw());
        }
        let owned_key = (entity, key.to_owned());
        if self.deleted_properties.contains(&owned_key) {
            return Ok(HostBinding::Primitive(Value::UNDEFINED));
        }
        if let Some(value) = self.entity_values.get(&owned_key).copied() {
            return Ok(HostBinding::Primitive(value));
        }
        if entity == entity::PROCESS_ENV {
            // Environment strings cannot be represented as Primitive(Value)
            // by the fixed host contract. A missing binding is still exact.
            return if self.env.contains_key(key) {
                Err(undefined_throw())
            } else {
                Ok(HostBinding::Primitive(Value::UNDEFINED))
            };
        }
        if let Some(value) = self
            .module_records
            .get(&entity)
            .and_then(|record| record.get(key))
        {
            return Ok(HostBinding::Primitive(value));
        }
        Ok(Self::static_property(entity, key).unwrap_or(HostBinding::Primitive(Value::UNDEFINED)))
    }

    fn entity_set(&mut self, entity: u32, key: &str, value: Value) -> Result<(), HostThrow> {
        if !self.is_object(entity) {
            return Err(undefined_throw());
        }
        let owned_key = (entity, key.to_owned());
        self.deleted_properties.remove(&owned_key);
        if let Some(record) = self.module_records.get_mut(&entity) {
            record.set(key, value);
        } else {
            self.entity_values.insert(owned_key, value);
        }
        Ok(())
    }

    fn entity_delete(&mut self, entity: u32, key: &str) -> Result<bool, HostThrow> {
        if !self.is_object(entity) {
            return Err(undefined_throw());
        }
        if entity == entity::PROCESS_ENV {
            let removed = self.env.remove(key).is_some()
                | self
                    .entity_values
                    .remove(&(entity, key.to_owned()))
                    .is_some();
            return Ok(removed);
        }
        if let Some(record) = self.module_records.get_mut(&entity) {
            return Ok(record.delete(key));
        }
        let owned_key = (entity, key.to_owned());
        let removed = self.entity_values.remove(&owned_key).is_some()
            || Self::static_property(entity, key).is_some();
        if removed {
            self.deleted_properties.insert(owned_key);
        }
        Ok(removed)
    }

    fn entity_has(&mut self, entity: u32, key: &str) -> Result<bool, HostThrow> {
        if !self.is_object(entity) {
            return Err(undefined_throw());
        }
        let owned_key = (entity, key.to_owned());
        if self.deleted_properties.contains(&owned_key) {
            return Ok(false);
        }
        Ok(self.entity_values.contains_key(&owned_key)
            || (entity == entity::PROCESS_ENV && self.env.contains_key(key))
            || self
                .module_records
                .get(&entity)
                .is_some_and(|record| record.contains(key))
            || Self::static_property(entity, key).is_some())
    }

    fn entity_call(
        &mut self,
        entity: u32,
        _this: Value,
        arguments: &[Value],
    ) -> Result<HostBinding, HostThrow> {
        match entity {
            entity::STDOUT_WRITE => {
                Self::append_arguments(&mut self.stdout, arguments, b"")?;
                Ok(HostBinding::Primitive(Value::TRUE))
            }
            entity::CONSOLE_LOG => {
                Self::append_arguments(&mut self.stdout, arguments, b" ")?;
                self.stdout.push(b'\n');
                Ok(HostBinding::Primitive(Value::UNDEFINED))
            }
            entity::CONSOLE_WARN | entity::CONSOLE_ERROR => {
                Self::append_arguments(&mut self.stderr, arguments, b" ")?;
                self.stderr.push(b'\n');
                Ok(HostBinding::Primitive(Value::UNDEFINED))
            }
            entity::PROCESS_EXIT => {
                self.exit_code = arguments
                    .first()
                    .and_then(|value| value.as_int32())
                    .map_or(0, |bits| bits as i32);
                Ok(HostBinding::Primitive(Value::UNDEFINED))
            }
            // JSON.stringify must return a freshly allocated runtime string,
            // which the fixed HostBinding contract cannot mint. Left as an
            // explicit throw rather than silently wrong output; requires a
            // runtime value-allocation bridge (escalated cross-crate).
            entity::JSON_STRINGIFY => Err(undefined_throw()),
            entity::VM_RUN_IN_NEW_CONTEXT => {
                let object = self.allocate_entity();
                Ok(HostBinding::Object(object))
            }
            // These APIs require reading runtime strings/arrays and allocating
            // runtime strings/objects, which HostBinding intentionally cannot do.
            entity::PROCESS_GET_BUILTIN_MODULE
            | entity::SET_TIMEOUT
            | entity::UTIL_PARSE_ARGS
            | entity::CRYPTO_CREATE_HASH => Err(undefined_throw()),
            _ => Err(undefined_throw()),
        }
    }

    fn entity_construct(
        &mut self,
        _entity: u32,
        _arguments: &[Value],
    ) -> Result<HostBinding, HostThrow> {
        Err(undefined_throw())
    }

    fn entity_instance_of(&mut self, _entity: u32, _value: Value) -> Result<bool, HostThrow> {
        Ok(false)
    }

    fn property_get(&mut self, object: Value, key: &str) -> Result<Value, HostThrow> {
        let entity = self.entity_from_value(object)?;
        self.entity_get(entity, key).map(binding_value)
    }

    fn property_set(&mut self, object: Value, key: &str, value: Value) -> Result<(), HostThrow> {
        let entity = self.entity_from_value(object)?;
        self.entity_set(entity, key, value)
    }

    fn property_delete(&mut self, object: Value, key: &str) -> Result<bool, HostThrow> {
        let entity = self.entity_from_value(object)?;
        self.entity_delete(entity, key)
    }

    fn property_has(&mut self, object: Value, key: &str) -> Result<bool, HostThrow> {
        let entity = self.entity_from_value(object)?;
        self.entity_has(entity, key)
    }

    fn call(
        &mut self,
        callee: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, HostThrow> {
        let entity = self.entity_from_value(callee)?;
        self.entity_call(entity, this, arguments).map(binding_value)
    }

    fn construct(&mut self, callee: Value, arguments: &[Value]) -> Result<Value, HostThrow> {
        let entity = self.entity_from_value(callee)?;
        self.entity_construct(entity, arguments).map(binding_value)
    }

    fn instance_of(&mut self, value: Value, constructor: Value) -> Result<bool, HostThrow> {
        let entity = self.entity_from_value(constructor)?;
        self.entity_instance_of(entity, value)
    }

    fn awaited(&mut self, value: Value) -> Result<Value, HostThrow> {
        Ok(value)
    }

    fn import(&mut self, specifier: &str) -> Result<Value, HostThrow> {
        self.modules
            .get(specifier)
            .copied()
            .map(foreign_value)
            .ok_or_else(undefined_throw)
    }

    fn export(&mut self, name: &str, value: Value) -> Result<(), HostThrow> {
        self.exports.set(name, value);
        Ok(())
    }
}

#[cfg(feature = "aot-main")]
fn run_aot_main() -> i32 {
    use std::io::Write;

    use bamts_bytecode::{DecodeLimits, decode_verified};
    use bamts_native::linked_program;
    use bamts_runtime::{Limits, run_linked_program};

    let linked = match linked_program() {
        Ok(linked) => linked,
        Err(_) => return 1,
    };
    let module = match decode_verified(linked.bytecode(), &DecodeLimits::default()) {
        Ok(module) => module,
        Err(_) => return 1,
    };
    let mut host = NodeHost::new();
    let outcome = match run_linked_program(&module, &linked, &mut host, &Limits::default()) {
        Ok(outcome) => outcome,
        Err(_) => return 1,
    };

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if stdout.write_all(host.stdout()).is_err() || stdout.write_all(&outcome.stdout).is_err() {
        return 1;
    }
    if host.exit_code() == 0 {
        outcome.exit_code
    } else {
        host.exit_code()
    }
}

/// C process entry for a linked BamTS AOT image.
#[cfg(feature = "aot-main")]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    run_aot_main()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(binding: HostBinding) -> u32 {
        match binding {
            HostBinding::Object(entity) => entity,
            _ => panic!("expected object binding"),
        }
    }

    fn function(binding: HostBinding) -> u32 {
        match binding {
            HostBinding::Function(entity) => entity,
            _ => panic!("expected function binding"),
        }
    }

    #[test]
    fn global_and_nested_entity_ids_are_stable() {
        let mut first = NodeHost::new();
        let mut second = NodeHost::new();
        assert_eq!(
            object(first.resolve_global("process").unwrap()),
            object(second.resolve_global("process").unwrap())
        );
        assert_eq!(
            object(first.entity_get(entity::PROCESS, "stdout").unwrap()),
            entity::PROCESS_STDOUT
        );
        assert_eq!(
            function(first.entity_get(entity::PROCESS_STDOUT, "write").unwrap()),
            entity::STDOUT_WRITE
        );
        assert_eq!(
            function(second.entity_get(entity::CONSOLE, "log").unwrap()),
            entity::CONSOLE_LOG
        );
    }

    #[test]
    fn console_and_process_capture_deterministic_bytes_and_exit() {
        let mut host = NodeHost::new();
        host.entity_call(
            entity::CONSOLE_LOG,
            Value::UNDEFINED,
            &[Value::int32(42), Value::TRUE],
        )
        .unwrap();
        host.entity_call(entity::STDOUT_WRITE, Value::UNDEFINED, &[Value::FALSE])
            .unwrap();
        host.entity_call(
            entity::PROCESS_EXIT,
            Value::UNDEFINED,
            &[Value::int32((-7_i32) as u32)],
        )
        .unwrap();
        assert_eq!(host.stdout(), b"42 true\nfalse");
        assert_eq!(host.exit_code(), -7);
    }

    #[test]
    fn environment_is_explicit_ordered_and_deletable() {
        let mut host = NodeHost::new();
        assert_eq!(host.env("NODE_ENV"), None);
        host.set_env("NODE_ENV", "production");
        host.set_env("A", "first");
        assert_eq!(host.env("NODE_ENV"), Some("production"));
        assert!(host.entity_has(entity::PROCESS_ENV, "NODE_ENV").unwrap());
        assert!(host.entity_delete(entity::PROCESS_ENV, "NODE_ENV").unwrap());
        assert_eq!(host.env("NODE_ENV"), None);
    }

    #[test]
    fn writable_console_properties_round_trip_runtime_values() {
        let mut host = NodeHost::new();
        host.entity_set(entity::CONSOLE, "warn", Value::int32(91))
            .unwrap();
        assert!(matches!(
            host.entity_get(entity::CONSOLE, "warn").unwrap(),
            HostBinding::Primitive(value) if value == Value::int32(91)
        ));
        assert!(host.entity_delete(entity::CONSOLE, "warn").unwrap());
        assert!(!host.entity_has(entity::CONSOLE, "warn").unwrap());
    }

    #[test]
    fn builtin_and_user_module_records_are_deterministic() {
        let mut first = NodeHost::new();
        let mut second = NodeHost::new();
        assert_eq!(
            first.import("node:vm").unwrap(),
            foreign_value(entity::NODE_VM)
        );
        let first_id = first.define_module_export("local:a", "default", Value::TRUE);
        let second_id = second.define_module_export("local:a", "default", Value::TRUE);
        assert_eq!(first_id, FIRST_DYNAMIC_ENTITY);
        assert_eq!(first_id, second_id);
        assert_eq!(first.import("local:a").unwrap(), foreign_value(first_id));
        assert!(matches!(
            first.entity_get(first_id, "default").unwrap(),
            HostBinding::Primitive(Value::TRUE)
        ));
        first.export("answer", Value::int32(42)).unwrap();
        assert_eq!(first.exports().get("answer"), Some(Value::int32(42)));
    }

    #[test]
    fn vm_returns_fresh_deterministic_host_objects() {
        let mut first = NodeHost::new();
        let mut second = NodeHost::new();
        let first_object = object(
            first
                .entity_call(entity::VM_RUN_IN_NEW_CONTEXT, Value::UNDEFINED, &[])
                .unwrap(),
        );
        let second_object = object(
            second
                .entity_call(entity::VM_RUN_IN_NEW_CONTEXT, Value::UNDEFINED, &[])
                .unwrap(),
        );
        assert_eq!(first_object, FIRST_DYNAMIC_ENTITY);
        assert_eq!(first_object, second_object);
    }

    #[test]
    fn corpus_external_surface_is_exact() {
        let mut host = NodeHost::new();
        assert_eq!(
            host.resolve_global("globalThis"),
            Some(HostBinding::Object(entity::GLOBAL_THIS))
        );
        assert_eq!(
            host.resolve_global("setTimeout"),
            Some(HostBinding::Function(entity::SET_TIMEOUT))
        );
        assert_eq!(
            object(host.entity_get(entity::GLOBAL_THIS, "process").unwrap()),
            entity::PROCESS
        );
        assert_eq!(
            function(host.entity_get(entity::NODE_UTIL, "parseArgs").unwrap()),
            entity::UTIL_PARSE_ARGS
        );
        assert_eq!(
            function(host.entity_get(entity::NODE_CRYPTO, "createHash").unwrap()),
            entity::CRYPTO_CREATE_HASH
        );
        assert_eq!(
            function(host.entity_get(entity::NODE_VM, "runInNewContext").unwrap()),
            entity::VM_RUN_IN_NEW_CONTEXT
        );
        assert!(host.import("node:util").is_ok());
        assert!(host.import("node:crypto").is_ok());
        assert!(host.import("node:vm").is_ok());
        assert!(host.import("node:fs").is_err());
    }

    #[test]
    fn forged_entity_ids_are_rejected() {
        let mut host = NodeHost::new();
        assert!(host.entity_get(u32::MAX, "x").is_err());
        assert!(host.entity_set(u32::MAX, "x", Value::TRUE).is_err());
        assert!(host.entity_call(u32::MAX, Value::UNDEFINED, &[]).is_err());
    }
}
