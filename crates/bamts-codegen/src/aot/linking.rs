use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use cranelift_object::object::{Object, ObjectSegment};
use sha2::{Digest, Sha256};

use super::{AotObject, TargetDescriptor};

/// Object format selected from the requested target, never from the build host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetFormat {
    Elf,
    MachO,
    Coff,
}

impl TargetFormat {
    fn for_triple(triple: &str) -> Result<Self, LinkError> {
        if triple.contains("linux") {
            Ok(Self::Elf)
        } else if triple.contains("darwin") || triple.contains("apple") {
            Ok(Self::MachO)
        } else if triple.contains("windows") {
            Ok(Self::Coff)
        } else {
            Err(LinkError::UnsupportedFormat(triple.to_owned()))
        }
    }

    const fn tag(self) -> &'static [u8] {
        match self {
            Self::Elf => b"elf",
            Self::MachO => b"mach-o",
            Self::Coff => b"coff",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum LinkInputRole {
    Object,
    StaticLibrary,
}

impl LinkInputRole {
    const fn tag(self) -> u8 {
        match self {
            Self::Object => 0,
            Self::StaticLibrary => 1,
        }
    }
}

/// One exact linker input. Paths remain separate argv entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkInput {
    path: PathBuf,
    role: LinkInputRole,
    content_digest: [u8; 32],
}

impl LinkInput {
    #[must_use]
    pub fn object(path: impl Into<PathBuf>, content_digest: [u8; 32]) -> Self {
        Self {
            path: path.into(),
            role: LinkInputRole::Object,
            content_digest,
        }
    }

    #[must_use]
    pub fn static_library(path: impl Into<PathBuf>, content_digest: [u8; 32]) -> Self {
        Self {
            path: path.into(),
            role: LinkInputRole::StaticLibrary,
            content_digest,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn role(&self) -> LinkInputRole {
        self.role
    }

    #[must_use]
    pub const fn content_digest(&self) -> [u8; 32] {
        self.content_digest
    }
}

/// Link hardening and runtime-library policy, represented without host probing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkFlags {
    pub relro: bool,
    pub now: bool,
    pub noexecstack: bool,
    pub static_link: bool,
}

impl LinkFlags {
    /// Hardened executable defaults compatible with the embedded Node archive.
    pub const HOST_EXECUTABLE: Self = Self {
        relro: true,
        now: true,
        noexecstack: true,
        static_link: false,
    };

    fn append_args(self, format: TargetFormat, args: &mut Vec<OsString>) {
        match format {
            TargetFormat::Elf => {
                if self.relro {
                    args.push("-Wl,-z,relro".into());
                }
                if self.now {
                    args.push("-Wl,-z,now".into());
                }
                if self.noexecstack {
                    args.push("-Wl,-z,noexecstack".into());
                }
                if self.static_link {
                    args.push("-static".into());
                }
                args.extend(["-ldl".into(), "-lpthread".into(), "-lm".into()]);
            }
            TargetFormat::MachO | TargetFormat::Coff => {
                if self.static_link {
                    args.push("-static".into());
                }
            }
        }
    }

    fn bind(self, hasher: &mut Sha256) {
        hasher.update([
            u8::from(self.relro),
            u8::from(self.now),
            u8::from(self.noexecstack),
            u8::from(self.static_link),
        ]);
    }
}

/// A pure linker invocation description. Codegen never executes this plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkPlan {
    target: TargetDescriptor,
    format: TargetFormat,
    inputs: Vec<LinkInput>,
    output: PathBuf,
    flags: LinkFlags,
    environment: Vec<(OsString, OsString)>,
    cache_key: LinkCacheKey,
}

impl LinkPlan {
    /// Produces argv in a fixed order while preserving every path as one argument.
    #[must_use]
    pub fn argv(&self) -> Vec<OsString> {
        let mut args = self
            .inputs
            .iter()
            .map(|input| input.path.as_os_str().to_owned())
            .collect::<Vec<_>>();
        args.push("-o".into());
        args.push(self.output.as_os_str().to_owned());
        self.flags.append_args(self.format, &mut args);
        args
    }

    #[must_use]
    pub fn target(&self) -> &TargetDescriptor {
        &self.target
    }

    #[must_use]
    pub const fn format(&self) -> TargetFormat {
        self.format
    }

    #[must_use]
    pub fn inputs(&self) -> &[LinkInput] {
        &self.inputs
    }

    #[must_use]
    pub fn output(&self) -> &Path {
        &self.output
    }

    #[must_use]
    pub const fn flags(&self) -> LinkFlags {
        self.flags
    }

    #[must_use]
    pub fn environment(&self) -> &[(OsString, OsString)] {
        &self.environment
    }

    #[must_use]
    pub const fn cache_key(&self) -> LinkCacheKey {
        self.cache_key
    }
}

/// Content identity for every output-affecting link-plan input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkCacheKey([u8; 32]);

impl LinkCacheKey {
    #[must_use]
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }

    #[must_use]
    pub fn hex(self) -> String {
        let mut output = String::with_capacity(64);
        use fmt::Write as _;
        for byte in self.0 {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
        }
        output
    }

    pub fn verify(self, stored: Self) -> Result<(), LinkError> {
        if self == stored {
            Ok(())
        } else {
            Err(LinkError::CacheKeyMismatch)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkProvenance {
    pub executable_digest: [u8; 32],
    pub plan: LinkPlan,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LinkError {
    DuplicateInput(PathBuf),
    UnsupportedFormat(String),
    DuplicateSymbols(Vec<String>),
    UnresolvedSymbols(Vec<String>),
    ImageParse(String),
    WritableExecutableSegment { index: usize },
    ExecutableStack { index: usize },
    MissingStackPolicy,
    DuplicateStackPolicy { count: usize },
    CacheKeyMismatch,
}

impl fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateInput(path) => {
                write!(formatter, "duplicate linker input `{}`", path.display())
            }
            Self::UnsupportedFormat(target) => {
                write!(formatter, "unsupported linker format for target `{target}`")
            }
            Self::DuplicateSymbols(symbols) => {
                write!(
                    formatter,
                    "duplicate runtime symbols: {}",
                    symbols.join(", ")
                )
            }
            Self::UnresolvedSymbols(symbols) => {
                write!(
                    formatter,
                    "unresolved runtime symbols: {}",
                    symbols.join(", ")
                )
            }
            Self::ImageParse(error) => write!(formatter, "invalid linked image: {error}"),
            Self::WritableExecutableSegment { index } => {
                write!(
                    formatter,
                    "linked image segment {index} is writable and executable"
                )
            }
            Self::ExecutableStack { index } => write!(
                formatter,
                "ELF image has an executable PT_GNU_STACK program header at index {index}"
            ),
            Self::MissingStackPolicy => formatter.write_str("ELF image is missing PT_GNU_STACK"),
            Self::DuplicateStackPolicy { count } => write!(
                formatter,
                "ELF image has {count} PT_GNU_STACK program headers"
            ),
            Self::CacheKeyMismatch => formatter.write_str("link cache key mismatch"),
        }
    }
}

impl Error for LinkError {}

/// Rejects linked images with W+X load segments or an unsafe ELF stack policy.
pub fn validate_linked_image(bytes: &[u8]) -> Result<(), LinkError> {
    let image = cranelift_object::object::File::parse(bytes)
        .map_err(|error| LinkError::ImageParse(error.to_string()))?;
    if let Some(endianness) = elf64_endianness(bytes) {
        return validate_elf64_image_policy(bytes, endianness);
    }
    for (index, segment) in image.segments().enumerate() {
        let permissions = segment.permissions();
        if permissions.writable() && permissions.executable() {
            return Err(LinkError::WritableExecutableSegment { index });
        }
    }
    Ok(())
}

const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ELFDATA2MSB: u8 = 2;
const PT_LOAD: u32 = 1;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PN_XNUM: u16 = 0xffff;
const ELF64_SECTION_INFO_OFFSET: usize = 44;
const ELF64_SECTION_INFO_END: usize = 48;

#[derive(Clone, Copy)]
enum ElfEndianness {
    Little,
    Big,
}

fn elf64_endianness(bytes: &[u8]) -> Option<ElfEndianness> {
    if bytes.len() < 6 || !bytes.starts_with(ELF_MAGIC) || bytes[4] != ELFCLASS64 {
        return None;
    }
    match bytes[5] {
        ELFDATA2LSB => Some(ElfEndianness::Little),
        ELFDATA2MSB => Some(ElfEndianness::Big),
        _ => None,
    }
}

fn read_u16(bytes: &[u8], offset: usize, endianness: ElfEndianness) -> Result<u16, LinkError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| LinkError::ImageParse("ELF header is truncated".to_owned()))?;
    Ok(match endianness {
        ElfEndianness::Little => u16::from_le_bytes([slice[0], slice[1]]),
        ElfEndianness::Big => u16::from_be_bytes([slice[0], slice[1]]),
    })
}

fn read_u32(bytes: &[u8], offset: usize, endianness: ElfEndianness) -> Result<u32, LinkError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| LinkError::ImageParse("ELF program header is truncated".to_owned()))?;
    Ok(match endianness {
        ElfEndianness::Little => u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]),
        ElfEndianness::Big => u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]),
    })
}

fn read_u64(bytes: &[u8], offset: usize, endianness: ElfEndianness) -> Result<u64, LinkError> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| LinkError::ImageParse("ELF header is truncated".to_owned()))?;
    Ok(match endianness {
        ElfEndianness::Little => u64::from_le_bytes(slice.try_into().expect("8 bytes")),
        ElfEndianness::Big => u64::from_be_bytes(slice.try_into().expect("8 bytes")),
    })
}

fn elf64_program_header_count(
    bytes: &[u8],
    encoded_count: u16,
    endianness: ElfEndianness,
) -> Result<usize, LinkError> {
    if encoded_count != PN_XNUM {
        return Ok(encoded_count as usize);
    }

    let shoff = usize::try_from(read_u64(bytes, 40, endianness)?).map_err(|_| {
        LinkError::ImageParse("ELF section header table offset does not fit usize".to_owned())
    })?;
    if shoff == 0 {
        return Err(LinkError::ImageParse(
            "ELF extended program header count requires section header zero".to_owned(),
        ));
    }
    let shentsize = read_u16(bytes, 58, endianness)? as usize;
    if shentsize < ELF64_SECTION_INFO_END {
        return Err(LinkError::ImageParse(
            "ELF section header entry size is too small for extended numbering".to_owned(),
        ));
    }
    let section_zero_end = shoff.checked_add(shentsize).ok_or_else(|| {
        LinkError::ImageParse("ELF section header zero bounds overflow".to_owned())
    })?;
    let section_zero = bytes.get(shoff..section_zero_end).ok_or_else(|| {
        LinkError::ImageParse("ELF section header zero is out of bounds".to_owned())
    })?;
    let count = read_u32(section_zero, ELF64_SECTION_INFO_OFFSET, endianness)?;
    usize::try_from(count).map_err(|_| {
        LinkError::ImageParse("ELF extended program header count does not fit usize".to_owned())
    })
}

/// Walks bounded ELF64 program headers: reject W+X PT_LOAD; require one non-exec PT_GNU_STACK.
fn validate_elf64_image_policy(bytes: &[u8], endianness: ElfEndianness) -> Result<(), LinkError> {
    let phoff = usize::try_from(read_u64(bytes, 32, endianness)?).map_err(|_| {
        LinkError::ImageParse("ELF program header table offset does not fit usize".to_owned())
    })?;
    let phentsize = read_u16(bytes, 54, endianness)? as usize;
    let encoded_phnum = read_u16(bytes, 56, endianness)?;
    let phnum = elf64_program_header_count(bytes, encoded_phnum, endianness)?;
    if phentsize < 8 {
        return Err(LinkError::ImageParse(
            "ELF program header entry size is too small".to_owned(),
        ));
    }
    let table_len = phentsize.checked_mul(phnum).ok_or_else(|| {
        LinkError::ImageParse("ELF program header table length overflow".to_owned())
    })?;
    let table_end = phoff.checked_add(table_len).ok_or_else(|| {
        LinkError::ImageParse("ELF program header table bounds overflow".to_owned())
    })?;
    if table_end > bytes.len() {
        return Err(LinkError::ImageParse(
            "ELF program header table is out of bounds".to_owned(),
        ));
    }

    let mut stack_count = 0_usize;
    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        let p_type = read_u32(bytes, offset, endianness)?;
        let p_flags = read_u32(bytes, offset + 4, endianness)?;
        if p_type == PT_LOAD && (p_flags & PF_W) != 0 && (p_flags & PF_X) != 0 {
            return Err(LinkError::WritableExecutableSegment { index });
        }
        if p_type == PT_GNU_STACK {
            stack_count += 1;
            if (p_flags & PF_X) != 0 {
                return Err(LinkError::ExecutableStack { index });
            }
        }
    }
    match stack_count {
        0 => Err(LinkError::MissingStackPolicy),
        1 => Ok(()),
        count => Err(LinkError::DuplicateStackPolicy { count }),
    }
}

/// Builds a deterministic plan from exact inputs. Input order is semantic and retained.
pub fn plan_link(
    descriptor: &TargetDescriptor,
    object_digest: [u8; 32],
    toolchain_identity: &[u8],
    inputs: Vec<LinkInput>,
    output: PathBuf,
    flags: LinkFlags,
    environment: Vec<(OsString, OsString)>,
) -> Result<LinkPlan, LinkError> {
    let format = TargetFormat::for_triple(descriptor.triple())?;
    let mut paths = BTreeSet::new();
    for input in &inputs {
        if !paths.insert(input.path.clone()) {
            return Err(LinkError::DuplicateInput(input.path.clone()));
        }
    }

    let environment = environment
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mut hasher = Sha256::new();
    hasher.update(b"bamts-link-plan/v1\0");
    hasher.update(descriptor.fingerprint());
    hasher.update(format.tag());
    hasher.update(object_digest);
    bind_field(&mut hasher, toolchain_identity);
    bind_field(&mut hasher, output.as_os_str().as_encoded_bytes());
    flags.bind(&mut hasher);

    for input in &inputs {
        hasher.update([input.role.tag()]);
        bind_field(&mut hasher, input.path.as_os_str().as_encoded_bytes());
        hasher.update(input.content_digest);
    }
    for (name, value) in &environment {
        bind_field(&mut hasher, name.as_encoded_bytes());
        bind_field(&mut hasher, value.as_encoded_bytes());
    }
    let cache_key = LinkCacheKey(hasher.finalize().into());

    Ok(LinkPlan {
        target: descriptor.clone(),
        format,
        inputs,
        output,
        flags,
        environment,
        cache_key,
    })
}

/// Requires every runtime helper exactly once across the supplied archives.
pub fn resolve_symbols(
    object: &AotObject,
    archives: &[(&str, &[String])],
) -> Result<(), LinkError> {
    let required = object
        .required_helpers
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut providers = BTreeMap::<&str, Vec<&str>>::new();
    for &(archive, symbols) in archives {
        for symbol in symbols {
            if required.contains(symbol.as_str()) {
                providers.entry(symbol).or_default().push(archive);
            }
        }
    }

    let unresolved = required
        .iter()
        .filter(|symbol| !providers.contains_key(**symbol))
        .map(|symbol| (*symbol).to_owned())
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        return Err(LinkError::UnresolvedSymbols(unresolved));
    }
    let duplicates = providers
        .into_iter()
        .filter(|(_, archives)| archives.len() > 1)
        .map(|(symbol, _)| symbol.to_owned())
        .collect::<Vec<_>>();
    if !duplicates.is_empty() {
        return Err(LinkError::DuplicateSymbols(duplicates));
    }
    Ok(())
}

fn bind_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(triple: &str) -> TargetDescriptor {
        TargetDescriptor::lookup(triple).expect("registered target resolves")
    }

    fn plan(triple: &str, output: &str) -> LinkPlan {
        plan_link(
            &descriptor(triple),
            [7; 32],
            b"cc 1.0",
            vec![
                LinkInput::object("directory with spaces/program.o", [1; 32]),
                LinkInput::static_library("runtime/libbamts node.a", [2; 32]),
            ],
            output.into(),
            LinkFlags::HOST_EXECUTABLE,
            vec![("LANG".into(), "C".into()), ("LC_ALL".into(), "C".into())],
        )
        .expect("link plan builds")
    }

    #[test]
    fn elf_plan_has_stable_hardened_argument_order() {
        let plan = plan("x86_64-unknown-linux-gnu", "output with spaces/program");
        assert_eq!(plan.format(), TargetFormat::Elf);
        assert_eq!(
            plan.argv(),
            vec![
                OsString::from("directory with spaces/program.o"),
                OsString::from("runtime/libbamts node.a"),
                OsString::from("-o"),
                OsString::from("output with spaces/program"),
                OsString::from("-Wl,-z,relro"),
                OsString::from("-Wl,-z,now"),
                OsString::from("-Wl,-z,noexecstack"),
                OsString::from("-ldl"),
                OsString::from("-lpthread"),
                OsString::from("-lm"),
            ]
        );
    }

    #[test]
    fn output_path_is_bound_into_cache_key() {
        assert_ne!(
            plan("x86_64-unknown-linux-gnu", "first").cache_key(),
            plan("x86_64-unknown-linux-gnu", "second").cache_key()
        );
    }

    #[test]
    fn cache_key_binds_target_object_toolchain_flags_and_inputs() {
        let base = plan("x86_64-unknown-linux-gnu", "output");
        let changed_target = plan("aarch64-unknown-linux-gnu", "output");
        assert_ne!(base.cache_key(), changed_target.cache_key());

        let changed_object = plan_link(
            base.target(),
            [8; 32],
            b"cc 1.0",
            base.inputs().to_vec(),
            base.output().to_owned(),
            base.flags(),
            base.environment().to_vec(),
        )
        .unwrap();
        assert_ne!(base.cache_key(), changed_object.cache_key());

        let changed_toolchain = plan_link(
            base.target(),
            [7; 32],
            b"cc 2.0",
            base.inputs().to_vec(),
            base.output().to_owned(),
            base.flags(),
            base.environment().to_vec(),
        )
        .unwrap();
        assert_ne!(base.cache_key(), changed_toolchain.cache_key());

        let mut changed_archive_inputs = base.inputs().to_vec();
        changed_archive_inputs[1] = LinkInput::static_library("runtime/libbamts node.a", [9; 32]);
        let changed_archive = plan_link(
            base.target(),
            [7; 32],
            b"cc 1.0",
            changed_archive_inputs,
            base.output().to_owned(),
            base.flags(),
            base.environment().to_vec(),
        )
        .unwrap();
        assert_ne!(base.cache_key(), changed_archive.cache_key());
    }

    #[test]
    fn duplicate_input_is_typed_error() {
        let duplicate = LinkInput::object("same.o", [1; 32]);
        assert_eq!(
            plan_link(
                &descriptor("x86_64-unknown-linux-gnu"),
                [0; 32],
                b"cc",
                vec![duplicate.clone(), duplicate],
                "output".into(),
                LinkFlags::HOST_EXECUTABLE,
                Vec::new(),
            ),
            Err(LinkError::DuplicateInput("same.o".into()))
        );
    }

    #[test]
    fn unsupported_format_is_typed_error() {
        assert_eq!(
            TargetFormat::for_triple("x86_64-unknown-none"),
            Err(LinkError::UnsupportedFormat(
                "x86_64-unknown-none".to_owned()
            ))
        );
    }

    #[test]
    fn cache_key_verification_fails_closed() {
        let first = plan("x86_64-unknown-linux-gnu", "first").cache_key();
        let second = plan("aarch64-unknown-linux-gnu", "second").cache_key();
        assert_eq!(first.verify(second), Err(LinkError::CacheKeyMismatch));
    }

    fn object(required_helpers: Vec<&'static str>) -> AotObject {
        AotObject {
            bytes: vec![1, 2, 3],
            target: "x86_64-unknown-linux-gnu".to_owned(),
            descriptor_symbol: "bamts_program_descriptor",
            entry_module: 0,
            entry_function: 0,
            entry_symbol: "bamts_module_0_function_0".to_owned(),
            required_helpers,
        }
    }

    #[test]
    fn environment_is_canonicalized_without_losing_argument_boundaries() {
        let planned = plan_link(
            &descriptor("x86_64-unknown-linux-gnu"),
            [7; 32],
            b"cc 1.0",
            vec![LinkInput::object("program.o", [1; 32])],
            "output".into(),
            LinkFlags::HOST_EXECUTABLE,
            vec![
                ("Z".into(), "last".into()),
                ("A".into(), "shadowed".into()),
                ("A".into(), "first".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            planned.environment(),
            &[
                (OsString::from("A"), OsString::from("first")),
                (OsString::from("Z"), OsString::from("last")),
            ]
        );
    }

    #[test]
    fn input_order_is_semantic_and_bound_into_cache_key() {
        let base = plan("x86_64-unknown-linux-gnu", "output");
        let mut reversed = base.inputs().to_vec();
        reversed.reverse();
        let reversed = plan_link(
            base.target(),
            [7; 32],
            b"cc 1.0",
            reversed,
            base.output().to_owned(),
            base.flags(),
            base.environment().to_vec(),
        )
        .unwrap();
        assert_ne!(base.argv(), reversed.argv());
        assert_ne!(base.cache_key(), reversed.cache_key());
    }

    #[test]
    fn helper_resolution_accepts_exactly_one_provider() {
        let symbols = vec!["bamts_helper_get_super".to_owned()];
        assert_eq!(
            resolve_symbols(
                &object(vec!["bamts_helper_get_super"]),
                &[("libbamts_node.a", symbols.as_slice())]
            ),
            Ok(())
        );
    }

    #[test]
    fn helper_resolution_reports_sorted_missing_symbols() {
        assert_eq!(
            resolve_symbols(&object(vec!["z", "a"]), &[]),
            Err(LinkError::UnresolvedSymbols(vec![
                "a".to_owned(),
                "z".to_owned()
            ]))
        );
    }

    #[test]
    fn helper_resolution_rejects_duplicate_providers() {
        let first = vec!["helper".to_owned()];
        let second = vec!["helper".to_owned()];
        assert_eq!(
            resolve_symbols(
                &object(vec!["helper"]),
                &[("one.a", first.as_slice()), ("two.a", second.as_slice())]
            ),
            Err(LinkError::DuplicateSymbols(vec!["helper".to_owned()]))
        );
    }

    const TEST_PF_R: u32 = 4;
    const TEST_PF_W: u32 = 2;
    const TEST_PF_X: u32 = 1;
    const TEST_PT_LOAD: u32 = 1;
    const TEST_PT_GNU_STACK: u32 = 0x6474_e551;

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16, endianness: ElfEndianness) {
        let encoded = match endianness {
            ElfEndianness::Little => value.to_le_bytes(),
            ElfEndianness::Big => value.to_be_bytes(),
        };
        bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32, endianness: ElfEndianness) {
        let encoded = match endianness {
            ElfEndianness::Little => value.to_le_bytes(),
            ElfEndianness::Big => value.to_be_bytes(),
        };
        bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64, endianness: ElfEndianness) {
        let encoded = match endianness {
            ElfEndianness::Little => value.to_le_bytes(),
            ElfEndianness::Big => value.to_be_bytes(),
        };
        bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
    }

    fn synthetic_elf64_endian(phdrs: &[(u32, u32)], endianness: ElfEndianness) -> Vec<u8> {
        let phentsize = 56_usize;
        let phnum = phdrs.len();
        let phoff = 64_usize;
        let mut image = vec![0_u8; phoff + phentsize * phnum];
        image[..16].copy_from_slice(&[
            0x7f,
            b'E',
            b'L',
            b'F',
            2,
            match endianness {
                ElfEndianness::Little => ELFDATA2LSB,
                ElfEndianness::Big => ELFDATA2MSB,
            },
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        write_u16(&mut image, 16, 2, endianness);
        write_u16(&mut image, 18, 62, endianness);
        write_u32(&mut image, 20, 1, endianness);
        write_u64(&mut image, 32, phoff as u64, endianness);
        write_u16(&mut image, 52, 64, endianness);
        write_u16(&mut image, 54, phentsize as u16, endianness);
        write_u16(&mut image, 56, phnum as u16, endianness);

        let image_len = image.len() as u64;
        for (index, &(p_type, p_flags)) in phdrs.iter().enumerate() {
            let offset = phoff + index * phentsize;
            write_u32(&mut image, offset, p_type, endianness);
            write_u32(&mut image, offset + 4, p_flags, endianness);
            if p_type == TEST_PT_LOAD {
                write_u64(&mut image, offset + 16, 0x40_0000, endianness);
                write_u64(&mut image, offset + 24, 0x40_0000, endianness);
                write_u64(&mut image, offset + 32, image_len, endianness);
                write_u64(&mut image, offset + 40, image_len, endianness);
                write_u64(&mut image, offset + 48, 0x1000, endianness);
            }
        }
        image
    }

    fn synthetic_elf64(phdrs: &[(u32, u32)]) -> Vec<u8> {
        synthetic_elf64_endian(phdrs, ElfEndianness::Little)
    }

    fn synthetic_elf64_extended_count_endian(
        late_type: u32,
        late_flags: u32,
        include_section_zero: bool,
        endianness: ElfEndianness,
    ) -> Vec<u8> {
        const PHENTSIZE: usize = 8;
        const PHNUM: usize = 65_536;
        const PHOFF: usize = 64;
        const SHENTSIZE: usize = 64;

        let table_end = PHOFF + PHENTSIZE * PHNUM;
        let image_len = table_end + usize::from(include_section_zero) * SHENTSIZE;
        let mut image = vec![0_u8; image_len];
        image[..16].copy_from_slice(&[
            0x7f,
            b'E',
            b'L',
            b'F',
            2,
            match endianness {
                ElfEndianness::Little => ELFDATA2LSB,
                ElfEndianness::Big => ELFDATA2MSB,
            },
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        write_u16(&mut image, 16, 2, endianness);
        write_u16(&mut image, 18, 62, endianness);
        write_u32(&mut image, 20, 1, endianness);
        write_u64(&mut image, 32, PHOFF as u64, endianness);
        write_u16(&mut image, 52, 64, endianness);
        write_u16(&mut image, 54, PHENTSIZE as u16, endianness);
        write_u16(&mut image, 56, PN_XNUM, endianness);

        write_u32(&mut image, PHOFF, TEST_PT_GNU_STACK, endianness);
        write_u32(&mut image, PHOFF + 4, TEST_PF_R | TEST_PF_W, endianness);
        let late_offset = PHOFF + (PHNUM - 1) * PHENTSIZE;
        write_u32(&mut image, late_offset, late_type, endianness);
        write_u32(&mut image, late_offset + 4, late_flags, endianness);

        if include_section_zero {
            write_u64(&mut image, 40, table_end as u64, endianness);
            write_u16(&mut image, 58, SHENTSIZE as u16, endianness);
            write_u32(
                &mut image,
                table_end + ELF64_SECTION_INFO_OFFSET,
                PHNUM as u32,
                endianness,
            );
        }
        image
    }

    fn valid_stack_image() -> Vec<u8> {
        synthetic_elf64(&[
            (TEST_PT_LOAD, TEST_PF_R | TEST_PF_X),
            (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W),
        ])
    }

    fn valid_be_stack_image() -> Vec<u8> {
        synthetic_elf64_endian(
            &[
                (TEST_PT_LOAD, TEST_PF_R | TEST_PF_X),
                (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W),
            ],
            ElfEndianness::Big,
        )
    }

    #[test]
    fn synthetic_rw_non_executable_stack_is_accepted() {
        assert_eq!(validate_linked_image(&valid_stack_image()), Ok(()));
    }

    #[test]
    fn synthetic_writable_executable_segment_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64(&[
                (TEST_PT_LOAD, TEST_PF_R | TEST_PF_W | TEST_PF_X),
                (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W),
            ])),
            Err(LinkError::WritableExecutableSegment { index: 0 })
        );
    }

    #[test]
    fn synthetic_executable_stack_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64(&[
                (TEST_PT_LOAD, TEST_PF_R | TEST_PF_X),
                (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W | TEST_PF_X),
            ])),
            Err(LinkError::ExecutableStack { index: 1 })
        );
    }

    #[test]
    fn synthetic_missing_stack_policy_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64(&[(TEST_PT_LOAD, TEST_PF_R | TEST_PF_X)])),
            Err(LinkError::MissingStackPolicy)
        );
    }

    #[test]
    fn synthetic_duplicate_stack_policy_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64(&[
                (TEST_PT_LOAD, TEST_PF_R | TEST_PF_X),
                (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W),
                (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W),
            ])),
            Err(LinkError::DuplicateStackPolicy { count: 2 })
        );
    }

    #[test]
    fn synthetic_extended_program_header_count_policy_is_enforced() {
        let accepted = synthetic_elf64_extended_count_endian(
            TEST_PT_LOAD,
            TEST_PF_R | TEST_PF_X,
            true,
            ElfEndianness::Little,
        );
        assert_eq!(
            validate_elf64_image_policy(&accepted, ElfEndianness::Little),
            Ok(())
        );
        assert_eq!(
            validate_elf64_image_policy(
                &synthetic_elf64_extended_count_endian(
                    TEST_PT_GNU_STACK,
                    TEST_PF_R | TEST_PF_W,
                    true,
                    ElfEndianness::Little,
                ),
                ElfEndianness::Little,
            ),
            Err(LinkError::DuplicateStackPolicy { count: 2 })
        );
        assert!(matches!(
            validate_elf64_image_policy(
                &synthetic_elf64_extended_count_endian(
                    TEST_PT_LOAD,
                    TEST_PF_R | TEST_PF_X,
                    false,
                    ElfEndianness::Little,
                ),
                ElfEndianness::Little,
            ),
            Err(LinkError::ImageParse(_))
        ));
    }

    #[test]
    fn synthetic_be_rw_non_executable_stack_is_accepted() {
        assert_eq!(validate_linked_image(&valid_be_stack_image()), Ok(()));
    }

    #[test]
    fn synthetic_be_writable_executable_segment_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64_endian(
                &[
                    (TEST_PT_LOAD, TEST_PF_R | TEST_PF_W | TEST_PF_X),
                    (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W),
                ],
                ElfEndianness::Big,
            )),
            Err(LinkError::WritableExecutableSegment { index: 0 })
        );
    }

    #[test]
    fn synthetic_be_executable_stack_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64_endian(
                &[
                    (TEST_PT_LOAD, TEST_PF_R | TEST_PF_X),
                    (TEST_PT_GNU_STACK, TEST_PF_R | TEST_PF_W | TEST_PF_X),
                ],
                ElfEndianness::Big,
            )),
            Err(LinkError::ExecutableStack { index: 1 })
        );
    }

    #[test]
    fn synthetic_be_missing_stack_policy_is_rejected() {
        assert_eq!(
            validate_linked_image(&synthetic_elf64_endian(
                &[(TEST_PT_LOAD, TEST_PF_R | TEST_PF_X)],
                ElfEndianness::Big,
            )),
            Err(LinkError::MissingStackPolicy)
        );
    }

    #[test]
    fn synthetic_be_extended_program_header_count_policy_is_enforced() {
        let accepted = synthetic_elf64_extended_count_endian(
            TEST_PT_LOAD,
            TEST_PF_R | TEST_PF_X,
            true,
            ElfEndianness::Big,
        );
        assert_eq!(
            validate_elf64_image_policy(&accepted, ElfEndianness::Big),
            Ok(())
        );
        assert_eq!(
            validate_elf64_image_policy(
                &synthetic_elf64_extended_count_endian(
                    TEST_PT_GNU_STACK,
                    TEST_PF_R | TEST_PF_W,
                    true,
                    ElfEndianness::Big,
                ),
                ElfEndianness::Big,
            ),
            Err(LinkError::DuplicateStackPolicy { count: 2 })
        );
    }

    #[test]
    fn synthetic_malformed_elf_still_returns_image_parse() {
        let truncated = {
            let mut image = valid_stack_image();
            image.truncate(32);
            image
        };
        assert!(matches!(
            validate_linked_image(&truncated),
            Err(LinkError::ImageParse(_))
        ));
        assert!(matches!(
            validate_linked_image(b"not-an-image"),
            Err(LinkError::ImageParse(_))
        ));
    }
}
