import Bamti.Bytecode.Verify

namespace Bamti.Bytecode

/-- Persisted identity: every field participates in cache lookup. -/
structure CacheKey where
  source : Nat
  compilerAbi : Nat
  options : Nat
  moduleId : Nat
  deriving DecidableEq, Repr

/-- Process-local facts deliberately excluded from a serialized cache key. -/
structure RuntimeIdentity where
  isolate : Nat
  realm : Nat
  heapEpoch : Nat
  codeAddress : Nat
  deriving DecidableEq, Repr

structure CacheLookup where
  serialized : CacheKey
  runtime : RuntimeIdentity
  deriving DecidableEq, Repr

def serializedKey (lookup : CacheLookup) : CacheKey := lookup.serialized

def cacheMatches (left right : CacheLookup) : Prop :=
  left.serialized.source = right.serialized.source ∧
  left.serialized.compilerAbi = right.serialized.compilerAbi ∧
  left.serialized.options = right.serialized.options ∧
  left.serialized.moduleId = right.serialized.moduleId

/-- A hit may cross isolates and realms only because those runtime identities never serialize. -/
theorem no_serialized_runtime_identity (key : CacheKey)
    (leftRuntime rightRuntime : RuntimeIdentity) (runtimeDistinct : leftRuntime ≠ rightRuntime) :
    cacheMatches ⟨key, leftRuntime⟩ ⟨key, rightRuntime⟩ ∧
    serializedKey ⟨key, leftRuntime⟩ = serializedKey ⟨key, rightRuntime⟩ ∧
    (⟨key, leftRuntime⟩ : CacheLookup).runtime ≠ (⟨key, rightRuntime⟩ : CacheLookup).runtime := by
  exact ⟨⟨rfl, rfl, rfl, rfl⟩, rfl, runtimeDistinct⟩

end Bamti.Bytecode
