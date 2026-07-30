//! W^X executable-memory provider for the host JIT backend.
//!
//! Every mapping this provider owns is writable *or* executable, never both.
//! Cranelift writes generated code and relocations into writable, non-executable
//! pages; at finalization each mapping transitions to its exact final
//! protection in one shot, the instruction cache is cleared and the pipeline
//! flushed, and only then — after an exhaustive, gap-free OS permission query —
//! does the provider reach [`WxPhase::Executable`]. [`WxMemoryHandle`] mints the
//! private [`FinalizedMemory`] receipt that gates [`crate::JitProgram`]
//! publication; no receipt exists in [`WxPhase::Writable`] or [`WxPhase::Freed`].
//!
//! Each Cranelift allocation request gets its own page-rounded
//! [`region::Allocation`], so code, read-only data, and writable data never
//! share a page. Reclaiming every mapping exactly once is enforced by
//! [`WxMemoryProvider::release`]: it marks [`WxPhase::Freed`] before clearing the
//! owned allocations, so a second explicit `free_memory` and the provider `Drop`
//! are both no-ops.
//!
//! # Safety
//!
//! All `unsafe` in the host-JIT backend is confined to this module: the OS
//! mapping syscalls (`region`), the instruction-cache coherence call, and the
//! aarch64/Linux BTI `mprotect`. The crate is `#![deny(unsafe_code)]`; this one
//! module carries `#[allow(unsafe_code)]` (see `crate::jit_memory`).

#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
use std::ffi::c_void;
use std::io;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU8, Ordering};

use cranelift_jit::{BranchProtection, JITMemoryKind, JITMemoryProvider};
use cranelift_module::{ModuleError, ModuleResult};
type MemoryResult<T> = Result<T, Box<ModuleError>>;

// -- Stable lifecycle phases -------------------------------------------------

const PHASE_WRITABLE: u8 = 0;
const PHASE_EXECUTABLE: u8 = 1;
const PHASE_FREED: u8 = 2;

/// The stable lifecycle phases of a [`WxMemoryProvider`]. Allocation is legal
/// only in [`WxPhase::Writable`]; publication only after [`WxPhase::Executable`];
/// reclaim only via [`WxPhase::Freed`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WxPhase {
    /// Every owned mapping is fresh `READ_WRITE` and not executable; finalization
    /// has not started.
    Writable,
    /// Every owned mapping reached its exact final protection.
    Executable,
    /// Every owned mapping has been reclaimed.
    Freed,
}

impl WxPhase {
    const fn from_raw(raw: u8) -> WxPhase {
        match raw {
            PHASE_WRITABLE => WxPhase::Writable,
            PHASE_EXECUTABLE => WxPhase::Executable,
            PHASE_FREED => WxPhase::Freed,
            _ => panic!("invalid W^X memory phase"),
        }
    }

    const fn into_raw(self) -> u8 {
        match self {
            WxPhase::Writable => PHASE_WRITABLE,
            WxPhase::Executable => PHASE_EXECUTABLE,
            WxPhase::Freed => PHASE_FREED,
        }
    }
}

// -- Shared lifecycle (provider + handle) ------------------------------------

/// Lifecycle state shared between the provider (consumed by `JITModule`) and the
/// handle that mints the publication receipt.
struct Liveness {
    phase: AtomicU8,
    #[cfg(test)]
    released_mappings: AtomicUsize,
}

impl Liveness {
    fn phase(&self) -> WxPhase {
        WxPhase::from_raw(self.phase.load(Ordering::Acquire))
    }

    fn mark(&self, phase: WxPhase) {
        self.phase.store(phase.into_raw(), Ordering::Release);
    }

    #[cfg(test)]
    fn released_mappings(&self) -> usize {
        self.released_mappings.load(Ordering::Acquire)
    }
}

// -- Mapping kinds -----------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MappingKind {
    /// Executable once finalized: cache-clear, `RX`, then BTI where requested.
    Code,
    /// Read-only once finalized: `R`.
    ReadOnly,
    /// Stays writable: `RW`.
    Writable,
}

impl MappingKind {
    fn from_request(kind: JITMemoryKind) -> MappingKind {
        match kind {
            JITMemoryKind::Executable => MappingKind::Code,
            JITMemoryKind::ReadOnly => MappingKind::ReadOnly,
            JITMemoryKind::Writable => MappingKind::Writable,
        }
    }

    /// The exact protection every page of an owner extent must reach after a
    /// successful finalization.
    const fn final_protection(self) -> region::Protection {
        match self {
            MappingKind::Code => region::Protection::READ_EXECUTE,
            MappingKind::ReadOnly => region::Protection::READ,
            MappingKind::Writable => region::Protection::READ_WRITE,
        }
    }
}

/// One page-rounded mapping owned exclusively by the provider. Every Cranelift
/// allocation request is a distinct [`region::Allocation`] at `READ_WRITE`, so
/// different mapping kinds never share a page.
struct OwnedMapping {
    allocation: region::Allocation,
    kind: MappingKind,
}

// -- Provider ----------------------------------------------------------------

/// A cross-platform W^X memory provider for the host JIT.
///
/// Created together with a [`WxMemoryHandle`] *before* [`JITModule::new`] takes
/// ownership of it; the handle outlives the module so [`compile_lowered`](crate)
/// can mint the publication receipt after [`finalize_definitions`].
///
/// [`JITModule::new`]: cranelift_jit::JITModule::new
/// [`finalize_definitions`]: cranelift_module::Module::finalize_definitions
pub(crate) struct WxMemoryProvider {
    liveness: Arc<Liveness>,
    mappings: Vec<OwnedMapping>,
    /// `true` once [`WxMemoryProvider::finalize`] begins. Allocations are legal
    /// only while `Writable` *and* this is still false.
    finalization_started: bool,
    #[cfg(test)]
    fault: Option<InjectedFault>,
}

// SAFETY: the provider sole-owns every `region::Allocation`. The mappings have
// no thread affinity and are unmapped exactly once by `release`, which marks
// the shared phase `Freed` before clearing the owned allocations. This matches
// the ownership contract region and Cranelift's `Box<dyn JITMemoryProvider +
// Send>` assume; the provider is deliberately not `Sync` because its
// `region::Allocation`s are not `Sync`.
unsafe impl Send for WxMemoryProvider {}

impl WxMemoryProvider {
    /// Creates a provider plus the handle that observes its lifecycle and mints
    /// the publication receipt after successful finalization.
    pub(crate) fn new() -> (WxMemoryProvider, WxMemoryHandle) {
        let liveness = Arc::new(Liveness {
            phase: AtomicU8::new(PHASE_WRITABLE),
            #[cfg(test)]
            released_mappings: AtomicUsize::new(0),
        });
        let provider = WxMemoryProvider {
            liveness: Arc::clone(&liveness),
            mappings: Vec::new(),
            finalization_started: false,
            #[cfg(test)]
            fault: None,
        };
        (provider, WxMemoryHandle { liveness })
    }

    fn phase(&self) -> WxPhase {
        self.liveness.phase()
    }

    /// Unmaps every owned mapping exactly once. Marks [`WxPhase::Freed`] before
    /// clearing the owned allocations, so a second explicit `free_memory` and
    /// the provider `Drop` are both no-ops.
    fn release(&mut self) {
        if self.phase() == WxPhase::Freed {
            return;
        }
        // Mark Freed *before* unmapping, so a re-entrant or repeated release is
        // observed as already reclaimed.
        #[cfg(test)]
        let released = self.mappings.len();
        self.liveness.mark(WxPhase::Freed);
        self.mappings.clear();
        #[cfg(test)]
        self.liveness
            .released_mappings
            .fetch_add(released, Ordering::Release);
    }

    /// Runs every protect/cache/BTI/query/flush step. On complete success the
    /// shared phase advances to [`WxPhase::Executable`]; on any failure the
    /// caller releases every mapping while the provider is still logically
    /// non-`Executable`.
    #[cfg_attr(
        test,
        allow(
            clippy::explicit_counter_loop,
            reason = "counter drives fault injection"
        )
    )]
    fn finalize_mappings(&self, branch_protection: BranchProtection) -> MemoryResult<()> {
        #[cfg(test)]
        let mut transition = 0;
        for mapping in &self.mappings {
            #[cfg(test)]
            if self.fault == Some(InjectedFault::Protect { transition }) {
                return Err(backend_error(format!(
                    "injected fault at protection transition {transition}"
                )));
            }
            #[cfg(test)]
            {
                transition += 1;
            }
            let base = mapping.allocation.as_ptr::<u8>();
            let len = mapping.allocation.len();
            match mapping.kind {
                MappingKind::Writable => {}
                MappingKind::ReadOnly => {
                    // SAFETY: `base..base+len` is this provider's sole-owned,
                    // page-aligned, live mapping; `region::protect` rounds its
                    // own arguments to page boundaries.
                    unsafe { region::protect(base, len, region::Protection::READ) }.map_err(
                        |error| {
                            backend_error(format!(
                                "unable to make readonly data read-only: {error}"
                            ))
                        },
                    )?;
                }
                MappingKind::Code => {
                    // Clear the instruction cache BEFORE the RW -> RX transition,
                    // matching the documented ordering of `clear_cache`.
                    //
                    // SAFETY: `base..base+len` is the provider's sole-owned live
                    // code mapping.
                    unsafe {
                        wasmtime_internal_jit_icache_coherence::clear_cache(base.cast(), len)
                    }
                    .map_err(|error| {
                        backend_error(format!("unable to clear the instruction cache: {error}"))
                    })?;
                    // SAFETY: same live mapping as above.
                    unsafe { region::protect(base, len, region::Protection::READ_EXECUTE) }
                        .map_err(|error| {
                            backend_error(format!(
                                "unable to make code readable+executable: {error}"
                            ))
                        })?;
                    apply_branch_protection(base, len, branch_protection)?;
                }
            }
        }

        // One pipeline flush after every mapping has transitioned.
        #[cfg(test)]
        if self.fault == Some(InjectedFault::PipelineFlush) {
            return Err(backend_error("injected fault at pipeline flush"));
        }
        wasmtime_internal_jit_icache_coherence::pipeline_flush_mt().map_err(|error| {
            backend_error(format!("unable to flush the instruction pipeline: {error}"))
        })?;

        // Exhaustive, gap-free OS permission query over every owner extent.
        #[cfg(test)]
        if self.fault == Some(InjectedFault::PermissionQuery) {
            return Err(backend_error("injected fault at permission query"));
        }
        for mapping in &self.mappings {
            verify_extent(
                mapping.allocation.as_ptr::<u8>(),
                mapping.allocation.len(),
                mapping.kind.final_protection(),
            )?;
        }

        Ok(())
    }

    /// Installs a named fault at one finalization point. Test-only: a release
    /// build never injects a fault, so the full sequence is attempted.
    #[cfg(test)]
    pub(crate) fn inject_fault(&mut self, fault: InjectedFault) {
        self.fault = Some(fault);
    }
}

impl JITMemoryProvider for WxMemoryProvider {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        if self.phase() != WxPhase::Writable || self.finalization_started {
            return Err(invalid_input(
                "W^X provider accepts allocations only while writable before finalization",
            ));
        }
        if align == 0 || !align.is_power_of_two() {
            return Err(invalid_input("alignment must be a nonzero power of two"));
        }
        let align =
            usize::try_from(align).map_err(|_| invalid_input("alignment does not fit usize"))?;
        let page = region::page::size();
        if align > page {
            // `region::alloc` returns a page-aligned base, so it can only honor
            // alignments up to one page. Reject larger requests rather than
            // silently returning under-aligned memory.
            return Err(invalid_input(
                "alignment larger than one page is unsupported",
            ));
        }
        let rounded = size
            .checked_add(page - 1)
            .map(|sum| sum & !(page - 1))
            .ok_or_else(|| invalid_input("allocation size overflows page rounding"))?;
        if rounded == 0 {
            return Err(invalid_input("allocation size must be nonzero"));
        }

        // Every request is a distinct page-rounded RW allocation; mapping kinds
        // never share a page.
        let allocation =
            region::alloc(rounded, region::Protection::READ_WRITE).map_err(io::Error::other)?;
        let pointer = allocation.as_ptr::<u8>() as *mut u8;
        self.mappings.push(OwnedMapping {
            allocation,
            kind: MappingKind::from_request(kind),
        });
        Ok(pointer)
    }

    unsafe fn free_memory(&mut self) {
        self.release();
    }

    fn finalize(&mut self, branch_protection: BranchProtection) -> ModuleResult<()> {
        if self.finalization_started || self.phase() != WxPhase::Writable {
            // Finalization may start at most once, and only while writable.
            return Err(*backend_error(
                "W^X provider finalization may start at most once and only while writable",
            ));
        }
        self.finalization_started = true;
        match self.finalize_mappings(branch_protection) {
            Ok(()) => {
                // The phase reaches Executable only after every protect/cache/
                // BTI/query/flush operation has succeeded.
                self.liveness.mark(WxPhase::Executable);
                Ok(())
            }
            Err(error) => {
                // Any partial finalization failure poisons the module: release
                // every mapping, leave the phase Freed, and never publish.
                self.release();
                Err(*error)
            }
        }
    }
}

impl Drop for WxMemoryProvider {
    fn drop(&mut self) {
        self.release();
    }
}

// -- Publication receipt -----------------------------------------------------

/// Observes a provider's lifecycle and mints the publication receipt. Obtained
/// from [`WxMemoryProvider::new`] before the provider moves into `JITModule`, so
/// the handle outlives the module and survives `compile_lowered`.
#[derive(Clone)]
pub(crate) struct WxMemoryHandle {
    liveness: Arc<Liveness>,
}

impl WxMemoryHandle {
    /// The current lifecycle phase (shared with the provider).
    pub(crate) fn phase(&self) -> WxPhase {
        self.liveness.phase()
    }

    #[cfg(test)]
    fn released_mappings(&self) -> usize {
        self.liveness.released_mappings()
    }

    /// Mints the publication receipt. A receipt exists only in
    /// [`WxPhase::Executable`]: finalization reached the exact final protection
    /// on every page of every owner extent. No receipt exists in
    /// [`WxPhase::Writable`] or [`WxPhase::Freed`].
    pub(crate) fn require_finalized(&self) -> FinalizedMemory {
        assert_eq!(
            self.phase(),
            WxPhase::Executable,
            "host JIT cannot publish without finalized executable memory",
        );
        FinalizedMemory { _private: () }
    }
}

/// Proof that every owned mapping reached its exact final protection.
///
/// Constructible only by [`WxMemoryHandle::require_finalized`] and only in
/// [`WxPhase::Executable`]; owned by [`crate::JitProgram`] to gate publication.
/// It carries no state: a `JitProgram` holding it could only have been built
/// after a successful, fully-verified finalization.
pub(crate) struct FinalizedMemory {
    _private: (),
}

// -- Branch Target Identification (aarch64/Linux) ---------------------------

/// Applies Branch Target Identification to an `RX` code extent where Cranelift
/// requests it and the CPU supports it. No-op elsewhere.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
fn apply_branch_protection(
    base: *const u8,
    len: usize,
    branch_protection: BranchProtection,
) -> MemoryResult<()> {
    /// Linux `PROT_BTI` (kept as a literal so it compiles on every aarch64
    /// libc target, mirroring `cranelift-jit`).
    const PROT_BTI: libc::c_int = 0x10;
    if branch_protection == BranchProtection::BTI && std::arch::is_aarch64_feature_detected!("bti")
    {
        let prot = libc::PROT_READ | libc::PROT_EXEC | PROT_BTI;
        // SAFETY: `base..base+len` is the provider's sole-owned live code
        // mapping; `mprotect` operates on whole pages.
        if unsafe { libc::mprotect(base as *mut c_void, len, prot) } != 0 {
            return Err(backend_error(format!(
                "unable to apply BTI: {}",
                io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
fn apply_branch_protection(
    _base: *const u8,
    _len: usize,
    _branch_protection: BranchProtection,
) -> MemoryResult<()> {
    Ok(())
}

// -- Exhaustive permission query --------------------------------------------

/// Queries every page of one owner extent and rejects gaps, uncommitted or
/// guarded pages, unexpected permissions, and every W+X page.
fn verify_extent(base: *const u8, len: usize, expected: region::Protection) -> MemoryResult<()> {
    let start = base as usize;
    let end = start
        .checked_add(len)
        .ok_or_else(|| backend_error("owner extent overflows the address space"))?;
    let mut covered = start;
    let regions = region::query_range(base, len)
        .map_err(|error| backend_error(format!("unable to query an owner extent: {error}")))?;
    for region in regions {
        let region = region
            .map_err(|error| backend_error(format!("unable to query an owner extent: {error}")))?;
        let range = region.as_range();
        if range.end <= covered {
            continue;
        }
        if range.start > covered {
            return Err(backend_error(format!(
                "unmapped gap at {covered:#x} inside an owner extent"
            )));
        }
        if !region.is_committed() || region.is_guarded() {
            return Err(backend_error(format!(
                "uncommitted or guarded page inside an owner extent at {:#x}",
                range.start
            )));
        }
        if region.is_writable() && region.is_executable() {
            return Err(backend_error(format!(
                "W+X page inside an owner extent at {:#x}",
                range.start
            )));
        }
        if region.protection() != expected {
            return Err(backend_error(format!(
                "unexpected permissions {} (expected {expected}) at {:#x}",
                region.protection(),
                range.start
            )));
        }
        covered = range.end;
    }
    if covered < end {
        return Err(backend_error(format!(
            "unmapped gap at {covered:#x} at the tail of an owner extent"
        )));
    }
    Ok(())
}

// -- Error helpers -----------------------------------------------------------

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Wraps a backend failure as a [`ModuleError`]. These are memory-protection
/// failures rather than allocation failures, but `ModuleError::Allocation`
/// is the only backend-originated variant that carries a plain [`io::Error`]
/// without introducing a new direct dependency.
fn backend_error(message: impl Into<String>) -> Box<ModuleError> {
    Box::new(ModuleError::Allocation {
        err: io::Error::other(message.into()),
    })
}

// -- Test fault injection ----------------------------------------------------

/// Named fault-injection points for the partial-finalization regression tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InjectedFault {
    /// Fail the `transition`-th (0-based) per-mapping protection transition.
    Protect { transition: usize },
    /// Fail the single post-transition instruction-pipeline flush.
    PipelineFlush,
    /// Fail the exhaustive post-transition OS permission query.
    PermissionQuery,
}

#[cfg(test)]
mod tests {
    use cranelift_jit::{BranchProtection, JITMemoryKind, JITMemoryProvider};
    use region::Protection;

    use super::{InjectedFault, WxMemoryProvider, WxPhase};

    /// Allocates one of every mapping kind and returns `(code, readonly, writable)`.
    fn allocate_all_kinds(provider: &mut WxMemoryProvider) -> (*mut u8, *mut u8, *mut u8) {
        let code = provider
            .allocate(100, 16, JITMemoryKind::Executable)
            .expect("code allocation succeeds");
        let readonly = provider
            .allocate(64, 8, JITMemoryKind::ReadOnly)
            .expect("readonly allocation succeeds");
        let writable = provider
            .allocate(64, 8, JITMemoryKind::Writable)
            .expect("writable allocation succeeds");
        (code, readonly, writable)
    }

    /// Page-rounded length of an `size`-byte request.
    fn rounded_len(size: usize) -> usize {
        let page = region::page::size();
        size.div_ceil(page) * page
    }

    /// Walks `[base, base+len)` with `region::query_range` and asserts every page
    /// is mapped, gap-free, committed, not guarded, not W+X, and exactly `expected`.
    fn assert_extent(base: *const u8, len: usize, expected: Protection) {
        let start = base as usize;
        let end = start + len;
        let mut covered = start;
        let mut any = false;
        for region in region::query_range(base, len).expect("extent is queryable") {
            let region = region.expect("region is queryable");
            let range = region.as_range();
            if range.end <= covered {
                continue;
            }
            assert!(range.start <= covered, "gap inside extent at {covered:#x}");
            assert!(
                region.is_committed(),
                "uncommitted page at {:#x}",
                range.start
            );
            assert!(!region.is_guarded(), "guarded page at {:#x}", range.start);
            assert!(
                !(region.is_writable() && region.is_executable()),
                "W+X page at {:#x}",
                range.start
            );
            assert_eq!(region.protection(), expected, "at {:#x}", range.start);
            covered = range.end;
            any = true;
        }
        assert!(any, "extent [{start:#x}, {end:#x}) has no mapped page");
        assert!(
            covered >= end,
            "extent [{start:#x}, {end:#x}) only covered to {covered:#x}"
        );
    }

    #[test]
    fn fresh_mappings_are_rw_and_not_executable_over_full_extent() {
        let (mut provider, memory) = WxMemoryProvider::new();
        assert_eq!(memory.phase(), WxPhase::Writable);

        let (code, readonly, writable) = allocate_all_kinds(&mut provider);

        // Every fresh mapping is RW and not executable over its full extent.
        for (base, size) in [(code, 100), (readonly, 64), (writable, 64)] {
            assert_extent(base, rounded_len(size), Protection::READ_WRITE);
        }
    }

    #[test]
    fn finalize_gives_code_rx_readonly_r_writable_rw_without_wx() {
        let (mut provider, memory) = WxMemoryProvider::new();
        let (code, readonly, writable) = allocate_all_kinds(&mut provider);

        // Simulate Cranelift writing generated bytes into the writable mapping.
        // SAFETY: `code` is a fresh RW allocation of `rounded_len(100)` bytes.
        unsafe { core::ptr::write_bytes(code, 0x90, rounded_len(100)) };

        provider
            .finalize(BranchProtection::None)
            .expect("finalization transitions every mapping");

        assert_eq!(memory.phase(), WxPhase::Executable);
        assert_extent(code, rounded_len(100), Protection::READ_EXECUTE);
        assert_extent(readonly, rounded_len(64), Protection::READ);
        assert_extent(writable, rounded_len(64), Protection::READ_WRITE);

        let _receipt = memory.require_finalized();
    }

    #[test]
    #[should_panic(expected = "host JIT cannot publish without finalized executable memory")]
    fn receipt_cannot_mint_while_writable() {
        let (_provider, memory) = WxMemoryProvider::new();
        let _receipt = memory.require_finalized();
    }

    #[test]
    #[should_panic(expected = "host JIT cannot publish without finalized executable memory")]
    fn receipt_cannot_mint_after_release() {
        let (provider, memory) = WxMemoryProvider::new();
        drop(provider);
        let _receipt = memory.require_finalized();
    }

    #[test]
    fn allocate_after_finalization_and_repeated_finalization_fail() {
        let (mut provider, memory) = WxMemoryProvider::new();
        let (code, _readonly, _writable) = allocate_all_kinds(&mut provider);
        provider
            .finalize(BranchProtection::None)
            .expect("finalization succeeds");

        // Finalization has started: allocations and a second finalization fail.
        assert!(provider.allocate(64, 8, JITMemoryKind::Writable).is_err());
        assert!(provider.finalize(BranchProtection::None).is_err());

        // The phase and the protected mappings are unchanged: no rollback on a
        // successful path's repeated-finalization attempt.
        assert_eq!(memory.phase(), WxPhase::Executable);
        assert_extent(code, rounded_len(100), Protection::READ_EXECUTE);
    }

    #[test]
    fn injected_partial_failure_never_mints_receipt_and_releases_all_mappings() {
        for fault in [
            InjectedFault::Protect { transition: 1 },
            InjectedFault::PipelineFlush,
            InjectedFault::PermissionQuery,
        ] {
            let (mut provider, memory) = WxMemoryProvider::new();
            allocate_all_kinds(&mut provider);
            provider.inject_fault(fault);

            provider
                .finalize(BranchProtection::None)
                .expect_err("injected fault prevents finalization");

            // No receipt; the module is poisoned and Freed.
            assert_eq!(memory.phase(), WxPhase::Freed);

            assert_eq!(memory.released_mappings(), 3);

            // A poisoned module is never retried.
            assert!(provider.finalize(BranchProtection::None).is_err());
            assert!(provider.allocate(16, 8, JITMemoryKind::Writable).is_err());
        }
    }

    #[test]
    fn explicit_free_then_drop_unmaps_once_and_ends_freed() {
        let (mut provider, memory) = WxMemoryProvider::new();
        allocate_all_kinds(&mut provider);
        provider
            .finalize(BranchProtection::None)
            .expect("finalization succeeds");

        // SAFETY: no code from this provider is executing.
        unsafe { provider.free_memory() };
        assert_eq!(memory.released_mappings(), 3);

        // Idempotent: a second explicit free and the Drop are both no-ops.
        // SAFETY: no code from this provider is executing.
        unsafe { provider.free_memory() };
        drop(provider);
        assert_eq!(memory.phase(), WxPhase::Freed);
        assert_eq!(memory.released_mappings(), 3);
    }

    #[test]
    fn dropping_unfinalized_provider_unmaps_every_mapping() {
        let (mut provider, memory) = WxMemoryProvider::new();
        allocate_all_kinds(&mut provider);
        drop(provider);

        assert_eq!(memory.phase(), WxPhase::Freed);
        assert_eq!(memory.released_mappings(), 3);
    }

    #[test]
    fn allocation_rejects_invalid_alignment_and_size() {
        let (mut provider, _memory) = WxMemoryProvider::new();
        let page = region::page::size();

        assert!(provider.allocate(64, 0, JITMemoryKind::Writable).is_err());
        assert!(provider.allocate(64, 3, JITMemoryKind::Writable).is_err());
        assert!(
            provider
                .allocate(
                    64,
                    u64::try_from(page * 2).unwrap(),
                    JITMemoryKind::Writable
                )
                .is_err()
        );
        assert!(provider.allocate(0, 8, JITMemoryKind::Writable).is_err());
        assert!(
            provider
                .allocate(usize::MAX, 8, JITMemoryKind::Writable)
                .is_err()
        );

        // Valid: page-aligned base honors any alignment up to one page.
        provider
            .allocate(64, 1, JITMemoryKind::Writable)
            .expect("page-aligned base satisfies align 1");
        provider
            .allocate(64, u64::try_from(page).unwrap(), JITMemoryKind::Writable)
            .expect("page-sized alignment is honored");
    }
}
