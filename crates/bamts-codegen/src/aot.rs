//! Ahead-of-time object emission for verified BamTS bytecode.

use std::error::Error;
use std::fmt;

use bamts_bytecode::{Module as BytecodeModule, Verified};
use cranelift_codegen::Context;
use cranelift_codegen::ir::{ExternalName, Function, UserExternalName};
use cranelift_codegen::isa;
use cranelift_codegen::settings::{self, Flags};
use cranelift_module::{DataDescription, FuncId, Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::{HELPER_NAMESPACE, Helper, LowerError, LoweredModule, function_symbol, lower_module};

const HELPER_COUNT: u32 = 30;
const AOT_MAGIC: u64 = u64::from_le_bytes(*b"BMTSAOT1");
const AOT_ABI_VERSION: u32 = 1;
const UNIT_DESCRIPTOR_BYTES: usize = 16;
const PROGRAM_DESCRIPTOR_BYTES: usize = 56;

const BYTECODE_SYMBOL: &str = "bamts_bytecode_blob";
const UNITS_SYMBOL: &str = "bamts_unit_descriptors";
/// The exported descriptor symbol consumed by `bamts-native`.
pub const PROGRAM_DESCRIPTOR_SYMBOL: &str = "bamts_program_descriptor";

/// An emitted AOT object and the information a linker driver needs to consume it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AotObject {
    /// Relocatable object-file bytes. This crate never invokes a linker.
    pub bytes: Vec<u8>,
    /// The normalized target triple recorded by Cranelift.
    pub target: String,
    /// Exported descriptor symbol that roots the embedded program image.
    pub descriptor_symbol: &'static str,
    /// Bytecode id of the program entry.
    pub entry_function: u32,
    /// Native symbol implementing `entry_function`.
    pub entry_symbol: String,
    /// Runtime helper symbols referenced by the emitted functions, in ABI order.
    pub required_helpers: Vec<&'static str>,
}

/// A typed AOT-emission failure.
#[derive(Debug)]
pub enum AotError {
    /// Cranelift does not support the requested target name.
    TargetLookup(String),
    /// The target ISA could not be configured.
    TargetBuild(String),
    /// The target does not expose a usable byte order.
    TargetEndianness(String),
    /// Shared bytecode-to-CLIF lowering failed.
    Lower(LowerError),
    /// The lowered module violated a backend invariant.
    InvalidLoweredModule(String),
    /// A Cranelift module declaration or definition failed.
    Module(String),
    /// Serializing the completed object failed.
    Emit(String),
}

impl fmt::Display for AotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetLookup(message) => write!(f, "unsupported AOT target: {message}"),
            Self::TargetBuild(message) => write!(f, "could not configure AOT target: {message}"),
            Self::TargetEndianness(target) => {
                write!(f, "AOT target has no known byte order: {target}")
            }
            Self::Lower(error) => write!(f, "AOT lowering failed: {error}"),
            Self::InvalidLoweredModule(message) => {
                write!(f, "invalid lowered module for AOT emission: {message}")
            }
            Self::Module(message) => write!(f, "AOT object definition failed: {message}"),
            Self::Emit(message) => write!(f, "AOT object serialization failed: {message}"),
        }
    }
}

impl Error for AotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lower(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LowerError> for AotError {
    fn from(error: LowerError) -> Self {
        Self::Lower(error)
    }
}

/// Compiles a verified module into one relocatable object for `target`.
///
/// The object contains every lowered function, the canonical bytecode encoding,
/// a `UnitDescriptor` record for every function, and the exported
/// `bamts_program_descriptor`. Runtime helpers remain ordinary undefined
/// symbols for the final linker to resolve; no linker is invoked here.
///
/// # Errors
///
/// Returns [`AotError`] when the target is unsupported, lowering fails, the
/// object module rejects a declaration or definition, or object serialization
/// fails.
pub fn compile_aot(
    bytecode: &BytecodeModule<Verified>,
    target: &str,
) -> Result<AotObject, AotError> {
    let flags = Flags::new(settings::builder());
    let isa_builder =
        isa::lookup_by_name(target).map_err(|error| AotError::TargetLookup(error.to_string()))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|error| AotError::TargetBuild(error.to_string()))?;
    let pointer_bits = isa.frontend_config().pointer_bits();
    if pointer_bits != 64 {
        return Err(AotError::Lower(LowerError::UnsupportedPointerWidth {
            bits: pointer_bits,
        }));
    }
    let target_endianness = isa
        .triple()
        .endianness()
        .map_err(|()| AotError::TargetEndianness(isa.triple().to_string()))?;
    let little_endianness = isa::lookup_by_name("x86_64")
        .expect("the all-native-arch build includes x86-64")
        .triple()
        .endianness()
        .expect("x86-64 has a defined byte order");
    let little_endian = target_endianness == little_endianness;
    let lowered = lower_module(bytecode, isa.frontend_config())?;
    let normalized_target = isa.triple().to_string();
    let builder = ObjectBuilder::new(isa, "bamts", default_libcall_names())
        .map_err(|error| AotError::Module(error.to_string()))?;
    let mut object = ObjectModule::new(builder);

    let function_ids = declare_functions(&mut object, &lowered)?;
    let helper_ids = declare_helpers(&mut object, lowered.call_conv)?;
    define_functions(&mut object, &lowered, &function_ids, &helper_ids)?;
    define_program_data(
        &mut object,
        &lowered,
        &function_ids,
        bytecode.encode(),
        little_endian,
    )?;

    let required_helpers = (0..HELPER_COUNT)
        .filter_map(Helper::from_external_index)
        .filter(|helper| {
            lowered
                .functions
                .iter()
                .any(|function| function.helpers.contains(helper))
        })
        .map(Helper::symbol)
        .collect();
    let entry_function = lowered.entry.get();
    let bytes = object
        .finish()
        .emit()
        .map_err(|error| AotError::Emit(error.to_string()))?;

    Ok(AotObject {
        bytes,
        target: normalized_target,
        descriptor_symbol: PROGRAM_DESCRIPTOR_SYMBOL,
        entry_function,
        entry_symbol: function_symbol(entry_function),
        required_helpers,
    })
}

fn declare_functions(
    object: &mut ObjectModule,
    lowered: &LoweredModule,
) -> Result<Vec<FuncId>, AotError> {
    lowered
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            if function.id.get() as usize != index {
                return Err(AotError::InvalidLoweredModule(format!(
                    "function {} appears at index {index}",
                    function.id.get()
                )));
            }
            let id = object
                .declare_function(&function.symbol, Linkage::Export, &function.signature)
                .map_err(|error| AotError::Module(error.to_string()))?;
            if id.as_u32() as usize != index {
                return Err(AotError::InvalidLoweredModule(
                    "object function ids are not index-ordered".to_string(),
                ));
            }
            Ok(id)
        })
        .collect()
}

fn declare_helpers(
    object: &mut ObjectModule,
    call_conv: cranelift_codegen::isa::CallConv,
) -> Result<Vec<FuncId>, AotError> {
    (0..HELPER_COUNT)
        .map(|index| {
            let helper = Helper::from_external_index(index).ok_or_else(|| {
                AotError::InvalidLoweredModule(format!("missing helper ABI index {index}"))
            })?;
            object
                .declare_function(
                    helper.symbol(),
                    Linkage::Import,
                    &helper.signature(call_conv),
                )
                .map_err(|error| AotError::Module(error.to_string()))
        })
        .collect()
}

fn define_functions(
    object: &mut ObjectModule,
    lowered: &LoweredModule,
    function_ids: &[FuncId],
    helper_ids: &[FuncId],
) -> Result<(), AotError> {
    for (lowered_function, &function_id) in lowered.functions.iter().zip(function_ids) {
        let mut function = lowered_function.clif.clone();
        remap_helper_names(&mut function, helper_ids)?;
        let mut context = Context::for_function(function);
        object
            .define_function(function_id, &mut context)
            .map_err(|error| AotError::Module(error.to_string()))?;
    }
    Ok(())
}

fn remap_helper_names(function: &mut Function, helper_ids: &[FuncId]) -> Result<(), AotError> {
    let external_functions: Vec<_> = function.dfg.ext_funcs.keys().collect();
    for function_ref in external_functions {
        let ExternalName::User(name_ref) = function.dfg.ext_funcs[function_ref].name else {
            continue;
        };
        let name = &function.params.user_named_funcs()[name_ref];
        if name.namespace != HELPER_NAMESPACE {
            continue;
        }
        let helper_id = helper_ids.get(name.index as usize).ok_or_else(|| {
            AotError::InvalidLoweredModule(format!(
                "function references unknown helper index {}",
                name.index
            ))
        })?;
        let replacement = function.declare_imported_user_function(UserExternalName {
            namespace: 0,
            index: helper_id.as_u32(),
        });
        function.dfg.ext_funcs[function_ref].name = ExternalName::user(replacement);
    }
    Ok(())
}

fn define_program_data(
    object: &mut ObjectModule,
    lowered: &LoweredModule,
    function_ids: &[FuncId],
    bytecode: Vec<u8>,
    little_endian: bool,
) -> Result<(), AotError> {
    let unit_bytes = lowered
        .functions
        .len()
        .checked_mul(UNIT_DESCRIPTOR_BYTES)
        .ok_or_else(|| AotError::InvalidLoweredModule("unit table size overflow".to_string()))?;
    if unit_bytes > u32::MAX as usize {
        return Err(AotError::InvalidLoweredModule(
            "unit table exceeds the relocation offset range".to_string(),
        ));
    }

    let bytecode_id = object
        .declare_data(BYTECODE_SYMBOL, Linkage::Local, false, false)
        .map_err(|error| AotError::Module(error.to_string()))?;
    let mut bytecode_data = DataDescription::new();
    bytecode_data.define(bytecode.into_boxed_slice());
    bytecode_data.set_align(1);
    object
        .define_data(bytecode_id, &bytecode_data)
        .map_err(|error| AotError::Module(error.to_string()))?;

    let units_id = object
        .declare_data(UNITS_SYMBOL, Linkage::Local, false, false)
        .map_err(|error| AotError::Module(error.to_string()))?;
    let mut unit_contents = vec![0; unit_bytes];
    for (index, function) in lowered.functions.iter().enumerate() {
        write_u32(
            &mut unit_contents,
            index * UNIT_DESCRIPTOR_BYTES,
            function.id.get(),
            little_endian,
        );
    }
    let mut units_data = DataDescription::new();
    units_data.define(unit_contents.into_boxed_slice());
    units_data.set_align(8);
    for (index, &function_id) in function_ids.iter().enumerate() {
        let function_ref = object.declare_func_in_data(function_id, &mut units_data);
        units_data.write_function_addr((index * UNIT_DESCRIPTOR_BYTES + 8) as u32, function_ref);
    }
    object
        .define_data(units_id, &units_data)
        .map_err(|error| AotError::Module(error.to_string()))?;

    let descriptor_id = object
        .declare_data(PROGRAM_DESCRIPTOR_SYMBOL, Linkage::Export, false, false)
        .map_err(|error| AotError::Module(error.to_string()))?;
    let mut descriptor = vec![0; PROGRAM_DESCRIPTOR_BYTES];
    write_u64(&mut descriptor, 0, AOT_MAGIC, little_endian);
    write_u32(&mut descriptor, 8, AOT_ABI_VERSION, little_endian);
    write_u64(
        &mut descriptor,
        24,
        bytecode_data.init.size() as u64,
        little_endian,
    );
    write_u64(
        &mut descriptor,
        40,
        function_ids.len() as u64,
        little_endian,
    );
    write_u32(&mut descriptor, 48, lowered.entry.get(), little_endian);

    let mut descriptor_data = DataDescription::new();
    descriptor_data.define(descriptor.into_boxed_slice());
    descriptor_data.set_align(8);
    descriptor_data.set_used(true);
    let bytecode_ref = object.declare_data_in_data(bytecode_id, &mut descriptor_data);
    descriptor_data.write_data_addr(16, bytecode_ref, 0);
    let units_ref = object.declare_data_in_data(units_id, &mut descriptor_data);
    descriptor_data.write_data_addr(32, units_ref, 0);
    object
        .define_data(descriptor_id, &descriptor_data)
        .map_err(|error| AotError::Module(error.to_string()))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32, little_endian: bool) {
    let encoded = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64, little_endian: bool) {
    let encoded = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamts_bytecode::{
        Constant, ConstantId, Function as BytecodeFunction, FunctionFlags, FunctionId, Instruction,
        Module as BytecodeModule, Register,
    };
    use cranelift_object::object::{
        Object, ObjectSection, ObjectSymbol, RelocationTarget, SymbolIndex,
    };

    fn function(code: Vec<Instruction>, register_count: u32) -> BytecodeFunction {
        BytecodeFunction::new(
            None,
            0,
            0,
            register_count,
            FunctionFlags::default(),
            code,
            Vec::new(),
        )
    }

    fn test_module() -> BytecodeModule<Verified> {
        BytecodeModule::new(
            vec![Constant::Undefined],
            vec![
                function(
                    vec![
                        Instruction::LoadConst {
                            dst: Register::new(0),
                            constant: ConstantId::new(0),
                        },
                        Instruction::Return {
                            value: Register::new(0),
                        },
                    ],
                    1,
                ),
                function(vec![Instruction::Halt], 0),
            ],
            FunctionId::new(1),
        )
        .verify()
        .expect("test bytecode verifies")
    }

    fn target() -> &'static str {
        if cfg!(target_arch = "x86_64") {
            "x86_64-unknown-linux-gnu"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64-unknown-linux-gnu"
        } else {
            panic!("AOT object test needs a supported 64-bit host architecture")
        }
    }

    fn symbol_bytes<'a>(
        file: &'a cranelift_object::object::File<'a>,
        symbol_name: &str,
    ) -> (&'a [u8], SymbolIndex) {
        let symbol = file
            .symbols()
            .find(|symbol| symbol.name() == Ok(symbol_name))
            .unwrap_or_else(|| panic!("missing symbol {symbol_name}"));
        let section_index = symbol.section_index().expect("defined symbol section");
        let section = file
            .section_by_index(section_index)
            .expect("symbol section");
        let section_data = section.data().expect("section data");
        let start = usize::try_from(symbol.address() - section.address()).expect("symbol offset");
        let size = usize::try_from(symbol.size()).expect("symbol size");
        (&section_data[start..start + size], symbol.index())
    }

    fn relocation_targets(
        file: &cranelift_object::object::File<'_>,
        owner: SymbolIndex,
    ) -> Vec<String> {
        let owner = file.symbol_by_index(owner).expect("owner symbol");
        let section = file
            .section_by_index(owner.section_index().expect("owner section"))
            .expect("owner section data");
        let start = owner.address() - section.address();
        let end = start + owner.size();
        section
            .relocations()
            .filter(|(offset, _)| (start..end).contains(offset))
            .filter_map(|(_, relocation)| match relocation.target() {
                RelocationTarget::Symbol(index) => file
                    .symbol_by_index(index)
                    .ok()
                    .and_then(|symbol| symbol.name().ok())
                    .map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn emits_pinned_descriptor_units_and_canonical_bytecode() {
        let bytecode = test_module();
        let canonical = bytecode.encode();
        let emitted = compile_aot(&bytecode, target()).expect("AOT object emits");
        let file = cranelift_object::object::File::parse(&*emitted.bytes).expect("object parses");

        let (descriptor, descriptor_index) = symbol_bytes(&file, PROGRAM_DESCRIPTOR_SYMBOL);
        assert_eq!(descriptor.len(), PROGRAM_DESCRIPTOR_BYTES);
        assert_eq!(&descriptor[0..8], b"BMTSAOT1");
        assert_eq!(u32::from_le_bytes(descriptor[8..12].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(descriptor[12..16].try_into().unwrap()),
            0
        );
        assert_eq!(
            u64::from_le_bytes(descriptor[24..32].try_into().unwrap()),
            canonical.len() as u64
        );
        assert_eq!(
            u64::from_le_bytes(descriptor[40..48].try_into().unwrap()),
            2
        );
        assert_eq!(
            u32::from_le_bytes(descriptor[48..52].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_le_bytes(descriptor[52..56].try_into().unwrap()),
            0
        );
        assert_eq!(
            relocation_targets(&file, descriptor_index),
            [BYTECODE_SYMBOL.to_string(), UNITS_SYMBOL.to_string()]
        );

        let (embedded, _) = symbol_bytes(&file, BYTECODE_SYMBOL);
        assert_eq!(embedded, canonical);

        let (units, units_index) = symbol_bytes(&file, UNITS_SYMBOL);
        assert_eq!(units.len(), 2 * UNIT_DESCRIPTOR_BYTES);
        assert_eq!(u32::from_le_bytes(units[0..4].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(units[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(units[16..20].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(units[20..24].try_into().unwrap()), 0);
        assert_eq!(
            relocation_targets(&file, units_index),
            [function_symbol(0), function_symbol(1)]
        );
        assert!(file.symbols().any(|symbol| {
            symbol.name() == Ok(emitted.entry_symbol.as_str()) && !symbol.is_undefined()
        }));
        assert_eq!(emitted.required_helpers, [Helper::LoadConstant.symbol()]);
        assert_eq!(emitted.entry_function, 1);
    }

    #[test]
    fn rejects_32_bit_target_without_emitting_an_object() {
        let error = compile_aot(&test_module(), "x86").expect_err("32-bit AOT target is rejected");
        assert!(matches!(
            error,
            AotError::Lower(LowerError::UnsupportedPointerWidth { bits: 32 })
        ));
    }
}
