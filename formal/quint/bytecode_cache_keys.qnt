module bytecode_cache_keys {
  val Seed = 313
  val Bound = 3

  var sourceDigest: int
  var compilerAbi: int
  var optionsDigest: int
  var moduleIdentity: int
  var cachedSourceDigest: int
  var cachedCompilerAbi: int
  var cachedOptionsDigest: int
  var cachedModuleIdentity: int
  var verified: bool
  var verificationFailed: bool
  var corruption: bool
  var cacheHit: bool
  var cacheMiss: bool
  var executionAdmitted: bool
  var executed: bool
  var sourceDigestUsed: bool
  var compilerAbiUsed: bool
  var optionsDigestUsed: bool
  var moduleIdentityUsed: bool
  var mtimeUsed: bool
  var pathIdUsed: bool
  var pointerUsed: bool
  var processRevisionUsed: bool

  action init = all {
    sourceDigest' = 1,
    compilerAbi' = 1,
    optionsDigest' = 1,
    moduleIdentity' = 1,
    cachedSourceDigest' = 1,
    cachedCompilerAbi' = 1,
    cachedOptionsDigest' = 1,
    cachedModuleIdentity' = 1,
    verified' = true,
    verificationFailed' = false,
    corruption' = false,
    cacheHit' = false,
    cacheMiss' = false,
    executionAdmitted' = false,
    executed' = false,
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action keepAuthoritativeKey = all {
    sourceDigest' = sourceDigest,
    compilerAbi' = compilerAbi,
    optionsDigest' = optionsDigest,
    moduleIdentity' = moduleIdentity,
  }

  action matchingCachedKey = all {
    cachedSourceDigest' = sourceDigest,
    cachedCompilerAbi' = compilerAbi,
    cachedOptionsDigest' = optionsDigest,
    cachedModuleIdentity' = moduleIdentity,
  }

  action keepCachedKey = all {
    cachedSourceDigest' = cachedSourceDigest,
    cachedCompilerAbi' = cachedCompilerAbi,
    cachedOptionsDigest' = cachedOptionsDigest,
    cachedModuleIdentity' = cachedModuleIdentity,
  }

  action verifiedCandidate = all {
    verified' = true,
    verificationFailed' = false,
    corruption' = false,
  }

  action corruptedCandidate = all {
    verified' = true,
    verificationFailed' = false,
    corruption' = true,
  }

  action unverifiedCandidate = all {
    verified' = false,
    verificationFailed' = true,
    corruption' = false,
  }

  action keepVerificationState = all {
    verified' = verified,
    verificationFailed' = verificationFailed,
    corruption' = corruption,
  }

  action hitAndExecute = all {
    cacheHit' = true,
    cacheMiss' = false,
    executionAdmitted' = true,
    executed' = true,
  }

  action rejectAsMiss = all {
    cacheHit' = false,
    cacheMiss' = true,
    executionAdmitted' = false,
    executed' = false,
  }

  action keepDecision = all {
    cacheHit' = cacheHit,
    cacheMiss' = cacheMiss,
    executionAdmitted' = executionAdmitted,
    executed' = executed,
  }

  action authoritativeKeyOnly = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action omitSourceDigest = all {
    sourceDigestUsed' = false,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action omitCompilerAbi = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = false,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action omitOptionsDigest = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = false,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action omitModuleIdentity = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = false,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action useMtime = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = true,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action usePathId = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = true,
    pointerUsed' = false,
    processRevisionUsed' = false,
  }

  action usePointer = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = true,
    processRevisionUsed' = false,
  }

  action useProcessRevision = all {
    sourceDigestUsed' = true,
    compilerAbiUsed' = true,
    optionsDigestUsed' = true,
    moduleIdentityUsed' = true,
    mtimeUsed' = false,
    pathIdUsed' = false,
    pointerUsed' = false,
    processRevisionUsed' = true,
  }

  action sourceDigestMismatch = all {
    cachedSourceDigest' = sourceDigest + 1,
    cachedCompilerAbi' = compilerAbi,
    cachedOptionsDigest' = optionsDigest,
    cachedModuleIdentity' = moduleIdentity,
  }

  action compilerAbiMismatch = all {
    cachedSourceDigest' = sourceDigest,
    cachedCompilerAbi' = compilerAbi + 1,
    cachedOptionsDigest' = optionsDigest,
    cachedModuleIdentity' = moduleIdentity,
  }

  action optionsDigestMismatch = all {
    cachedSourceDigest' = sourceDigest,
    cachedCompilerAbi' = compilerAbi,
    cachedOptionsDigest' = optionsDigest + 1,
    cachedModuleIdentity' = moduleIdentity,
  }

  action moduleIdentityMismatch = all {
    cachedSourceDigest' = sourceDigest,
    cachedCompilerAbi' = compilerAbi,
    cachedOptionsDigest' = optionsDigest,
    cachedModuleIdentity' = moduleIdentity + 1,
  }

  action serve = all {
    keepAuthoritativeKey,
    matchingCachedKey,
    verifiedCandidate,
    hitAndExecute,
    authoritativeKeyOnly,
  }

  action loadCorrupt = all {
    keepAuthoritativeKey,
    matchingCachedKey,
    corruptedCandidate,
    rejectAsMiss,
    authoritativeKeyOnly,
  }

  action loadUnverified = all {
    keepAuthoritativeKey,
    matchingCachedKey,
    unverifiedCandidate,
    rejectAsMiss,
    authoritativeKeyOnly,
  }

  action loadSourceDigestMismatch = all {
    keepAuthoritativeKey,
    sourceDigestMismatch,
    verifiedCandidate,
    rejectAsMiss,
    authoritativeKeyOnly,
  }

  action loadCompilerAbiMismatch = all {
    keepAuthoritativeKey,
    compilerAbiMismatch,
    verifiedCandidate,
    rejectAsMiss,
    authoritativeKeyOnly,
  }

  action loadOptionsDigestMismatch = all {
    keepAuthoritativeKey,
    optionsDigestMismatch,
    verifiedCandidate,
    rejectAsMiss,
    authoritativeKeyOnly,
  }

  action loadModuleIdentityMismatch = all {
    keepAuthoritativeKey,
    moduleIdentityMismatch,
    verifiedCandidate,
    rejectAsMiss,
    authoritativeKeyOnly,
  }

  action loadKeyMismatch = any {
    loadSourceDigestMismatch,
    loadCompilerAbiMismatch,
    loadOptionsDigestMismatch,
    loadModuleIdentityMismatch,
  }

  action miss = any {
    loadCorrupt,
    loadUnverified,
    loadKeyMismatch,
  }

  action step = any {
    serve,
    miss,
  }

  action retainLoadState = all {
    keepAuthoritativeKey,
    keepCachedKey,
    keepVerificationState,
    keepDecision,
  }

  action retainKeySchema = all {
    sourceDigestUsed' = sourceDigestUsed,
    compilerAbiUsed' = compilerAbiUsed,
    optionsDigestUsed' = optionsDigestUsed,
    moduleIdentityUsed' = moduleIdentityUsed,
    mtimeUsed' = mtimeUsed,
    pathIdUsed' = pathIdUsed,
    pointerUsed' = pointerUsed,
    processRevisionUsed' = processRevisionUsed,
  }

  action stutter = all {
    retainLoadState,
    retainKeySchema,
  }

  action faultAdmitUnverified = all {
    keepAuthoritativeKey,
    matchingCachedKey,
    unverifiedCandidate,
    hitAndExecute,
    authoritativeKeyOnly,
  }

  action faultOmitSourceDigest = all {
    retainLoadState,
    omitSourceDigest,
  }

  action faultOmitCompilerAbi = all {
    retainLoadState,
    omitCompilerAbi,
  }

  action faultOmitOptionsDigest = all {
    retainLoadState,
    omitOptionsDigest,
  }

  action faultOmitModuleIdentity = all {
    retainLoadState,
    omitModuleIdentity,
  }

  action faultUseMtime = all {
    retainLoadState,
    useMtime,
  }

  action faultUsePathId = all {
    retainLoadState,
    usePathId,
  }

  action faultUsePointer = all {
    retainLoadState,
    usePointer,
  }

  action faultUseProcessRevision = all {
    retainLoadState,
    useProcessRevision,
  }

  val AuthoritativeKeyInBounds =
    sourceDigest > 0 and sourceDigest <= Bound and
      compilerAbi > 0 and compilerAbi <= Bound and
      optionsDigest > 0 and optionsDigest <= Bound and
      moduleIdentity > 0 and moduleIdentity <= Bound

  val CachedKeyInBounds =
    cachedSourceDigest > 0 and cachedSourceDigest <= Bound and
      cachedCompilerAbi > 0 and cachedCompilerAbi <= Bound and
      cachedOptionsDigest > 0 and cachedOptionsDigest <= Bound and
      cachedModuleIdentity > 0 and cachedModuleIdentity <= Bound

  val CacheKeyMatches =
    sourceDigest == cachedSourceDigest and
      compilerAbi == cachedCompilerAbi and
      optionsDigest == cachedOptionsDigest and
      moduleIdentity == cachedModuleIdentity

  val AllAuthoritativeFieldsUsed =
    sourceDigestUsed and compilerAbiUsed and optionsDigestUsed and moduleIdentityUsed

  val NoForbiddenIdentitySource =
    not(mtimeUsed) and not(pathIdUsed) and not(pointerUsed) and not(processRevisionUsed)

  val CacheFieldIsolation =
    AuthoritativeKeyInBounds and CachedKeyInBounds and NoForbiddenIdentitySource

  val ExactCacheFieldIsolation =
    CacheFieldIsolation and AllAuthoritativeFieldsUsed

  val AdmissionAllowed =
    verified and not(verificationFailed) and not(corruption) and CacheKeyMatches and
      ExactCacheFieldIsolation

  val ServeOnlyVerified =
    (not(cacheHit) or executionAdmitted) and
      (not(executed) or executionAdmitted) and
      (not(executionAdmitted) or
        (cacheHit and not(cacheMiss) and executed and AdmissionAllowed)) and
      (not(cacheMiss) or
        (not(cacheHit) and not(executionAdmitted) and not(executed)))

  run cache_boundsTest = init
    .then(serve)
    .then(all {
      stutter,
      assert(
        Seed == 313 and Bound == 3 and cacheHit and not(cacheMiss) and
          executionAdmitted and executed and AdmissionAllowed and ServeOnlyVerified and
          CacheFieldIsolation and ExactCacheFieldIsolation
      ),
    })

  run corruptMissTest = init
    .then(loadCorrupt)
    .then(all {
      stutter,
      assert(
        corruption and cacheMiss and not(cacheHit) and not(executionAdmitted) and
          not(executed) and ServeOnlyVerified and CacheFieldIsolation and ExactCacheFieldIsolation
      ),
    })

  run unverifiedMissTest = init
    .then(loadUnverified)
    .then(all {
      stutter,
      assert(
        verificationFailed and not(verified) and cacheMiss and not(cacheHit) and
          not(executionAdmitted) and not(executed) and ServeOnlyVerified and
          CacheFieldIsolation and ExactCacheFieldIsolation
      ),
    })

  run keyMismatchMissTest = init
    .then(loadKeyMismatch)
    .then(all {
      stutter,
      assert(
        not(CacheKeyMatches) and cacheMiss and not(cacheHit) and not(executionAdmitted) and
          not(executed) and ServeOnlyVerified and CacheFieldIsolation and ExactCacheFieldIsolation
      ),
    })

  run sourceDigestMismatchMissTest = init
    .then(loadSourceDigestMismatch)
    .then(all {
      stutter,
      assert(
        cachedSourceDigest != sourceDigest and cacheMiss and not(executionAdmitted) and
          not(executed) and ServeOnlyVerified
      ),
    })

  run compilerAbiMismatchMissTest = init
    .then(loadCompilerAbiMismatch)
    .then(all {
      stutter,
      assert(
        cachedCompilerAbi != compilerAbi and cacheMiss and not(executionAdmitted) and
          not(executed) and ServeOnlyVerified
      ),
    })

  run optionsDigestMismatchMissTest = init
    .then(loadOptionsDigestMismatch)
    .then(all {
      stutter,
      assert(
        cachedOptionsDigest != optionsDigest and cacheMiss and not(executionAdmitted) and
          not(executed) and ServeOnlyVerified
      ),
    })

  run moduleIdentityMismatchMissTest = init
    .then(loadModuleIdentityMismatch)
    .then(all {
      stutter,
      assert(
        cachedModuleIdentity != moduleIdentity and cacheMiss and not(executionAdmitted) and
          not(executed) and ServeOnlyVerified
      ),
    })

  run missReachableTest = init
    .then(miss)
    .then(all {
      stutter,
      assert(
        cacheMiss and not(cacheHit) and not(executionAdmitted) and not(executed) and
          ServeOnlyVerified and ExactCacheFieldIsolation
      ),
    })

  run admitUnverifiedMutationTest = init
    .then(faultAdmitUnverified)
    .then(all {
      stutter,
      assert(
        not(ServeOnlyVerified) and CacheFieldIsolation and ExactCacheFieldIsolation
      ),
    })

  run omitSourceDigestMutationTest = init
    .then(faultOmitSourceDigest)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and CacheFieldIsolation and not(ExactCacheFieldIsolation)
      ),
    })

  run omitCompilerAbiMutationTest = init
    .then(faultOmitCompilerAbi)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and CacheFieldIsolation and not(ExactCacheFieldIsolation)
      ),
    })

  run omitOptionsDigestMutationTest = init
    .then(faultOmitOptionsDigest)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and CacheFieldIsolation and not(ExactCacheFieldIsolation)
      ),
    })

  run omitModuleIdentityMutationTest = init
    .then(faultOmitModuleIdentity)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and CacheFieldIsolation and not(ExactCacheFieldIsolation)
      ),
    })

  run mtimeMutationTest = init
    .then(faultUseMtime)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and AuthoritativeKeyInBounds and CachedKeyInBounds and
          AllAuthoritativeFieldsUsed and not(CacheFieldIsolation) and not(ExactCacheFieldIsolation)
      ),
    })

  run pathIdMutationTest = init
    .then(faultUsePathId)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and AuthoritativeKeyInBounds and CachedKeyInBounds and
          AllAuthoritativeFieldsUsed and not(CacheFieldIsolation) and not(ExactCacheFieldIsolation)
      ),
    })

  run pointerMutationTest = init
    .then(faultUsePointer)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and AuthoritativeKeyInBounds and CachedKeyInBounds and
          AllAuthoritativeFieldsUsed and not(CacheFieldIsolation) and not(ExactCacheFieldIsolation)
      ),
    })

  run processRevisionMutationTest = init
    .then(faultUseProcessRevision)
    .then(all {
      stutter,
      assert(
        ServeOnlyVerified and AuthoritativeKeyInBounds and CachedKeyInBounds and
          AllAuthoritativeFieldsUsed and not(CacheFieldIsolation) and not(ExactCacheFieldIsolation)
      ),
    })
}
