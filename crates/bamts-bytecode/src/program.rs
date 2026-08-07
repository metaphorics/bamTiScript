use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use crate::{
    Constant, ConstantId, DecodeError, DecodeLimits, EcmaString, Instruction, Module, Unverified,
    Verified, VerifyError, decode,
};

/// `BMTPC\0\0\1`: the canonical whole-program container, distinct from module magic.
pub const PROGRAM_MAGIC: [u8; 8] = [66, 77, 84, 80, 67, 0, 0, 1];
/// The sole supported program-envelope version.
pub const PROGRAM_VERSION: u8 = 4;

index_type!(
    /// Index of a module within a program.
    ModuleId
);
index_type!(
    /// Index of an edge within a module's linkage table.
    EdgeId
);
index_type!(
    /// Index of a binding within a module's binding table.
    BindingId
);

/// A module dependency. External dependencies deliberately have no path or host identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EdgeTarget {
    Local(ModuleId),
    External,
}

/// The runtime roles represented by one canonicalized module dependency.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EdgeKind {
    Static,
    Dynamic,
    StaticAndDynamic,
}

impl EdgeKind {
    #[must_use]
    pub const fn has_static(self) -> bool {
        matches!(self, Self::Static | Self::StaticAndDynamic)
    }

    #[must_use]
    pub const fn has_dynamic(self) -> bool {
        matches!(self, Self::Dynamic | Self::StaticAndDynamic)
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Static, Self::Static) => Self::Static,
            (Self::Dynamic, Self::Dynamic) => Self::Dynamic,
            (Self::StaticAndDynamic, _)
            | (_, Self::StaticAndDynamic)
            | (Self::Static, Self::Dynamic)
            | (Self::Dynamic, Self::Static) => Self::StaticAndDynamic,
        }
    }
}

/// One canonicalized module dependency.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Edge {
    pub specifier: ConstantId,
    pub target: EdgeTarget,
    pub kind: EdgeKind,
}

/// The initialization and linkage role of a module binding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BindingKind {
    Hoisted,
    Lexical,
    Imported { edge: EdgeId, name: ConstantId },
    Namespace { edge: EdgeId },
}

/// One named module binding. A binding identifies a live cell, never an activation register.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Binding {
    pub name: ConstantId,
    pub kind: BindingKind,
}

/// The source of an exported name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExportSource {
    Local(BindingId),
    Indirect { edge: EdgeId, name: ConstantId },
}

/// One named export.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Export {
    pub name: ConstantId,
    pub source: ExportSource,
}

/// A canonical module blob and its program-only identity/linkage metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramModule<State = Unverified> {
    pub name: ConstantId,
    pub code: Module<State>,
    pub edges: Vec<Edge>,
    pub bindings: Vec<Binding>,
    pub exports: Vec<Export>,
}

impl<State> ProgramModule<State> {
    #[must_use]
    pub fn name(&self) -> ConstantId {
        self.name
    }

    #[must_use]
    pub fn code(&self) -> &Module<State> {
        &self.code
    }

    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    #[must_use]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    #[must_use]
    pub fn exports(&self) -> &[Export] {
        &self.exports
    }
}

/// The sole self-contained executable wire value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Program<State = Unverified> {
    modules: Vec<ProgramModule<State>>,
    entry: ModuleId,
    state: PhantomData<State>,
}

impl<State> Program<State> {
    #[must_use]
    pub fn modules(&self) -> &[ProgramModule<State>] {
        &self.modules
    }

    #[must_use]
    pub const fn entry(&self) -> ModuleId {
        self.entry
    }

    #[must_use]
    pub fn module(&self, id: ModuleId) -> Option<&ProgramModule<State>> {
        self.modules.get(id.get() as usize)
    }
}

impl Program<Unverified> {
    #[must_use]
    pub fn new(modules: Vec<ProgramModule<Unverified>>, entry: ModuleId) -> Self {
        Self {
            modules,
            entry,
            state: PhantomData,
        }
    }

    /// Verifies every embedded module, then all program linkage invariants.
    pub fn verify(self) -> Result<Program<Verified>, ProgramVerifyError> {
        let mut modules = Vec::with_capacity(self.modules.len());
        for (index, module) in self.modules.into_iter().enumerate() {
            let ProgramModule {
                name,
                code,
                edges,
                bindings,
                exports,
            } = module;
            let code = code.verify().map_err(|error| ProgramVerifyError {
                module: Some(ModuleId::new(index as u32)),
                kind: ProgramVerifyErrorKind::Module(error),
            })?;
            modules.push(ProgramModule {
                name,
                code,
                edges,
                bindings,
                exports,
            });
        }
        Program::link(modules, self.entry)
    }
}

/// A verified export resolution with no copied names or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolvedExport {
    Local {
        module: ModuleId,
        binding: BindingId,
    },
    External {
        module: ModuleId,
        edge: EdgeId,
        name: ConstantId,
    },
}

impl Program<Verified> {
    /// Links modules that have already passed the canonical module verifier.
    pub fn link(
        modules: Vec<ProgramModule<Verified>>,
        entry: ModuleId,
    ) -> Result<Self, ProgramVerifyError> {
        verify_program_metadata(&modules, entry)?;
        Ok(Self {
            modules,
            entry,
            state: PhantomData,
        })
    }

    /// Emits the deterministic program envelope. Each module payload is exactly
    /// `Module::encode()` and is length-delimited without another module codec.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&PROGRAM_MAGIC);
        output.push(PROGRAM_VERSION);
        write_u32(self.entry.get(), &mut output);
        write_u32(self.modules.len() as u32, &mut output);
        for module in &self.modules {
            let blob = module.code.encode();
            write_u32(module.name.get(), &mut output);
            write_u32(blob.len() as u32, &mut output);
            output.extend_from_slice(&blob);
            write_u32(module.edges.len() as u32, &mut output);
            for edge in &module.edges {
                write_u32(edge.specifier.get(), &mut output);
                match edge.target {
                    EdgeTarget::Local(target) => {
                        output.push(0);
                        write_u32(target.get(), &mut output);
                    }
                    EdgeTarget::External => output.push(1),
                }
                output.push(match edge.kind {
                    EdgeKind::Static => 0,
                    EdgeKind::Dynamic => 1,
                    EdgeKind::StaticAndDynamic => 2,
                });
            }
            write_u32(module.bindings.len() as u32, &mut output);
            for binding in &module.bindings {
                write_u32(binding.name.get(), &mut output);
                match binding.kind {
                    BindingKind::Hoisted => output.push(0),
                    BindingKind::Lexical => output.push(1),
                    BindingKind::Imported { edge, name } => {
                        output.push(2);
                        write_u32(edge.get(), &mut output);
                        write_u32(name.get(), &mut output);
                    }
                    BindingKind::Namespace { edge } => {
                        output.push(3);
                        write_u32(edge.get(), &mut output);
                    }
                }
            }
            write_u32(module.exports.len() as u32, &mut output);
            for export in &module.exports {
                write_u32(export.name.get(), &mut output);
                match export.source {
                    ExportSource::Local(binding) => {
                        output.push(0);
                        write_u32(binding.get(), &mut output);
                    }
                    ExportSource::Indirect { edge, name } => {
                        output.push(1);
                        write_u32(edge.get(), &mut output);
                        write_u32(name.get(), &mut output);
                    }
                }
            }
        }
        output
    }

    /// Resolves a verified export by its exact ECMAScript name.
    ///
    /// # Termination
    ///
    /// This walk has no visit set. It terminates because a `Program<Verified>`
    /// can only be produced by [`Program::link`], which runs
    /// `verify_export_resolutions` and rejects every export cycle before this
    /// method can ever run. The two hop kinds handled below --
    /// `ExportSource::Local` through a `BindingKind::Imported` over a local
    /// edge, and `ExportSource::Indirect` over a local edge -- are exactly the
    /// hops `verify_export_resolutions` walks when detecting cycles. Any new
    /// hop kind added here must also be covered there, or the bound below
    /// turns the divergence into a `None` instead of a hang.
    #[must_use]
    pub fn resolve_export(&self, module: ModuleId, name: &EcmaString) -> Option<ResolvedExport> {
        let mut module_id = module;
        let mut linked_name = None;
        // Bound by the module count: an acyclic walk visits at most every
        // module once, so this is unreachable while the invariant above holds.
        for _ in 0..=self.modules.len() {
            let current = self.module(module_id)?;
            let export_name = linked_name.unwrap_or(name);
            let export = current
                .exports
                .iter()
                .find(|export| string(&current.code, export.name) == Some(export_name))?;
            match export.source {
                ExportSource::Local(binding) => {
                    match current.bindings.get(binding.get() as usize)?.kind {
                        BindingKind::Imported { edge, name } => {
                            let edge_id = edge;
                            let edge = current.edges.get(edge.get() as usize)?;
                            match edge.target {
                                EdgeTarget::Local(target) => {
                                    module_id = target;
                                    linked_name = Some(string(&current.code, name)?);
                                }
                                EdgeTarget::External => {
                                    return Some(ResolvedExport::External {
                                        module: module_id,
                                        edge: edge_id,
                                        name,
                                    });
                                }
                            }
                        }
                        BindingKind::Hoisted
                        | BindingKind::Lexical
                        | BindingKind::Namespace { .. } => {
                            return Some(ResolvedExport::Local {
                                module: module_id,
                                binding,
                            });
                        }
                    }
                }
                ExportSource::Indirect { edge, name } => {
                    let edge_id = edge;
                    let edge = current.edges.get(edge.get() as usize)?;
                    match edge.target {
                        EdgeTarget::Local(target) => {
                            module_id = target;
                            linked_name = Some(string(&current.code, name)?);
                        }
                        EdgeTarget::External => {
                            return Some(ResolvedExport::External {
                                module: module_id,
                                edge: edge_id,
                                name,
                            });
                        }
                    }
                }
            }
        }
        None
    }
}

/// Strict program-level resource ceilings, applied before allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramDecodeLimits {
    pub max_bytes: usize,
    pub max_modules: u32,
    pub max_module_bytes: usize,
    pub max_total_module_bytes: usize,
    pub max_edges_per_module: u32,
    pub max_bindings_per_module: u32,
    pub max_exports_per_module: u32,
    pub max_total_edges: u32,
    pub max_total_bindings: u32,
    pub max_total_exports: u32,
    pub module: DecodeLimits,
}

impl Default for ProgramDecodeLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_modules: 1 << 16,
            max_module_bytes: 16 * 1024 * 1024,
            max_total_module_bytes: 48 * 1024 * 1024,
            max_edges_per_module: 1 << 20,
            max_bindings_per_module: 1 << 20,
            max_exports_per_module: 1 << 20,
            max_total_edges: 1 << 22,
            max_total_bindings: 1 << 22,
            max_total_exports: 1 << 22,
            module: DecodeLimits::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramDecodeError {
    pub offset: usize,
    pub kind: ProgramDecodeErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramDecodeErrorKind {
    InputLimitExceeded {
        limit: usize,
        actual: usize,
    },
    UnexpectedEof,
    BadMagic {
        expected: u8,
        actual: u8,
    },
    UnsupportedVersion {
        version: u8,
    },
    NonCanonicalInteger,
    IntegerOverflow,
    InvalidEdgeTarget {
        tag: u8,
    },
    InvalidEdgeKind {
        tag: u8,
    },
    InvalidBindingKind {
        tag: u8,
    },
    InvalidExportSource {
        tag: u8,
    },
    LimitExceeded {
        field: &'static str,
        limit: u64,
        actual: u64,
    },
    AllocationFailed,
    Module {
        module: ModuleId,
        error: DecodeError,
    },
    TrailingBytes {
        count: usize,
    },
}

impl fmt::Display for ProgramDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "program byte {}: ", self.offset)?;
        match &self.kind {
            ProgramDecodeErrorKind::InputLimitExceeded { limit, actual } => {
                write!(formatter, "input has {actual} bytes, limit is {limit}")
            }
            ProgramDecodeErrorKind::UnexpectedEof => formatter.write_str("unexpected end of input"),
            ProgramDecodeErrorKind::BadMagic { expected, actual } => write!(
                formatter,
                "bad magic byte {actual:#04x}, expected {expected:#04x}"
            ),
            ProgramDecodeErrorKind::UnsupportedVersion { version } => {
                write!(formatter, "unsupported program version {version}")
            }
            ProgramDecodeErrorKind::NonCanonicalInteger => {
                formatter.write_str("noncanonical LEB128 integer")
            }
            ProgramDecodeErrorKind::IntegerOverflow => {
                formatter.write_str("LEB128 integer exceeds 32 bits")
            }
            ProgramDecodeErrorKind::InvalidEdgeTarget { tag } => {
                write!(formatter, "invalid edge target tag {tag}")
            }
            ProgramDecodeErrorKind::InvalidEdgeKind { tag } => {
                write!(formatter, "invalid edge kind tag {tag}")
            }
            ProgramDecodeErrorKind::InvalidBindingKind { tag } => {
                write!(formatter, "invalid binding kind tag {tag}")
            }
            ProgramDecodeErrorKind::InvalidExportSource { tag } => {
                write!(formatter, "invalid export source tag {tag}")
            }
            ProgramDecodeErrorKind::LimitExceeded {
                field,
                limit,
                actual,
            } => {
                write!(formatter, "{field} value {actual} exceeds limit {limit}")
            }
            ProgramDecodeErrorKind::AllocationFailed => {
                formatter.write_str("failed to reserve decode buffer")
            }
            ProgramDecodeErrorKind::Module { module, error } => {
                write!(formatter, "module {}: {error}", module.get())
            }
            ProgramDecodeErrorKind::TrailingBytes { count } => {
                write!(formatter, "{count} trailing bytes")
            }
        }
    }
}

impl Error for ProgramDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            ProgramDecodeErrorKind::Module { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramVerifyError {
    pub module: Option<ModuleId>,
    pub kind: ProgramVerifyErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramVerifyErrorKind {
    EmptyProgram,
    TooManyModules {
        count: usize,
    },
    EntryModuleOutOfBounds {
        entry: u32,
        module_count: usize,
    },
    Module(VerifyError),
    ModuleNameOutOfBounds {
        constant: ConstantId,
    },
    ModuleNameNotString {
        constant: ConstantId,
    },
    InvalidModuleName,
    MetadataStringIllFormed {
        constant: ConstantId,
    },
    DuplicateModuleName {
        first: ModuleId,
    },
    TooManyEdges {
        count: usize,
    },
    TooManyBindings {
        count: usize,
    },
    TooManyExports {
        count: usize,
    },
    SpecifierOutOfBounds {
        edge: EdgeId,
        constant: ConstantId,
    },
    SpecifierNotString {
        edge: EdgeId,
        constant: ConstantId,
    },
    AbsoluteSpecifier {
        edge: EdgeId,
    },
    DuplicateSpecifier {
        first: EdgeId,
        second: EdgeId,
    },
    LocalTargetOutOfBounds {
        edge: EdgeId,
        target: ModuleId,
    },
    BindingNameOutOfBounds {
        binding: BindingId,
        constant: ConstantId,
    },
    BindingNameNotString {
        binding: BindingId,
        constant: ConstantId,
    },
    BindingEdgeOutOfBounds {
        binding: BindingId,
        edge: EdgeId,
    },
    ImportedNameOutOfBounds {
        binding: BindingId,
        constant: ConstantId,
    },
    ImportedNameNotString {
        binding: BindingId,
        constant: ConstantId,
    },
    DuplicateBinding {
        first: BindingId,
        second: BindingId,
    },
    StaticBindingRequiresStaticEdge {
        binding: BindingId,
        edge: EdgeId,
    },
    IndirectExportRequiresStaticEdge {
        export: u32,
        edge: EdgeId,
    },
    MissingImportedExport {
        binding: BindingId,
    },
    ExportNameOutOfBounds {
        export: u32,
        constant: ConstantId,
    },
    ExportNameNotString {
        export: u32,
        constant: ConstantId,
    },
    DuplicateExport {
        first: u32,
        second: u32,
    },
    ExportBindingOutOfBounds {
        export: u32,
        binding: BindingId,
    },
    ExportEdgeOutOfBounds {
        export: u32,
        edge: EdgeId,
    },
    IndirectNameOutOfBounds {
        export: u32,
        constant: ConstantId,
    },
    IndirectNameNotString {
        export: u32,
        constant: ConstantId,
    },
    DynamicImportMissingEdge {
        specifier: ConstantId,
    },
    SnapshotExportInstruction {
        function: u32,
        pc: u32,
    },
    MissingIndirectExport {
        export: u32,
    },
    IndirectExportCycle {
        export: u32,
    },
}

impl fmt::Display for ProgramVerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(module) = self.module {
            write!(formatter, "module {}: ", module.get())?;
        }
        match &self.kind {
            ProgramVerifyErrorKind::EmptyProgram => formatter.write_str("program has no modules"),
            ProgramVerifyErrorKind::TooManyModules { count } => {
                write!(formatter, "{count} modules exceed the program limit")
            }
            ProgramVerifyErrorKind::EntryModuleOutOfBounds {
                entry,
                module_count,
            } => write!(
                formatter,
                "entry module {entry} is outside {module_count} modules"
            ),
            ProgramVerifyErrorKind::Module(verify_error) => {
                write!(formatter, "{verify_error}")
            }
            ProgramVerifyErrorKind::ModuleNameOutOfBounds { constant } => write!(
                formatter,
                "module name constant {} is out of bounds",
                constant.get()
            ),
            ProgramVerifyErrorKind::ModuleNameNotString { constant } => write!(
                formatter,
                "module name constant {} is not a string",
                constant.get()
            ),
            ProgramVerifyErrorKind::InvalidModuleName => {
                formatter.write_str("module name is not a normalized module name")
            }
            ProgramVerifyErrorKind::MetadataStringIllFormed { constant } => write!(
                formatter,
                "metadata string constant {} is ill-formed UTF-16",
                constant.get()
            ),
            ProgramVerifyErrorKind::DuplicateModuleName { first } => write!(
                formatter,
                "duplicate module name, first seen at module {}",
                first.get()
            ),
            ProgramVerifyErrorKind::TooManyEdges { count } => {
                write!(formatter, "{count} edges exceed the per-module limit")
            }
            ProgramVerifyErrorKind::TooManyBindings { count } => {
                write!(formatter, "{count} bindings exceed the per-module limit")
            }
            ProgramVerifyErrorKind::TooManyExports { count } => {
                write!(formatter, "{count} exports exceed the per-module limit")
            }
            ProgramVerifyErrorKind::SpecifierOutOfBounds { edge, constant } => write!(
                formatter,
                "edge {} specifier constant {} is out of bounds",
                edge.get(),
                constant.get()
            ),
            ProgramVerifyErrorKind::SpecifierNotString { edge, constant } => write!(
                formatter,
                "edge {} specifier constant {} is not a string",
                edge.get(),
                constant.get()
            ),
            ProgramVerifyErrorKind::AbsoluteSpecifier { edge } => {
                write!(formatter, "edge {} has an absolute specifier", edge.get())
            }
            ProgramVerifyErrorKind::DuplicateSpecifier { first, second } => write!(
                formatter,
                "edges {} and {} share one specifier",
                first.get(),
                second.get()
            ),
            ProgramVerifyErrorKind::LocalTargetOutOfBounds { edge, target } => write!(
                formatter,
                "edge {} targets module {} which is out of bounds",
                edge.get(),
                target.get()
            ),
            ProgramVerifyErrorKind::BindingNameOutOfBounds { binding, constant } => write!(
                formatter,
                "binding {} name constant {} is out of bounds",
                binding.get(),
                constant.get()
            ),
            ProgramVerifyErrorKind::BindingNameNotString { binding, constant } => write!(
                formatter,
                "binding {} name constant {} is not a string",
                binding.get(),
                constant.get()
            ),
            ProgramVerifyErrorKind::BindingEdgeOutOfBounds { binding, edge } => write!(
                formatter,
                "binding {} references edge {} which is out of bounds",
                binding.get(),
                edge.get()
            ),
            ProgramVerifyErrorKind::ImportedNameOutOfBounds { binding, constant } => write!(
                formatter,
                "binding {} imported name constant {} is out of bounds",
                binding.get(),
                constant.get()
            ),
            ProgramVerifyErrorKind::ImportedNameNotString { binding, constant } => write!(
                formatter,
                "binding {} imported name constant {} is not a string",
                binding.get(),
                constant.get()
            ),
            ProgramVerifyErrorKind::DuplicateBinding { first, second } => write!(
                formatter,
                "bindings {} and {} share one name",
                first.get(),
                second.get()
            ),
            ProgramVerifyErrorKind::StaticBindingRequiresStaticEdge { binding, edge } => write!(
                formatter,
                "binding {} requires a static edge but edge {} is not static",
                binding.get(),
                edge.get()
            ),
            ProgramVerifyErrorKind::IndirectExportRequiresStaticEdge { export, edge } => write!(
                formatter,
                "export {export} requires a static edge but edge {} is not static",
                edge.get()
            ),
            ProgramVerifyErrorKind::MissingImportedExport { binding } => write!(
                formatter,
                "binding {} imports a name not exported by its edge",
                binding.get()
            ),
            ProgramVerifyErrorKind::ExportNameOutOfBounds { export, constant } => write!(
                formatter,
                "export {export} name constant {} is out of bounds",
                constant.get()
            ),
            ProgramVerifyErrorKind::ExportNameNotString { export, constant } => write!(
                formatter,
                "export {export} name constant {} is not a string",
                constant.get()
            ),
            ProgramVerifyErrorKind::DuplicateExport { first, second } => {
                write!(formatter, "exports {first} and {second} share one name")
            }
            ProgramVerifyErrorKind::ExportBindingOutOfBounds { export, binding } => write!(
                formatter,
                "export {export} references binding {} which is out of bounds",
                binding.get()
            ),
            ProgramVerifyErrorKind::ExportEdgeOutOfBounds { export, edge } => write!(
                formatter,
                "export {export} references edge {} which is out of bounds",
                edge.get()
            ),
            ProgramVerifyErrorKind::IndirectNameOutOfBounds { export, constant } => write!(
                formatter,
                "export {export} indirect name constant {} is out of bounds",
                constant.get()
            ),
            ProgramVerifyErrorKind::IndirectNameNotString { export, constant } => write!(
                formatter,
                "export {export} indirect name constant {} is not a string",
                constant.get()
            ),
            ProgramVerifyErrorKind::DynamicImportMissingEdge { specifier } => write!(
                formatter,
                "dynamic import specifier constant {} has no matching edge",
                specifier.get()
            ),
            ProgramVerifyErrorKind::SnapshotExportInstruction { function, pc } => write!(
                formatter,
                "function {function} at instruction {pc} performs a snapshot export, which is not executable"
            ),
            ProgramVerifyErrorKind::MissingIndirectExport { export } => write!(
                formatter,
                "export {export} is an indirect export with no resolution"
            ),
            ProgramVerifyErrorKind::IndirectExportCycle { export } => write!(
                formatter,
                "export {export} is part of an indirect export cycle"
            ),
        }
    }
}

impl Error for ProgramVerifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            ProgramVerifyErrorKind::Module(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgramLoadError {
    Decode(ProgramDecodeError),
    Verify(ProgramVerifyError),
}

impl fmt::Display for ProgramLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(formatter),
            Self::Verify(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProgramLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error),
            Self::Verify(error) => Some(error),
        }
    }
}

/// Strictly decodes a program envelope while retaining unverified typestate.
pub fn decode_program(
    bytes: &[u8],
    limits: &ProgramDecodeLimits,
) -> Result<Program<Unverified>, ProgramDecodeError> {
    if bytes.len() > limits.max_bytes {
        return Err(ProgramDecodeError {
            offset: 0,
            kind: ProgramDecodeErrorKind::InputLimitExceeded {
                limit: limits.max_bytes,
                actual: bytes.len(),
            },
        });
    }
    let mut decoder = ProgramDecoder {
        bytes,
        offset: 0,
        limits,
        total_module_bytes: 0,
        total_edges: 0,
        total_bindings: 0,
        total_exports: 0,
    };
    for expected in PROGRAM_MAGIC {
        let at = decoder.offset;
        let actual = decoder.byte()?;
        if actual != expected {
            return Err(decoder.error_at(at, ProgramDecodeErrorKind::BadMagic { expected, actual }));
        }
    }
    let version_at = decoder.offset;
    let version = decoder.byte()?;
    if version != PROGRAM_VERSION {
        return Err(decoder.error_at(
            version_at,
            ProgramDecodeErrorKind::UnsupportedVersion { version },
        ));
    }
    let entry = ModuleId::new(decoder.u32()?);
    let module_count = decoder.count("modules", limits.max_modules)?;
    let mut modules = Vec::new();
    for index in 0..module_count {
        let module = decoder.module(ModuleId::new(index as u32))?;
        decoder.push_decoded(&mut modules, module)?;
    }
    if decoder.offset != bytes.len() {
        return Err(decoder.error(ProgramDecodeErrorKind::TrailingBytes {
            count: bytes.len() - decoder.offset,
        }));
    }
    Ok(Program::new(modules, entry))
}

/// Decodes and verifies a whole program in one boundary operation.
pub fn decode_verified_program(
    bytes: &[u8],
    limits: &ProgramDecodeLimits,
) -> Result<Program<Verified>, ProgramLoadError> {
    decode_program(bytes, limits)
        .map_err(ProgramLoadError::Decode)?
        .verify()
        .map_err(ProgramLoadError::Verify)
}

struct ProgramDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
    limits: &'a ProgramDecodeLimits,
    total_module_bytes: usize,
    total_edges: u64,
    total_bindings: u64,
    total_exports: u64,
}

impl<'a> ProgramDecoder<'a> {
    fn module(
        &mut self,
        module_id: ModuleId,
    ) -> Result<ProgramModule<Unverified>, ProgramDecodeError> {
        let name = ConstantId::new(self.u32()?);
        let length = self.length("module bytes", self.limits.max_module_bytes)?;
        self.total_module_bytes = self
            .total_module_bytes
            .checked_add(length)
            .ok_or_else(|| self.error(ProgramDecodeErrorKind::IntegerOverflow))?;
        if self.total_module_bytes > self.limits.max_total_module_bytes {
            return Err(self.error(ProgramDecodeErrorKind::LimitExceeded {
                field: "total module bytes",
                limit: self.limits.max_total_module_bytes as u64,
                actual: self.total_module_bytes as u64,
            }));
        }
        let blob_offset = self.offset;
        let blob = self.slice(length)?;
        let mut module_limits = self.limits.module.clone();
        module_limits.max_bytes = module_limits.max_bytes.min(self.limits.max_module_bytes);
        let code = decode(blob, &module_limits).map_err(|error| {
            self.error_at(
                blob_offset + error.offset,
                ProgramDecodeErrorKind::Module {
                    module: module_id,
                    error,
                },
            )
        })?;

        let edge_count = self.count("edges", self.limits.max_edges_per_module)?;
        self.add_total(
            "total edges",
            edge_count,
            self.limits.max_total_edges,
            Total::Edges,
        )?;
        let mut edges = Vec::new();
        for _ in 0..edge_count {
            let specifier = ConstantId::new(self.u32()?);
            let tag_at = self.offset;
            let target = match self.byte()? {
                0 => EdgeTarget::Local(ModuleId::new(self.u32()?)),
                1 => EdgeTarget::External,
                tag => {
                    return Err(
                        self.error_at(tag_at, ProgramDecodeErrorKind::InvalidEdgeTarget { tag })
                    );
                }
            };
            let kind_at = self.offset;
            let kind = match self.byte()? {
                0 => EdgeKind::Static,
                1 => EdgeKind::Dynamic,
                2 => EdgeKind::StaticAndDynamic,
                tag => {
                    return Err(
                        self.error_at(kind_at, ProgramDecodeErrorKind::InvalidEdgeKind { tag })
                    );
                }
            };
            self.push_decoded(
                &mut edges,
                Edge {
                    specifier,
                    target,
                    kind,
                },
            )?;
        }

        let binding_count = self.count("bindings", self.limits.max_bindings_per_module)?;
        self.add_total(
            "total bindings",
            binding_count,
            self.limits.max_total_bindings,
            Total::Bindings,
        )?;
        let mut bindings = Vec::new();
        for _ in 0..binding_count {
            let name = ConstantId::new(self.u32()?);
            let tag_at = self.offset;
            let kind = match self.byte()? {
                0 => BindingKind::Hoisted,
                1 => BindingKind::Lexical,
                2 => BindingKind::Imported {
                    edge: EdgeId::new(self.u32()?),
                    name: ConstantId::new(self.u32()?),
                },
                3 => BindingKind::Namespace {
                    edge: EdgeId::new(self.u32()?),
                },
                tag => {
                    return Err(
                        self.error_at(tag_at, ProgramDecodeErrorKind::InvalidBindingKind { tag })
                    );
                }
            };
            self.push_decoded(&mut bindings, Binding { name, kind })?;
        }

        let export_count = self.count("exports", self.limits.max_exports_per_module)?;
        self.add_total(
            "total exports",
            export_count,
            self.limits.max_total_exports,
            Total::Exports,
        )?;
        let mut exports = Vec::new();
        for _ in 0..export_count {
            let name = ConstantId::new(self.u32()?);
            let tag_at = self.offset;
            let source = match self.byte()? {
                0 => ExportSource::Local(BindingId::new(self.u32()?)),
                1 => ExportSource::Indirect {
                    edge: EdgeId::new(self.u32()?),
                    name: ConstantId::new(self.u32()?),
                },
                tag => {
                    return Err(
                        self.error_at(tag_at, ProgramDecodeErrorKind::InvalidExportSource { tag })
                    );
                }
            };
            self.push_decoded(&mut exports, Export { name, source })?;
        }
        Ok(ProgramModule {
            name,
            code,
            edges,
            bindings,
            exports,
        })
    }

    fn add_total(
        &mut self,
        field: &'static str,
        count: usize,
        limit: u32,
        total: Total,
    ) -> Result<(), ProgramDecodeError> {
        let slot = match total {
            Total::Edges => &mut self.total_edges,
            Total::Bindings => &mut self.total_bindings,
            Total::Exports => &mut self.total_exports,
        };
        *slot += count as u64;
        if *slot > u64::from(limit) {
            return Err(ProgramDecodeError {
                offset: self.offset,
                kind: ProgramDecodeErrorKind::LimitExceeded {
                    field,
                    limit: u64::from(limit),
                    actual: *slot,
                },
            });
        }
        Ok(())
    }

    fn count(&mut self, field: &'static str, limit: u32) -> Result<usize, ProgramDecodeError> {
        let actual = self.u32()?;
        if actual > limit {
            return Err(self.error(ProgramDecodeErrorKind::LimitExceeded {
                field,
                limit: u64::from(limit),
                actual: u64::from(actual),
            }));
        }
        Ok(actual as usize)
    }

    fn push_decoded<T>(&self, values: &mut Vec<T>, value: T) -> Result<(), ProgramDecodeError> {
        values
            .try_reserve(1)
            .map_err(|_| self.error(ProgramDecodeErrorKind::AllocationFailed))?;
        values.push(value);
        Ok(())
    }

    fn length(&mut self, field: &'static str, limit: usize) -> Result<usize, ProgramDecodeError> {
        let actual = self.u32()? as usize;
        if actual > limit {
            return Err(self.error(ProgramDecodeErrorKind::LimitExceeded {
                field,
                limit: limit as u64,
                actual: actual as u64,
            }));
        }
        Ok(actual)
    }

    fn byte(&mut self) -> Result<u8, ProgramDecodeError> {
        let byte = self
            .bytes
            .get(self.offset)
            .copied()
            .ok_or_else(|| self.error(ProgramDecodeErrorKind::UnexpectedEof))?;
        self.offset += 1;
        Ok(byte)
    }

    fn slice(&mut self, length: usize) -> Result<&'a [u8], ProgramDecodeError> {
        let end = self
            .offset
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| self.error(ProgramDecodeErrorKind::UnexpectedEof))?;
        let result = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(result)
    }

    fn u32(&mut self) -> Result<u32, ProgramDecodeError> {
        let start = self.offset;
        read_leb128(self.bytes, &mut self.offset).map_err(|error| {
            let kind = match error {
                Leb128Error::UnexpectedEof => ProgramDecodeErrorKind::UnexpectedEof,
                Leb128Error::IntegerOverflow => ProgramDecodeErrorKind::IntegerOverflow,
                Leb128Error::NonCanonicalInteger => ProgramDecodeErrorKind::NonCanonicalInteger,
            };
            self.error_at(start, kind)
        })
    }

    const fn error(&self, kind: ProgramDecodeErrorKind) -> ProgramDecodeError {
        self.error_at(self.offset, kind)
    }

    const fn error_at(&self, offset: usize, kind: ProgramDecodeErrorKind) -> ProgramDecodeError {
        ProgramDecodeError { offset, kind }
    }
}

/// Failure modes for canonical unsigned LEB128 `u32` decoding.
///
/// This is the sole LEB128 error taxonomy in the crate: both
/// [`ProgramDecoder`](struct@ProgramDecoder) and [`crate::Decoder`] delegate to
/// [`read_leb128`] and map these variants into their own error enums.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Leb128Error {
    UnexpectedEof,
    IntegerOverflow,
    NonCanonicalInteger,
}

/// Reads one canonical unsigned LEB128 `u32` from `bytes` starting at
/// `*offset`, advancing `*offset` past the consumed bytes.
///
/// Rejects EOF mid-integer, overlong (trailing-zero) encodings, and values
/// exceeding 32 bits. On error `*offset` is left at the point of failure;
/// callers that need the original position for diagnostics should save it
/// before calling.
pub(crate) fn read_leb128(bytes: &[u8], offset: &mut usize) -> Result<u32, Leb128Error> {
    let start = *offset;
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*offset).ok_or(Leb128Error::UnexpectedEof)?;
        *offset += 1;
        if shift == 28 {
            // Fifth group: only the low four bits may be set, and the
            // continuation bit must be clear (else overflow); a zero final
            // group would be overlong.
            if byte & 0x80 != 0 || byte > 0x0f {
                return Err(Leb128Error::IntegerOverflow);
            }
            if byte == 0 {
                return Err(Leb128Error::NonCanonicalInteger);
            }
            return Ok(result | (u32::from(byte) << 28));
        }
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            if byte == 0 && *offset - start > 1 {
                return Err(Leb128Error::NonCanonicalInteger);
            }
            return Ok(result);
        }
        shift += 7;
    }
}

enum Total {
    Edges,
    Bindings,
    Exports,
}

fn verify_program_metadata(
    modules: &[ProgramModule<Verified>],
    entry: ModuleId,
) -> Result<(), ProgramVerifyError> {
    if modules.is_empty() {
        return Err(program_error(ProgramVerifyErrorKind::EmptyProgram));
    }
    if modules.len() > u32::MAX as usize {
        return Err(program_error(ProgramVerifyErrorKind::TooManyModules {
            count: modules.len(),
        }));
    }
    if entry.get() as usize >= modules.len() {
        return Err(program_error(
            ProgramVerifyErrorKind::EntryModuleOutOfBounds {
                entry: entry.get(),
                module_count: modules.len(),
            },
        ));
    }

    let mut module_names = HashMap::with_capacity(modules.len());
    for (index, module) in modules.iter().enumerate() {
        let module_id = ModuleId::new(index as u32);
        let name = required_string(
            module_id,
            module,
            module.name,
            ProgramVerifyErrorKind::ModuleNameOutOfBounds {
                constant: module.name,
            },
            ProgramVerifyErrorKind::ModuleNameNotString {
                constant: module.name,
            },
        )?;
        if !is_normalized_module_name(name) {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::InvalidModuleName,
            ));
        }
        if let Some(first) = module_names.insert(name, module_id) {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::DuplicateModuleName { first },
            ));
        }
        verify_module_metadata(modules, module_id, module)?;
    }
    // Built once and shared by both verifiers so they cannot disagree about
    // what "exported" means. `verify_module_metadata` already rejected
    // out-of-bounds/non-string export names above, so the lookup is total.
    let export_indices: Vec<HashMap<&EcmaString, usize>> = modules
        .iter()
        .map(|module| {
            module
                .exports
                .iter()
                .enumerate()
                .map(|(index, export)| {
                    (
                        string(&module.code, export.name)
                            .expect("metadata verifier checked export name"),
                        index,
                    )
                })
                .collect()
        })
        .collect();
    verify_export_resolutions(modules, &export_indices)?;
    verify_imported_bindings(modules, &export_indices)
}

fn verify_module_metadata(
    modules: &[ProgramModule<Verified>],
    module_id: ModuleId,
    module: &ProgramModule<Verified>,
) -> Result<(), ProgramVerifyError> {
    if module.edges.len() > u32::MAX as usize {
        return Err(module_error(
            module_id,
            ProgramVerifyErrorKind::TooManyEdges {
                count: module.edges.len(),
            },
        ));
    }
    if module.bindings.len() > u32::MAX as usize {
        return Err(module_error(
            module_id,
            ProgramVerifyErrorKind::TooManyBindings {
                count: module.bindings.len(),
            },
        ));
    }
    if module.exports.len() > u32::MAX as usize {
        return Err(module_error(
            module_id,
            ProgramVerifyErrorKind::TooManyExports {
                count: module.exports.len(),
            },
        ));
    }

    let mut specifiers = HashMap::with_capacity(module.edges.len());
    for (index, edge) in module.edges.iter().enumerate() {
        let edge_id = EdgeId::new(index as u32);
        let specifier = required_string(
            module_id,
            module,
            edge.specifier,
            ProgramVerifyErrorKind::SpecifierOutOfBounds {
                edge: edge_id,
                constant: edge.specifier,
            },
            ProgramVerifyErrorKind::SpecifierNotString {
                edge: edge_id,
                constant: edge.specifier,
            },
        )?;
        if is_absolute_specifier(specifier) {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::AbsoluteSpecifier { edge: edge_id },
            ));
        }
        if let Some(first) = specifiers.insert(specifier, edge_id) {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::DuplicateSpecifier {
                    first,
                    second: edge_id,
                },
            ));
        }
        if let EdgeTarget::Local(target) = edge.target
            && target.get() as usize >= modules.len()
        {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::LocalTargetOutOfBounds {
                    edge: edge_id,
                    target,
                },
            ));
        }
    }

    let mut binding_names = HashMap::with_capacity(module.bindings.len());
    for (index, binding) in module.bindings.iter().enumerate() {
        let binding_id = BindingId::new(index as u32);
        let binding_name = required_string(
            module_id,
            module,
            binding.name,
            ProgramVerifyErrorKind::BindingNameOutOfBounds {
                binding: binding_id,
                constant: binding.name,
            },
            ProgramVerifyErrorKind::BindingNameNotString {
                binding: binding_id,
                constant: binding.name,
            },
        )?;
        if let Some(first) = binding_names.insert(binding_name, binding_id) {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::DuplicateBinding {
                    first,
                    second: binding_id,
                },
            ));
        }
        match binding.kind {
            BindingKind::Imported { edge, name } => {
                let dependency = require_edge(module_id, module, binding_id, edge)?;
                required_string(
                    module_id,
                    module,
                    name,
                    ProgramVerifyErrorKind::ImportedNameOutOfBounds {
                        binding: binding_id,
                        constant: name,
                    },
                    ProgramVerifyErrorKind::ImportedNameNotString {
                        binding: binding_id,
                        constant: name,
                    },
                )?;
                if !dependency.kind.has_static() {
                    return Err(module_error(
                        module_id,
                        ProgramVerifyErrorKind::StaticBindingRequiresStaticEdge {
                            binding: binding_id,
                            edge,
                        },
                    ));
                }
            }
            BindingKind::Namespace { edge } => {
                let dependency = require_edge(module_id, module, binding_id, edge)?;
                if !dependency.kind.has_static() {
                    return Err(module_error(
                        module_id,
                        ProgramVerifyErrorKind::StaticBindingRequiresStaticEdge {
                            binding: binding_id,
                            edge,
                        },
                    ));
                }
            }
            BindingKind::Hoisted | BindingKind::Lexical => {}
        }
    }

    let mut export_names = HashMap::with_capacity(module.exports.len());
    for (index, export) in module.exports.iter().enumerate() {
        let export_id = index as u32;
        let export_name = required_string(
            module_id,
            module,
            export.name,
            ProgramVerifyErrorKind::ExportNameOutOfBounds {
                export: export_id,
                constant: export.name,
            },
            ProgramVerifyErrorKind::ExportNameNotString {
                export: export_id,
                constant: export.name,
            },
        )?;
        if let Some(first) = export_names.insert(export_name, export_id) {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::DuplicateExport {
                    first,
                    second: export_id,
                },
            ));
        }
        match export.source {
            ExportSource::Local(binding) => {
                if binding.get() as usize >= module.bindings.len() {
                    return Err(module_error(
                        module_id,
                        ProgramVerifyErrorKind::ExportBindingOutOfBounds {
                            export: export_id,
                            binding,
                        },
                    ));
                }
            }
            ExportSource::Indirect { edge, name } => {
                let dependency = module.edges.get(edge.get() as usize).ok_or_else(|| {
                    module_error(
                        module_id,
                        ProgramVerifyErrorKind::ExportEdgeOutOfBounds {
                            export: export_id,
                            edge,
                        },
                    )
                })?;
                if !dependency.kind.has_static() {
                    return Err(module_error(
                        module_id,
                        ProgramVerifyErrorKind::IndirectExportRequiresStaticEdge {
                            export: export_id,
                            edge,
                        },
                    ));
                }
                required_string(
                    module_id,
                    module,
                    name,
                    ProgramVerifyErrorKind::IndirectNameOutOfBounds {
                        export: export_id,
                        constant: name,
                    },
                    ProgramVerifyErrorKind::IndirectNameNotString {
                        export: export_id,
                        constant: name,
                    },
                )?;
            }
        }
    }

    for (function_index, function) in module.code.functions().iter().enumerate() {
        if let Some(name) = function.name()
            && !string(&module.code, name)
                .expect("module verifier checked function-name string")
                .is_well_formed()
        {
            return Err(module_error(
                module_id,
                ProgramVerifyErrorKind::MetadataStringIllFormed { constant: name },
            ));
        }
        for (pc, instruction) in function.code().iter().copied().enumerate() {
            match instruction {
                Instruction::Import { specifier, .. } => {
                    let import_name =
                        string(&module.code, specifier).expect("module verifier checked string");
                    if !import_name.is_well_formed() {
                        return Err(module_error(
                            module_id,
                            ProgramVerifyErrorKind::MetadataStringIllFormed {
                                constant: specifier,
                            },
                        ));
                    }
                    if !specifiers
                        .get(import_name)
                        .is_some_and(|edge| module.edges[edge.get() as usize].kind.has_dynamic())
                    {
                        return Err(module_error(
                            module_id,
                            ProgramVerifyErrorKind::DynamicImportMissingEdge { specifier },
                        ));
                    }
                }
                Instruction::Export { .. } => {
                    return Err(module_error(
                        module_id,
                        ProgramVerifyErrorKind::SnapshotExportInstruction {
                            function: function_index as u32,
                            pc: pc as u32,
                        },
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn required_string(
    module_id: ModuleId,
    module: &ProgramModule<Verified>,
    id: ConstantId,
    bounds: ProgramVerifyErrorKind,
    kind: ProgramVerifyErrorKind,
) -> Result<&EcmaString, ProgramVerifyError> {
    match module.code.constants().get(id.get() as usize) {
        None => Err(module_error(module_id, bounds)),
        Some(Constant::String(value)) if value.is_well_formed() => Ok(value),
        Some(Constant::String(_)) => Err(module_error(
            module_id,
            ProgramVerifyErrorKind::MetadataStringIllFormed { constant: id },
        )),
        Some(_) => Err(module_error(module_id, kind)),
    }
}

fn require_edge(
    module_id: ModuleId,
    module: &ProgramModule<Verified>,
    binding: BindingId,
    edge: EdgeId,
) -> Result<&Edge, ProgramVerifyError> {
    module.edges.get(edge.get() as usize).ok_or_else(|| {
        module_error(
            module_id,
            ProgramVerifyErrorKind::BindingEdgeOutOfBounds { binding, edge },
        )
    })
}

fn verify_imported_bindings(
    modules: &[ProgramModule<Verified>],
    export_indices: &[HashMap<&EcmaString, usize>],
) -> Result<(), ProgramVerifyError> {
    for (module_index, module) in modules.iter().enumerate() {
        let module_id = ModuleId::new(module_index as u32);
        for (binding_index, binding) in module.bindings.iter().enumerate() {
            let BindingKind::Imported { edge, name } = binding.kind else {
                continue;
            };
            let EdgeTarget::Local(target) = module.edges[edge.get() as usize].target else {
                continue;
            };
            let imported_name =
                string(&module.code, name).expect("metadata verifier checked imported name");
            // O(1) lookup against the shared index instead of a linear scan of
            // the target's exports; the index is the same one
            // `verify_export_resolutions` resolves through.
            if !export_indices[target.get() as usize].contains_key(imported_name) {
                return Err(module_error(
                    module_id,
                    ProgramVerifyErrorKind::MissingImportedExport {
                        binding: BindingId::new(binding_index as u32),
                    },
                ));
            }
        }
    }
    Ok(())
}

fn verify_export_resolutions(
    modules: &[ProgramModule<Verified>],
    export_indices: &[HashMap<&EcmaString, usize>],
) -> Result<(), ProgramVerifyError> {
    let mut states: Vec<Vec<u8>> = modules
        .iter()
        .map(|module| vec![0; module.exports.len()])
        .collect();
    let mut stack = Vec::new();
    for (module_index, module) in modules.iter().enumerate() {
        for export_index in 0..module.exports.len() {
            if states[module_index][export_index] == 2 {
                continue;
            }
            stack.clear();
            let mut current = (module_index, export_index);
            loop {
                match states[current.0][current.1] {
                    2 => break,
                    1 => {
                        return Err(module_error(
                            ModuleId::new(current.0 as u32),
                            ProgramVerifyErrorKind::IndirectExportCycle {
                                export: current.1 as u32,
                            },
                        ));
                    }
                    _ => {}
                }
                states[current.0][current.1] = 1;
                stack.push(current);
                let current_module = &modules[current.0];
                let next = match current_module.exports[current.1].source {
                    ExportSource::Local(binding) => {
                        match current_module.bindings[binding.get() as usize].kind {
                            BindingKind::Imported { edge, name } => {
                                let edge = &current_module.edges[edge.get() as usize];
                                match edge.target {
                                    EdgeTarget::External => None,
                                    EdgeTarget::Local(target) => {
                                        let name = string(&current_module.code, name)
                                            .expect("metadata verifier checked imported name");
                                        let target_index = target.get() as usize;
                                        let target_export = export_indices[target_index]
                                            .get(name)
                                            .copied()
                                            .ok_or_else(|| {
                                                module_error(
                                                    ModuleId::new(current.0 as u32),
                                                    ProgramVerifyErrorKind::MissingImportedExport {
                                                        binding,
                                                    },
                                                )
                                            })?;
                                        Some((target_index, target_export))
                                    }
                                }
                            }
                            BindingKind::Hoisted
                            | BindingKind::Lexical
                            | BindingKind::Namespace { .. } => None,
                        }
                    }
                    ExportSource::Indirect { edge, name } => {
                        let edge = &current_module.edges[edge.get() as usize];
                        match edge.target {
                            EdgeTarget::External => None,
                            EdgeTarget::Local(target) => {
                                let name = string(&current_module.code, name)
                                    .expect("metadata verifier checked indirect name");
                                let target_index = target.get() as usize;
                                let target_export = export_indices[target_index]
                                    .get(name)
                                    .copied()
                                    .ok_or_else(|| {
                                        module_error(
                                            ModuleId::new(current.0 as u32),
                                            ProgramVerifyErrorKind::MissingIndirectExport {
                                                export: current.1 as u32,
                                            },
                                        )
                                    })?;
                                Some((target_index, target_export))
                            }
                        }
                    }
                };
                let Some(next) = next else {
                    break;
                };
                current = next;
            }
            for &(resolved_module, resolved_export) in &stack {
                states[resolved_module][resolved_export] = 2;
            }
        }
    }
    Ok(())
}

fn string(module: &Module<Verified>, id: ConstantId) -> Option<&EcmaString> {
    match module.constants().get(id.get() as usize) {
        Some(Constant::String(value)) => Some(value),
        _ => None,
    }
}

fn is_normalized_module_name(name: &EcmaString) -> bool {
    let units = name.as_units();
    if units.is_empty()
        || units.first() == Some(&u16::from(b'/'))
        || units.contains(&u16::from(b'\\'))
        || units.contains(&0)
        || units.split(|&unit| unit == u16::from(b'/')).any(|part| {
            part.is_empty()
                || part == [u16::from(b'.')]
                || part == [u16::from(b'.'), u16::from(b'.')]
        })
    {
        return false;
    }
    !units
        .split(|&unit| unit == u16::from(b'/'))
        .next()
        .unwrap_or_default()
        .contains(&u16::from(b':'))
}

fn is_absolute_specifier(specifier: &EcmaString) -> bool {
    let units = specifier.as_units();
    units.starts_with(&[u16::from(b'/')])
        || units.starts_with(&[u16::from(b'\\')])
        || units.get(..5).is_some_and(|prefix| {
            prefix.iter().copied().zip(*b"file:").all(|(unit, ascii)| {
                unit == u16::from(ascii)
                    || (ascii.is_ascii_alphabetic()
                        && unit == u16::from(ascii.to_ascii_uppercase()))
            })
        })
        || matches!(units, [drive, colon, ..] if (*drive >= u16::from(b'a') && *drive <= u16::from(b'z') || *drive >= u16::from(b'A') && *drive <= u16::from(b'Z')) && *colon == u16::from(b':'))
}

const fn program_error(kind: ProgramVerifyErrorKind) -> ProgramVerifyError {
    ProgramVerifyError { module: None, kind }
}

const fn module_error(module: ModuleId, kind: ProgramVerifyErrorKind) -> ProgramVerifyError {
    ProgramVerifyError {
        module: Some(module),
        kind,
    }
}

fn write_u32(value: u32, output: &mut Vec<u8>) {
    let mut remaining = value;
    loop {
        let byte = (remaining & 0x7f) as u8;
        remaining >>= 7;
        if remaining == 0 {
            output.push(byte);
            return;
        }
        output.push(byte | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Function, FunctionFlags, FunctionId, Register};

    fn verified_module(name: &str, extra: &[&str]) -> Module<Verified> {
        let mut constants = vec![Constant::String(EcmaString::encode(name))];
        constants.extend(
            extra
                .iter()
                .map(|value| Constant::String(EcmaString::encode(value))),
        );
        Module::new(
            constants,
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
        .unwrap()
    }

    fn program_module(name: &str) -> ProgramModule<Verified> {
        ProgramModule {
            name: ConstantId::new(0),
            code: verified_module(name, &["x", "./dep", "remote"]),
            edges: Vec::new(),
            bindings: vec![Binding {
                name: ConstantId::new(1),
                kind: BindingKind::Hoisted,
            }],
            exports: vec![Export {
                name: ConstantId::new(1),
                source: ExportSource::Local(BindingId::new(0)),
            }],
        }
    }

    fn valid_program() -> Program<Verified> {
        Program::link(vec![program_module("main")], ModuleId::new(0)).unwrap()
    }

    fn read_u32(bytes: &[u8], offset: &mut usize) -> u32 {
        let mut value = 0;
        let mut shift = 0;
        loop {
            let byte = bytes[*offset];
            *offset += 1;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    fn raw_header(entry: u32, modules: u32) -> Vec<u8> {
        let mut bytes = PROGRAM_MAGIC.to_vec();
        bytes.push(PROGRAM_VERSION);
        write_u32(entry, &mut bytes);
        write_u32(modules, &mut bytes);
        bytes
    }

    fn raw_module_prefix(code: &Module<Verified>) -> Vec<u8> {
        let blob = code.encode();
        let mut bytes = raw_header(0, 1);
        write_u32(0, &mut bytes);
        write_u32(blob.len() as u32, &mut bytes);
        bytes.extend_from_slice(&blob);
        bytes
    }

    #[test]
    fn metadata_strings_must_be_well_formed_utf16() {
        let code = Module::new(
            vec![Constant::String(EcmaString::from_units(&[0xD800]))],
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
        .expect("a literal string may contain an unpaired surrogate");
        let error = Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code,
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .expect_err("module metadata cannot contain an unpaired surrogate");
        assert_eq!(
            error.kind,
            ProgramVerifyErrorKind::MetadataStringIllFormed {
                constant: ConstantId::new(0),
            }
        );
    }

    #[test]
    fn round_trip_reencode_is_identical() {
        let encoded = valid_program().encode();
        let decoded = decode_verified_program(&encoded, &ProgramDecodeLimits::default()).unwrap();
        assert_eq!(decoded.encode(), encoded);
        assert_eq!(
            decoded.resolve_export(ModuleId::new(0), &EcmaString::encode("x")),
            Some(ResolvedExport::Local {
                module: ModuleId::new(0),
                binding: BindingId::new(0),
            })
        );
    }

    #[test]
    fn every_truncation_and_trailing_bytes_are_rejected() {
        let encoded = valid_program().encode();
        for length in 0..encoded.len() {
            assert!(
                decode_program(&encoded[..length], &ProgramDecodeLimits::default()).is_err(),
                "accepted truncation at {length}"
            );
        }
        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            decode_program(&trailing, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::TrailingBytes { count: 1 },
                ..
            })
        ));
    }

    #[test]
    fn embedded_module_blob_is_byte_identical() {
        let program = valid_program();
        let blob = program.modules()[0].code.encode();
        let encoded = program.encode();
        let mut offset = PROGRAM_MAGIC.len() + 1;
        assert_eq!(read_u32(&encoded, &mut offset), 0);
        assert_eq!(read_u32(&encoded, &mut offset), 1);
        assert_eq!(read_u32(&encoded, &mut offset), 0);
        assert_eq!(read_u32(&encoded, &mut offset) as usize, blob.len());
        assert_eq!(&encoded[offset..offset + blob.len()], blob);
    }

    #[test]
    fn version_three_programs_are_rejected() {
        let mut encoded = PROGRAM_MAGIC.to_vec();
        encoded.push(3);
        assert!(matches!(
            decode_program(&encoded, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::UnsupportedVersion { version: 3 },
                ..
            })
        ));
    }

    #[test]
    fn canonical_edge_kind_tags_and_version_round_trip() {
        let mut module = program_module("main");
        module.edges = vec![
            Edge {
                specifier: ConstantId::new(1),
                target: EdgeTarget::External,
                kind: EdgeKind::Static,
            },
            Edge {
                specifier: ConstantId::new(2),
                target: EdgeTarget::External,
                kind: EdgeKind::Dynamic,
            },
            Edge {
                specifier: ConstantId::new(3),
                target: EdgeTarget::External,
                kind: EdgeKind::StaticAndDynamic,
            },
        ];
        let program = Program::link(vec![module], ModuleId::new(0)).unwrap();
        let encoded = program.encode();
        assert_eq!(encoded[PROGRAM_MAGIC.len()], PROGRAM_VERSION);

        let mut offset = PROGRAM_MAGIC.len() + 1;
        assert_eq!(read_u32(&encoded, &mut offset), 0);
        assert_eq!(read_u32(&encoded, &mut offset), 1);
        assert_eq!(read_u32(&encoded, &mut offset), 0);
        offset += read_u32(&encoded, &mut offset) as usize;
        assert_eq!(read_u32(&encoded, &mut offset), 3);
        for (specifier, kind) in [(1, 0), (2, 1), (3, 2)] {
            assert_eq!(read_u32(&encoded, &mut offset), specifier);
            assert_eq!(encoded[offset], 1);
            offset += 1;
            assert_eq!(encoded[offset], kind);
            offset += 1;
        }
        assert_eq!(
            decode_verified_program(&encoded, &ProgramDecodeLimits::default())
                .unwrap()
                .encode(),
            encoded
        );
    }

    #[test]
    fn oversized_counts_lengths_and_input_are_rejected() {
        let mut limits = ProgramDecodeLimits {
            max_modules: 0,
            ..ProgramDecodeLimits::default()
        };
        let modules = raw_header(0, 1);
        assert!(matches!(
            decode_program(&modules, &limits),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::LimitExceeded {
                    field: "modules",
                    ..
                },
                ..
            })
        ));

        let mut length = raw_header(0, 1);
        write_u32(0, &mut length);
        write_u32(2, &mut length);
        limits.max_modules = 1;
        limits.max_module_bytes = 1;
        assert!(matches!(
            decode_program(&length, &limits),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::LimitExceeded {
                    field: "module bytes",
                    ..
                },
                ..
            })
        ));

        limits.max_bytes = 1;
        assert!(matches!(
            decode_program(&valid_program().encode(), &limits),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::InputLimitExceeded { .. },
                ..
            })
        ));
    }

    #[test]
    fn every_metadata_count_and_total_limit_is_enforced() {
        let mut with_edge = program_module("main");
        with_edge.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::External,
            kind: EdgeKind::Static,
        });
        let edge_bytes = Program::link(vec![with_edge], ModuleId::new(0))
            .unwrap()
            .encode();
        for limits in [
            ProgramDecodeLimits {
                max_edges_per_module: 0,
                ..ProgramDecodeLimits::default()
            },
            ProgramDecodeLimits {
                max_total_edges: 0,
                ..ProgramDecodeLimits::default()
            },
        ] {
            assert!(matches!(
                decode_program(&edge_bytes, &limits),
                Err(ProgramDecodeError {
                    kind: ProgramDecodeErrorKind::LimitExceeded { .. },
                    ..
                })
            ));
        }

        let encoded = valid_program().encode();
        for limits in [
            ProgramDecodeLimits {
                max_bindings_per_module: 0,
                ..ProgramDecodeLimits::default()
            },
            ProgramDecodeLimits {
                max_total_bindings: 0,
                ..ProgramDecodeLimits::default()
            },
            ProgramDecodeLimits {
                max_exports_per_module: 0,
                ..ProgramDecodeLimits::default()
            },
            ProgramDecodeLimits {
                max_total_exports: 0,
                ..ProgramDecodeLimits::default()
            },
        ] {
            assert!(matches!(
                decode_program(&encoded, &limits),
                Err(ProgramDecodeError {
                    kind: ProgramDecodeErrorKind::LimitExceeded { .. },
                    ..
                })
            ));
        }
    }

    #[test]
    fn hostile_tiny_envelope_rejects_large_edge_count_before_allocation() {
        let code = verified_module("main", &["x"]);
        let mut bytes = raw_module_prefix(&code);
        write_u32(u32::MAX, &mut bytes);
        let limits = ProgramDecodeLimits {
            max_edges_per_module: u32::MAX,
            max_total_edges: u32::MAX,
            ..ProgramDecodeLimits::default()
        };
        assert!(matches!(
            decode_program(&bytes, &limits),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::UnexpectedEof,
                ..
            })
        ));
    }

    #[test]
    fn hostile_tiny_envelope_rejects_large_binding_count_before_allocation() {
        let code = verified_module("main", &["x"]);
        let mut bytes = raw_module_prefix(&code);
        write_u32(0, &mut bytes);
        write_u32(u32::MAX, &mut bytes);
        let limits = ProgramDecodeLimits {
            max_bindings_per_module: u32::MAX,
            max_total_bindings: u32::MAX,
            ..ProgramDecodeLimits::default()
        };
        assert!(matches!(
            decode_program(&bytes, &limits),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::UnexpectedEof,
                ..
            })
        ));
    }

    #[test]
    fn invalid_envelope_tags_and_integers_are_rejected() {
        let code = verified_module("main", &["x"]);

        let mut edge_tag = raw_module_prefix(&code);
        write_u32(1, &mut edge_tag);
        write_u32(1, &mut edge_tag);
        edge_tag.push(9);
        assert!(matches!(
            decode_program(&edge_tag, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::InvalidEdgeTarget { tag: 9 },
                ..
            })
        ));

        let mut edge_kind = raw_module_prefix(&code);
        write_u32(1, &mut edge_kind);
        write_u32(1, &mut edge_kind);
        edge_kind.extend_from_slice(&[1, 9]);
        assert!(matches!(
            decode_program(&edge_kind, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::InvalidEdgeKind { tag: 9 },
                ..
            })
        ));

        let mut binding_tag = raw_module_prefix(&code);
        write_u32(0, &mut binding_tag);
        write_u32(1, &mut binding_tag);
        binding_tag.extend_from_slice(&[1, 9]);
        assert!(matches!(
            decode_program(&binding_tag, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::InvalidBindingKind { tag: 9 },
                ..
            })
        ));

        let mut export_tag = raw_module_prefix(&code);
        write_u32(0, &mut export_tag);
        write_u32(0, &mut export_tag);
        write_u32(1, &mut export_tag);
        export_tag.extend_from_slice(&[1, 9]);
        assert!(matches!(
            decode_program(&export_tag, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::InvalidExportSource { tag: 9 },
                ..
            })
        ));

        let mut noncanonical = PROGRAM_MAGIC.to_vec();
        noncanonical.push(PROGRAM_VERSION);
        noncanonical.extend_from_slice(&[0x80, 0]);
        assert!(matches!(
            decode_program(&noncanonical, &ProgramDecodeLimits::default()),
            Err(ProgramDecodeError {
                kind: ProgramDecodeErrorKind::NonCanonicalInteger,
                ..
            })
        ));
    }

    #[test]
    fn malformed_utf16_metadata_inside_module_blob_is_typed_verify_error() {
        let mut encoded = valid_program().encode();
        let needle = [2, 4, b'm', 0, b'a', 0, b'i', 0, b'n', 0];
        let at = encoded
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap();
        encoded[at + 2..at + 4].copy_from_slice(&0xD800_u16.to_le_bytes());
        assert!(matches!(
            decode_verified_program(&encoded, &ProgramDecodeLimits::default()),
            Err(ProgramLoadError::Verify(ProgramVerifyError {
                kind: ProgramVerifyErrorKind::MetadataStringIllFormed { .. },
                ..
            }))
        ));
    }

    #[test]
    fn non_normalized_and_duplicate_module_names_are_rejected() {
        for name in ["", "/abs", "../escape", "a/../b", "a//b", "a\\b", "C:/abs"] {
            let error = Program::link(vec![program_module(name)], ModuleId::new(0)).unwrap_err();
            assert!(matches!(
                error.kind,
                ProgramVerifyErrorKind::InvalidModuleName
            ));
        }
        let error = Program::link(
            vec![program_module("same"), program_module("same")],
            ModuleId::new(0),
        )
        .unwrap_err();
        assert!(matches!(
            error.kind,
            ProgramVerifyErrorKind::DuplicateModuleName { .. }
        ));
    }

    #[test]
    fn absolute_path_specifiers_are_rejected_without_banning_external_schemes() {
        for specifier in [
            "/tmp/module",
            "\\\\server\\share",
            "C:/module",
            "file:///tmp/module",
        ] {
            let mut module = ProgramModule {
                name: ConstantId::new(0),
                code: verified_module("main", &[specifier]),
                edges: Vec::new(),
                bindings: Vec::new(),
                exports: Vec::new(),
            };
            module.edges.push(Edge {
                specifier: ConstantId::new(1),
                target: EdgeTarget::External,
                kind: EdgeKind::Static,
            });
            assert!(matches!(
                Program::link(vec![module], ModuleId::new(0))
                    .unwrap_err()
                    .kind,
                ProgramVerifyErrorKind::AbsoluteSpecifier { .. }
            ));
        }

        let mut module = ProgramModule {
            name: ConstantId::new(0),
            code: verified_module("main", &["node:fs"]),
            edges: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
        };
        module.edges.push(Edge {
            specifier: ConstantId::new(1),
            target: EdgeTarget::External,
            kind: EdgeKind::Static,
        });
        Program::link(vec![module], ModuleId::new(0)).unwrap();
    }

    #[test]
    fn duplicate_linkage_tables_are_rejected() {
        let mut module = program_module("main");
        module.edges = vec![
            Edge {
                specifier: ConstantId::new(2),
                target: EdgeTarget::External,
                kind: EdgeKind::Static,
            },
            Edge {
                specifier: ConstantId::new(2),
                target: EdgeTarget::External,
                kind: EdgeKind::Static,
            },
        ];
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::DuplicateSpecifier { .. }
        ));

        let mut module = program_module("main");
        module.bindings.push(module.bindings[0]);
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::DuplicateBinding { .. }
        ));

        let mut module = program_module("main");
        module.exports.push(module.exports[0]);
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::DuplicateExport { .. }
        ));
    }

    #[test]
    fn bad_local_edge_binding_and_export_indices_are_rejected() {
        let mut module = program_module("main");
        module.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(1)),
            kind: EdgeKind::Static,
        });
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::LocalTargetOutOfBounds { .. }
        ));

        let mut module = program_module("main");
        module.bindings[0].kind = BindingKind::Namespace {
            edge: EdgeId::new(0),
        };
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::BindingEdgeOutOfBounds { .. }
        ));

        let mut module = program_module("main");
        module.exports[0].source = ExportSource::Local(BindingId::new(1));
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::ExportBindingOutOfBounds { .. }
        ));

        let mut module = program_module("main");
        module.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::ExportEdgeOutOfBounds { .. }
        ));
    }

    #[test]
    fn every_metadata_string_reference_checks_bounds_and_kind() {
        let code = Module::new(
            vec![
                Constant::String(EcmaString::encode("main")),
                Constant::Int32(7),
            ],
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
        .unwrap();
        let empty = |name| ProgramModule {
            name,
            code: code.clone(),
            edges: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
        };

        for (name, expected_bounds) in [(ConstantId::new(2), true), (ConstantId::new(1), false)] {
            let kind = Program::link(vec![empty(name)], ModuleId::new(0))
                .unwrap_err()
                .kind;
            assert!(
                matches!(kind, ProgramVerifyErrorKind::ModuleNameOutOfBounds { .. })
                    == expected_bounds
            );
            assert!(
                matches!(kind, ProgramVerifyErrorKind::ModuleNameNotString { .. })
                    != expected_bounds
            );
        }

        for (specifier, expected_bounds) in
            [(ConstantId::new(2), true), (ConstantId::new(1), false)]
        {
            let mut module = empty(ConstantId::new(0));
            module.edges.push(Edge {
                specifier,
                target: EdgeTarget::External,
                kind: EdgeKind::Static,
            });
            let kind = Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind;
            assert!(
                matches!(kind, ProgramVerifyErrorKind::SpecifierOutOfBounds { .. })
                    == expected_bounds
            );
            assert!(
                matches!(kind, ProgramVerifyErrorKind::SpecifierNotString { .. })
                    != expected_bounds
            );
        }

        for (name, expected_bounds) in [(ConstantId::new(2), true), (ConstantId::new(1), false)] {
            let mut module = empty(ConstantId::new(0));
            module.bindings.push(Binding {
                name,
                kind: BindingKind::Lexical,
            });
            let kind = Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind;
            assert!(
                matches!(kind, ProgramVerifyErrorKind::BindingNameOutOfBounds { .. })
                    == expected_bounds
            );
            assert!(
                matches!(kind, ProgramVerifyErrorKind::BindingNameNotString { .. })
                    != expected_bounds
            );
        }

        for (name, expected_bounds) in [(ConstantId::new(2), true), (ConstantId::new(1), false)] {
            let mut module = empty(ConstantId::new(0));
            module.exports.push(Export {
                name,
                source: ExportSource::Local(BindingId::new(0)),
            });
            let kind = Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind;
            assert!(
                matches!(kind, ProgramVerifyErrorKind::ExportNameOutOfBounds { .. })
                    == expected_bounds
            );
            assert!(
                matches!(kind, ProgramVerifyErrorKind::ExportNameNotString { .. })
                    != expected_bounds
            );
        }

        let mut module = empty(ConstantId::new(0));
        module.edges.push(Edge {
            specifier: ConstantId::new(0),
            target: EdgeTarget::External,
            kind: EdgeKind::Static,
        });
        module.bindings.push(Binding {
            name: ConstantId::new(0),
            kind: BindingKind::Imported {
                edge: EdgeId::new(0),
                name: ConstantId::new(1),
            },
        });
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::ImportedNameNotString { .. }
        ));

        let mut module = empty(ConstantId::new(0));
        module.edges.push(Edge {
            specifier: ConstantId::new(0),
            target: EdgeTarget::External,
            kind: EdgeKind::Static,
        });
        module.exports.push(Export {
            name: ConstantId::new(0),
            source: ExportSource::Indirect {
                edge: EdgeId::new(0),
                name: ConstantId::new(1),
            },
        });
        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::IndirectNameNotString { .. }
        ));
    }

    #[test]
    fn indirect_exports_require_static_edges() {
        let mut module = program_module("main");
        module.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::External,
            kind: EdgeKind::Dynamic,
        });
        module.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };

        assert!(matches!(
            Program::link(vec![module], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::IndirectExportRequiresStaticEdge { .. }
        ));
    }

    #[test]
    fn export_cycles_and_missing_targets_are_rejected() {
        let mut left = program_module("left");
        let mut right = program_module("right");
        left.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(1)),
            kind: EdgeKind::Static,
        });
        right.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(0)),
            kind: EdgeKind::Static,
        });
        left.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };
        right.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };
        assert!(matches!(
            Program::link(vec![left, right], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::IndirectExportCycle { .. }
        ));

        let mut left = program_module("left");
        let right = program_module("right");
        left.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(1)),
            kind: EdgeKind::Static,
        });
        left.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(3),
        };
        assert!(matches!(
            Program::link(vec![left, right], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::MissingIndirectExport { .. }
        ));
    }

    #[test]
    fn external_indirect_exports_resolve_totally() {
        let mut module = program_module("main");
        module.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::External,
            kind: EdgeKind::Static,
        });
        module.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(3),
        };
        let program = Program::link(vec![module], ModuleId::new(0)).unwrap();
        assert_eq!(
            program.resolve_export(ModuleId::new(0), &EcmaString::encode("x")),
            Some(ResolvedExport::External {
                module: ModuleId::new(0),
                edge: EdgeId::new(0),
                name: ConstantId::new(3),
            })
        );
    }

    #[test]
    fn entry_bounds_are_checked_after_decode_or_link() {
        let error = Program::link(vec![program_module("main")], ModuleId::new(1)).unwrap_err();
        assert!(matches!(
            error.kind,
            ProgramVerifyErrorKind::EntryModuleOutOfBounds { .. }
        ));

        let encoded = Program {
            modules: valid_program().modules,
            entry: ModuleId::new(1),
            state: PhantomData,
        }
        .encode();
        assert!(matches!(
            decode_verified_program(&encoded, &ProgramDecodeLimits::default()),
            Err(ProgramLoadError::Verify(ProgramVerifyError {
                kind: ProgramVerifyErrorKind::EntryModuleOutOfBounds { .. },
                ..
            }))
        ));
    }

    #[test]
    fn dynamic_imports_require_dynamic_capability() {
        let code = Module::new(
            vec![
                Constant::String(EcmaString::encode("main")),
                Constant::String(EcmaString::encode("./dep")),
            ],
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![
                    Instruction::Import {
                        dst: Register::new(0),
                        specifier: ConstantId::new(1),
                    },
                    Instruction::Halt,
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .unwrap();
        let dynamic_module = |kind| ProgramModule {
            name: ConstantId::new(0),
            code: code.clone(),
            edges: vec![Edge {
                specifier: ConstantId::new(1),
                target: EdgeTarget::External,
                kind,
            }],
            bindings: Vec::new(),
            exports: Vec::new(),
        };

        Program::link(vec![dynamic_module(EdgeKind::Dynamic)], ModuleId::new(0)).unwrap();
        Program::link(
            vec![dynamic_module(EdgeKind::StaticAndDynamic)],
            ModuleId::new(0),
        )
        .unwrap();
        assert!(matches!(
            Program::link(vec![dynamic_module(EdgeKind::Static)], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::DynamicImportMissingEdge { .. }
        ));
    }

    #[test]
    fn runtime_dynamic_import_needs_no_linkage_edge() {
        let code = Module::new(
            vec![Constant::String(EcmaString::encode("main"))],
            vec![Function::new(
                None,
                0,
                0,
                2,
                FunctionFlags::default(),
                vec![
                    Instruction::LoadConst {
                        dst: Register::new(0),
                        constant: ConstantId::new(0),
                    },
                    Instruction::ImportDynamic {
                        dst: Register::new(1),
                        specifier: Register::new(0),
                    },
                    Instruction::Halt,
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .expect("runtime dynamic-import instruction verifies");
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
        .expect("runtime dynamic import has no static-edge requirement");
    }

    #[test]
    fn static_bindings_require_static_capability() {
        for kind in [
            BindingKind::Imported {
                edge: EdgeId::new(0),
                name: ConstantId::new(1),
            },
            BindingKind::Namespace {
                edge: EdgeId::new(0),
            },
        ] {
            let mut module = program_module("main");
            module.edges.push(Edge {
                specifier: ConstantId::new(2),
                target: EdgeTarget::External,
                kind: EdgeKind::Dynamic,
            });
            module.bindings[0].kind = kind;
            assert!(matches!(
                Program::link(vec![module], ModuleId::new(0))
                    .unwrap_err()
                    .kind,
                ProgramVerifyErrorKind::StaticBindingRequiresStaticEdge { .. }
            ));
        }
    }

    #[test]
    fn static_and_dynamic_edge_satisfies_both_linkage_capabilities() {
        let code = Module::new(
            vec![
                Constant::String(EcmaString::encode("main")),
                Constant::String(EcmaString::encode("./dep")),
                Constant::String(EcmaString::encode("value")),
            ],
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![
                    Instruction::Import {
                        dst: Register::new(0),
                        specifier: ConstantId::new(1),
                    },
                    Instruction::Halt,
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .unwrap();
        Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code,
                edges: vec![Edge {
                    specifier: ConstantId::new(1),
                    target: EdgeTarget::External,
                    kind: EdgeKind::StaticAndDynamic,
                }],
                bindings: vec![Binding {
                    name: ConstantId::new(2),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: ConstantId::new(2),
                    },
                }],
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .unwrap();
    }

    #[test]
    fn executable_program_rejects_snapshot_exports() {
        let module = Module::new(
            vec![Constant::String(EcmaString::encode("main"))],
            vec![Function::new(
                None,
                0,
                0,
                1,
                FunctionFlags::default(),
                vec![
                    Instruction::LoadConst {
                        dst: Register::new(0),
                        constant: ConstantId::new(0),
                    },
                    Instruction::Export {
                        name: ConstantId::new(0),
                        src: Register::new(0),
                    },
                    Instruction::Halt,
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .unwrap();
        assert!(matches!(
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
            .unwrap_err()
            .kind,
            ProgramVerifyErrorKind::SnapshotExportInstruction { .. }
        ));
    }

    #[test]
    fn post_import_mutation_keeps_external_binding_as_live_identity() {
        let code = Module::new(
            vec![
                Constant::String(EcmaString::encode("main")),
                Constant::String(EcmaString::encode("x")),
                Constant::String(EcmaString::encode("builtin:live")),
            ],
            vec![Function::new(
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
                    Instruction::Halt,
                ],
                Vec::new(),
            )],
            FunctionId::new(0),
        )
        .verify()
        .unwrap();
        let program = Program::link(
            vec![ProgramModule {
                name: ConstantId::new(0),
                code,
                edges: vec![Edge {
                    specifier: ConstantId::new(2),
                    target: EdgeTarget::External,
                    kind: EdgeKind::Static,
                }],
                bindings: vec![Binding {
                    name: ConstantId::new(1),
                    kind: BindingKind::Imported {
                        edge: EdgeId::new(0),
                        name: ConstantId::new(1),
                    },
                }],
                exports: Vec::new(),
            }],
            ModuleId::new(0),
        )
        .unwrap();
        assert!(matches!(
            program.modules()[0].bindings()[0].kind,
            BindingKind::Imported { .. }
        ));
    }

    #[test]
    fn external_imported_reexports_remain_available_to_providers() {
        let mut module = program_module("main");
        module.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::External,
            kind: EdgeKind::Static,
        });
        module.bindings[0].kind = BindingKind::Imported {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };
        let program = Program::link(vec![module], ModuleId::new(0)).unwrap();
        assert_eq!(
            program.resolve_export(ModuleId::new(0), &EcmaString::encode("x")),
            Some(ResolvedExport::External {
                module: ModuleId::new(0),
                edge: EdgeId::new(0),
                name: ConstantId::new(1),
            })
        );
    }

    #[test]
    fn imported_export_cycles_are_rejected() {
        let mut left = program_module("left");
        left.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(1)),
            kind: EdgeKind::Static,
        });
        left.bindings[0].kind = BindingKind::Imported {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };

        let mut right = program_module("right");
        right.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(0)),
            kind: EdgeKind::Static,
        });
        right.bindings[0].kind = BindingKind::Imported {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };

        assert!(matches!(
            Program::link(vec![left, right], ModuleId::new(0))
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::IndirectExportCycle { .. }
        ));
    }

    #[test]
    fn local_imports_require_exports_and_follow_indirect_exports() {
        let mut importer = program_module("importer");
        importer.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(1)),
            kind: EdgeKind::Static,
        });
        importer.bindings[0].kind = BindingKind::Imported {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };

        let mut relay = program_module("relay");
        relay.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(2)),
            kind: EdgeKind::Static,
        });
        relay.exports[0].source = ExportSource::Indirect {
            edge: EdgeId::new(0),
            name: ConstantId::new(1),
        };
        let program = Program::link(
            vec![importer, relay, program_module("leaf")],
            ModuleId::new(0),
        )
        .unwrap();
        let leaf = Some(ResolvedExport::Local {
            module: ModuleId::new(2),
            binding: BindingId::new(0),
        });
        assert_eq!(
            program.resolve_export(ModuleId::new(0), &EcmaString::encode("x")),
            leaf
        );
        assert_eq!(
            program.resolve_export(ModuleId::new(1), &EcmaString::encode("x")),
            leaf
        );

        let mut importer = program_module("importer");
        importer.edges.push(Edge {
            specifier: ConstantId::new(2),
            target: EdgeTarget::Local(ModuleId::new(1)),
            kind: EdgeKind::Static,
        });
        importer.bindings[0].kind = BindingKind::Imported {
            edge: EdgeId::new(0),
            name: ConstantId::new(3),
        };
        importer.exports.clear();
        assert!(matches!(
            Program::link(vec![importer, program_module("target")], ModuleId::new(0),)
                .unwrap_err()
                .kind,
            ProgramVerifyErrorKind::MissingImportedExport { .. }
        ));
    }

    #[test]
    fn verify_error_display_is_human_readable_not_debug() {
        // Pin the Display output: it must read as a sentence, never as
        // Rust struct syntax like `DuplicateSpecifier { first: EdgeId(1), second: EdgeId(4) }`.
        let error = ProgramVerifyError {
            module: Some(ModuleId::new(3)),
            kind: ProgramVerifyErrorKind::DuplicateSpecifier {
                first: EdgeId::new(1),
                second: EdgeId::new(4),
            },
        };
        assert_eq!(
            error.to_string(),
            "module 3: edges 1 and 4 share one specifier"
        );

        // No module prefix variant.
        let no_module = ProgramVerifyError {
            module: None,
            kind: ProgramVerifyErrorKind::EmptyProgram,
        };
        assert_eq!(no_module.to_string(), "program has no modules");

        // Verify the output never contains debug-style braces or field names.
        assert!(
            !error.to_string().contains('{'),
            "Display must not emit Debug struct syntax"
        );
    }
}
