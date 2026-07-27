module napi_lifecycle {
  val Seed = 311
  val Bound = 3
  val MaxAbi = 9
  val ClosedPhase = 2

  var owner: int
  var callbackOwner: int
  var generation: int
  var handleGeneration: int
  var scopeDepth: int
  var scopeStack: List[int]
  var nextScope: int
  var lastClosePreStack: List[int]
  var lastClosedScope: int
  var hasClosedScope: bool
  var refRoots: int
  var asyncPending: int
  var tsfnPending: int
  var teardownPhase: int
  var previousTeardownPhase: int
  var finalizerQueued: bool
  var finalized: bool
  var finalizerRuns: int
  var requestedAbi: int
  var abiRejected: bool
  var callbackAfterTeardown: bool
  var callbackDelivered: int
  var callbackRejected: bool

  action init = all {
    owner' = 1,
    callbackOwner' = 1,
    generation' = 1,
    handleGeneration' = 1,
    scopeDepth' = 0,
    scopeStack' = List(),
    nextScope' = 1,
    lastClosePreStack' = List(0),
    lastClosedScope' = 0,
    hasClosedScope' = false,
    refRoots' = 0,
    asyncPending' = 0,
    tsfnPending' = 0,
    teardownPhase' = 0,
    previousTeardownPhase' = 0,
    finalizerQueued' = false,
    finalized' = false,
    finalizerRuns' = 0,
    requestedAbi' = 1,
    abiRejected' = false,
    callbackAfterTeardown' = false,
    callbackDelivered' = 0,
    callbackRejected' = false,
  }

  action openScope = all {
    teardownPhase == 0 and scopeDepth < Bound and nextScope <= Bound,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth + 1,
    scopeStack' = scopeStack.append(nextScope),
    nextScope' = nextScope + 1,
    lastClosePreStack' = List(0),
    lastClosedScope' = 0,
    hasClosedScope' = false,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action closeScope = all {
    teardownPhase == 0 and scopeDepth > 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth - 1,
    scopeStack' = scopeStack.slice(0, scopeDepth - 1),
    nextScope' = nextScope,
    lastClosePreStack' = scopeStack,
    lastClosedScope' = scopeStack.nth(scopeDepth - 1),
    hasClosedScope' = true,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action refHandle = all {
    teardownPhase == 0 and refRoots < Bound,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots + 1,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action unrefHandle = all {
    teardownPhase == 0 and refRoots > 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots - 1,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action queueAsyncWork = all {
    teardownPhase == 0 and asyncPending < Bound,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending + 1,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action deliverAsyncCallback = all {
    teardownPhase == 0 and asyncPending > 0 and callbackDelivered < Bound,
    owner' = owner,
    callbackOwner' = owner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending - 1,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered + 1,
    callbackRejected' = callbackRejected,
  }

  action queueTsfn = all {
    teardownPhase == 0 and tsfnPending < Bound,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending + 1,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action drainTsfn = all {
    teardownPhase == 0 and tsfnPending > 0 and callbackDelivered < Bound,
    owner' = owner,
    callbackOwner' = owner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending - 1,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered + 1,
    callbackRejected' = callbackRejected,
  }

  action requestAbi = all {
    teardownPhase == 0 and requestedAbi < MaxAbi,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi + 1,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action rejectUnsupportedAbi = all {
    teardownPhase == 0 and requestedAbi == MaxAbi,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = true,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action teardown = all {
    teardownPhase == 0 and scopeDepth == 0 and refRoots == 0 and asyncPending == 0 and tsfnPending == 0 and not(finalizerQueued) and not(finalized),
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation + 1,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase + 1,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = true,
    finalized' = false,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action rejectPostTeardownCallback = all {
    teardownPhase > 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = true,
  }

  action runFinalizer = all {
    teardownPhase == 1 and finalizerQueued and not(finalized) and finalizerRuns == 0 and scopeDepth == 0 and refRoots == 0 and asyncPending == 0 and tsfnPending == 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = ClosedPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = false,
    finalized' = true,
    finalizerRuns' = finalizerRuns + 1,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultOwnerGeneration = all {
    teardownPhase == 0,
    owner' = owner,
    callbackOwner' = owner + 1,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultScopeNonLifo = all {
    teardownPhase == 0 and scopeDepth > 1,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth - 1,
    scopeStack' = scopeStack.tail(),
    nextScope' = nextScope,
    lastClosePreStack' = scopeStack,
    lastClosedScope' = scopeStack.head(),
    hasClosedScope' = true,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultRefUnderflow = all {
    teardownPhase == 0 and refRoots > 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = -1,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultAsyncOverflow = all {
    teardownPhase == 0 and asyncPending > 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = Bound + 1,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultTsfnOverflow = all {
    teardownPhase == 0 and tsfnPending > 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = Bound + 1,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultCallbackAfterTeardown = all {
    teardownPhase > 0 and callbackDelivered < Bound,
    owner' = owner,
    callbackOwner' = owner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = true,
    callbackDelivered' = callbackDelivered + 1,
    callbackRejected' = callbackRejected,
  }

  action faultFinalizerDuplicate = all {
    finalized and finalizerRuns == 1,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns + 1,
    requestedAbi' = requestedAbi,
    abiRejected' = abiRejected,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action faultAbiAboveCap = all {
    teardownPhase == 0,
    owner' = owner,
    callbackOwner' = callbackOwner,
    generation' = generation,
    handleGeneration' = handleGeneration,
    scopeDepth' = scopeDepth,
    scopeStack' = scopeStack,
    nextScope' = nextScope,
    lastClosePreStack' = lastClosePreStack,
    lastClosedScope' = lastClosedScope,
    hasClosedScope' = hasClosedScope,
    refRoots' = refRoots,
    asyncPending' = asyncPending,
    tsfnPending' = tsfnPending,
    teardownPhase' = teardownPhase,
    previousTeardownPhase' = teardownPhase,
    finalizerQueued' = finalizerQueued,
    finalized' = finalized,
    finalizerRuns' = finalizerRuns,
    requestedAbi' = MaxAbi + 1,
    abiRejected' = false,
    callbackAfterTeardown' = callbackAfterTeardown,
    callbackDelivered' = callbackDelivered,
    callbackRejected' = callbackRejected,
  }

  action step = any {
    openScope,
    closeScope,
    refHandle,
    unrefHandle,
    queueAsyncWork,
    deliverAsyncCallback,
    queueTsfn,
    drainTsfn,
    requestAbi,
    rejectUnsupportedAbi,
    teardown,
    rejectPostTeardownCallback,
    runFinalizer,
  }

  val InvOwnerGeneration =
    callbackOwner == owner and
    callbackDelivered >= 0 and callbackDelivered <= Bound and
    (not(teardownPhase == 0) or generation == handleGeneration) and
    (not(teardownPhase > 0) or generation == handleGeneration + 1)

  val InvScopeLifo =
    scopeDepth >= 0 and scopeDepth <= Bound and
    scopeStack.length() == scopeDepth and
    nextScope >= 1 and nextScope <= Bound + 1 and
    (not(hasClosedScope) or
      (lastClosePreStack.length() == scopeStack.length() + 1 and
       lastClosedScope == lastClosePreStack.nth(lastClosePreStack.length() - 1) and
       scopeStack == lastClosePreStack.slice(0, lastClosePreStack.length() - 1)))

  val InvRefRoot =
    refRoots >= 0 and refRoots <= Bound and
    (refRoots == 0 or teardownPhase == 0)

  val InvAsyncWork =
    asyncPending >= 0 and asyncPending <= Bound and
    (asyncPending == 0 or teardownPhase == 0)

  val InvTsfn =
    tsfnPending >= 0 and tsfnPending <= Bound and
    (tsfnPending == 0 or teardownPhase == 0)

  val InvTeardownOrder =
    teardownPhase >= 0 and teardownPhase <= ClosedPhase and
    previousTeardownPhase >= 0 and previousTeardownPhase <= ClosedPhase and
    teardownPhase >= previousTeardownPhase and
    (teardownPhase == 0 or scopeDepth == 0) and
    (teardownPhase == 0 or (refRoots == 0 and asyncPending == 0 and tsfnPending == 0)) and
    (teardownPhase == 0 or not(callbackAfterTeardown))

  val InvFinalizer =
    finalizerRuns >= 0 and finalizerRuns <= 1 and
    not(finalized and finalizerQueued) and
    (not(finalizerQueued) or (teardownPhase == 1 and finalizerRuns == 0)) and
    (not(finalized) or (teardownPhase == ClosedPhase and finalizerRuns == 1))

  val InvAbiCap =
    requestedAbi >= 1 and requestedAbi <= MaxAbi and
    (not(abiRejected) or requestedAbi == MaxAbi)

  val AllSafety =
    InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvAsyncWork and
    InvTsfn and InvTeardownOrder and InvFinalizer and InvAbiCap

  run napi_boundsTest =
    init
      .then(openScope)
      .then(openScope)
      .then(closeScope)
      .expect(hasClosedScope and scopeDepth == 1 and InvScopeLifo)
      .then(refHandle)
      .then(queueAsyncWork)
      .then(queueTsfn)
      .expect(refRoots == 1 and asyncPending == 1 and tsfnPending == 1 and InvRefRoot and InvAsyncWork and InvTsfn)
      .then(deliverAsyncCallback)
      .then(drainTsfn)
      .then(closeScope)
      .then(unrefHandle)
      .then(8.reps(i => requestAbi))
      .then(rejectUnsupportedAbi)
      .expect(callbackDelivered == 2 and callbackOwner == owner and requestedAbi == MaxAbi and abiRejected and AllSafety)
      .then(teardown)
      .expect(teardownPhase == 1 and generation == handleGeneration + 1 and finalizerQueued and not(finalized) and InvOwnerGeneration and InvTeardownOrder and InvFinalizer)
      .then(rejectPostTeardownCallback)
      .expect(callbackRejected and not(callbackAfterTeardown) and InvTeardownOrder)
      .then(runFinalizer)
      .expect(finalized and finalizerRuns == 1 and teardownPhase == ClosedPhase and InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvAsyncWork and InvTsfn and InvTeardownOrder and InvFinalizer and InvAbiCap)

  run napi_staleHandleScenario =
    init
      .then(queueAsyncWork)
      .then(deliverAsyncCallback)
      .then(teardown)
      .then(rejectPostTeardownCallback)
      .then(runFinalizer)
      .expect(generation == handleGeneration + 1 and callbackRejected and finalized and InvOwnerGeneration and InvTeardownOrder and InvFinalizer)

  run napi_ownerGenerationFaultTest =
    init
      .then(queueAsyncWork)
      .then(deliverAsyncCallback)
      .then(faultOwnerGeneration)
      .expect(not(InvOwnerGeneration) and InvScopeLifo and InvRefRoot and InvAsyncWork and InvTsfn and InvTeardownOrder and InvFinalizer and InvAbiCap)

  run napi_scopeLifoFaultTest =
    init
      .then(openScope)
      .then(openScope)
      .then(faultScopeNonLifo)
      .expect(not(InvScopeLifo) and InvOwnerGeneration and InvRefRoot and InvAsyncWork and InvTsfn and InvTeardownOrder and InvFinalizer and InvAbiCap)

  run napi_refRootFaultTest =
    init
      .then(refHandle)
      .then(faultRefUnderflow)
      .expect(not(InvRefRoot) and InvOwnerGeneration and InvScopeLifo and InvAsyncWork and InvTsfn and InvTeardownOrder and InvFinalizer and InvAbiCap)

  run napi_asyncWorkFaultTest =
    init
      .then(queueAsyncWork)
      .then(faultAsyncOverflow)
      .expect(not(InvAsyncWork) and InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvTsfn and InvTeardownOrder and InvFinalizer and InvAbiCap)

  run napi_tsfnFaultTest =
    init
      .then(queueTsfn)
      .then(faultTsfnOverflow)
      .expect(not(InvTsfn) and InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvAsyncWork and InvTeardownOrder and InvFinalizer and InvAbiCap)

  run napi_teardownOrderFaultTest =
    init
      .then(teardown)
      .then(faultCallbackAfterTeardown)
      .expect(not(InvTeardownOrder) and InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvAsyncWork and InvTsfn and InvFinalizer and InvAbiCap)

  run napi_finalizerFaultTest =
    init
      .then(teardown)
      .then(runFinalizer)
      .then(faultFinalizerDuplicate)
      .expect(not(InvFinalizer) and InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvAsyncWork and InvTsfn and InvTeardownOrder and InvAbiCap)

  run napi_abiCapFaultTest =
    init
      .then(requestAbi)
      .then(faultAbiAboveCap)
      .expect(not(InvAbiCap) and InvOwnerGeneration and InvScopeLifo and InvRefRoot and InvAsyncWork and InvTsfn and InvTeardownOrder and InvFinalizer)
}
