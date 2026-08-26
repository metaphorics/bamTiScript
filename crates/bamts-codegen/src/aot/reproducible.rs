use std::error::Error;
use std::fmt;

use bamts_bytecode::{Program, Verified};
use sha2::{Digest, Sha256};

use super::AotObject;
use super::emission::{
    EmissionError, EmittedObject, TargetDescriptor, content_digest, emit_for_target,
};

/// Stable object-writer name; no path or wall-clock input enters emitted bytes.
pub const REPRODUCIBLE_FILE_NAME: &str = "bamts";

/// Content identity of a source program, target, object metadata, and object bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildCacheKey {
    source_hash: [u8; 32],
    target_fingerprint: [u8; 32],
    metadata_hash: [u8; 32],
    object_hash: [u8; 32],
}

impl BuildCacheKey {
    #[must_use]
    pub fn from_parts(
        source_hash: [u8; 32],
        target_fingerprint: [u8; 32],
        metadata_hash: [u8; 32],
        object_hash: [u8; 32],
    ) -> Self {
        Self {
            source_hash,
            target_fingerprint,
            metadata_hash,
            object_hash,
        }
    }

    #[must_use]
    pub fn key(self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"bamts-build-cache/v1\0");
        hasher.update(self.source_hash);
        hasher.update(self.target_fingerprint);
        hasher.update(self.metadata_hash);
        hasher.update(self.object_hash);
        hasher.finalize().into()
    }

    #[must_use]
    pub fn key_hex(self) -> String {
        hex_digest(self.key())
    }
}

/// A byte-stable emitted object and its build-cache identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReproducibleArtifact {
    emitted: EmittedObject,
    cache_key: BuildCacheKey,
}

impl ReproducibleArtifact {
    #[must_use]
    pub const fn emitted(&self) -> &EmittedObject {
        &self.emitted
    }

    #[must_use]
    pub const fn cache_key(&self) -> BuildCacheKey {
        self.cache_key
    }
}

#[derive(Debug)]
pub enum ReproducibleError {
    Emit(EmissionError),
    NonDeterministic { first: String, second: String },
}

impl fmt::Display for ReproducibleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Emit(error) => write!(formatter, "reproducible emission failed: {error}"),
            Self::NonDeterministic { first, second } => write!(
                formatter,
                "identical AOT inputs produced different object digests: {first} != {second}"
            ),
        }
    }
}

impl Error for ReproducibleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Emit(error) => Some(error),
            Self::NonDeterministic { .. } => None,
        }
    }
}

/// Canonical, length-delimited metadata for every non-byte field of an object.
#[must_use]
pub fn canonical_object_metadata(object: &AotObject) -> Vec<u8> {
    let mut output = Vec::new();
    push_field(&mut output, object.target.as_bytes());
    push_field(&mut output, object.descriptor_symbol.as_bytes());
    output.extend_from_slice(&object.entry_module.to_le_bytes());
    output.extend_from_slice(&object.entry_function.to_le_bytes());
    push_field(&mut output, object.entry_symbol.as_bytes());

    let mut helpers = object.required_helpers.clone();
    helpers.sort_unstable();
    output.extend_from_slice(&(helpers.len() as u64).to_le_bytes());
    for helper in helpers {
        push_field(&mut output, helper.as_bytes());
    }
    output
}

/// Emits twice and accepts the artifact only when the raw object bytes match.
pub fn emit_reproducible(
    program: &Program<Verified>,
    target: &TargetDescriptor,
) -> Result<ReproducibleArtifact, ReproducibleError> {
    let first = emit_for_target(program, target).map_err(ReproducibleError::Emit)?;
    let second = emit_for_target(program, target).map_err(ReproducibleError::Emit)?;
    if first.bytes() != second.bytes() {
        return Err(ReproducibleError::NonDeterministic {
            first: first.content_digest_hex(),
            second: second.content_digest_hex(),
        });
    }

    let source_hash = content_digest(&program.encode());
    let metadata_hash = content_digest(&canonical_object_metadata(first.object()));
    let cache_key = BuildCacheKey::from_parts(
        source_hash,
        target.fingerprint(),
        metadata_hash,
        first.content_digest(),
    );
    Ok(ReproducibleArtifact {
        emitted: first,
        cache_key,
    })
}

fn push_field(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_le_bytes());
    output.extend_from_slice(value);
}

fn hex_digest(digest: [u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use bamts_bytecode::{
        Constant, ConstantId, EcmaString, Function, FunctionFlags, FunctionId, Instruction, Module,
        ModuleId, ProgramModule, Register,
    };

    use super::*;

    fn program(value: i32) -> Program<Verified> {
        let function = Function::new(
            None,
            0,
            0,
            1,
            FunctionFlags::default(),
            vec![
                Instruction::LoadConst {
                    dst: Register::new(0),
                    constant: ConstantId::new(1),
                },
                Instruction::Return {
                    value: Register::new(0),
                },
            ],
            Vec::new(),
        );
        let module = Module::new(
            vec![
                Constant::String(EcmaString::encode("reproducible-test")),
                Constant::Int32(value),
            ],
            vec![function],
            FunctionId::new(0),
        )
        .verify()
        .expect("test module verifies");
        Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code: module,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect("test program links")
    }

    fn descriptor(triple: &str) -> TargetDescriptor {
        TargetDescriptor::lookup(triple).expect("registered target resolves")
    }

    #[test]
    fn identical_inputs_emit_identical_bytes_and_keys() {
        let target = descriptor("x86_64-unknown-linux-gnu");
        let first = emit_reproducible(&program(42), &target).expect("first artifact emits");
        let second = emit_reproducible(&program(42), &target).expect("second artifact emits");
        assert_eq!(first.emitted().bytes(), second.emitted().bytes());
        assert_eq!(first.cache_key(), second.cache_key());
        assert_eq!(first.cache_key().key_hex().len(), 64);
    }

    #[test]
    fn changed_source_changes_cache_key_and_bytes() {
        let target = descriptor("x86_64-unknown-linux-gnu");
        let first = emit_reproducible(&program(42), &target).expect("first artifact emits");
        let second = emit_reproducible(&program(43), &target).expect("second artifact emits");
        assert_ne!(first.emitted().bytes(), second.emitted().bytes());
        assert_ne!(first.cache_key(), second.cache_key());
        assert_ne!(first.cache_key().key(), second.cache_key().key());
    }

    #[test]
    fn canonical_metadata_sorts_helper_symbols() {
        let target = descriptor("x86_64-unknown-linux-gnu");
        let emitted = emit_for_target(&program(42), &target).expect("artifact emits");
        let mut reordered = emitted.object().clone();
        reordered.required_helpers.reverse();
        assert_eq!(
            canonical_object_metadata(emitted.object()),
            canonical_object_metadata(&reordered)
        );
    }

    #[test]
    fn target_fingerprint_is_part_of_cache_key() {
        let source = content_digest(b"source");
        let metadata = content_digest(b"metadata");
        let object = content_digest(b"object");
        let x86 = BuildCacheKey::from_parts(
            source,
            descriptor("x86_64-unknown-linux-gnu").fingerprint(),
            metadata,
            object,
        );
        let arm = BuildCacheKey::from_parts(
            source,
            descriptor("aarch64-unknown-linux-gnu").fingerprint(),
            metadata,
            object,
        );
        assert_ne!(x86.key(), arm.key());
    }
}
