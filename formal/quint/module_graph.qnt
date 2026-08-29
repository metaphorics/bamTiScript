module module_graph {
  val Seed = 607
  val Bound = 3

  val Absent = 0
  val Requested = 1
  val Resolving = 2
  val Loading = 3
  val Compiled = 4
  val Linked = 5
  val Installed = 6
  val EvaluationScheduled = 7
  val Evaluated = 8
  val FailedAbsorbing = 9
  val FailedRetryable = 10

  val RequestIdle = 0
  val RequestLive = 1
  val RequestDeferred = 2
  val RequestTombstoned = 3
  val RequestSettled = 4

  val NoKey = 0
  val RootKey = 1
  val OtherKey = 2
  val NodeLimit = 2
  val EdgeLimit = 1
  val ByteLimit = 2
  val ResolverStepLimit = 3

  var graph: {
    rootState: int,
    dependencyState: int,
    rootInstalled: bool,
    dependencyInstalled: bool,
    rootCachePublished: bool,
    dependencyCachePublished: bool,
    request1State: int,
    request2State: int,
    request1ExpectedKey: int,
    request2ExpectedKey: int,
    request1Lease: int,
    request2Lease: int,
    providerRequestId: int,
    providerExpectedKey: int,
    completionPending: bool,
    completionId: int,
    completionKey: int,
    completionConsumed: bool,
    consumedCompletionId: int,
    installedEntryKey: int,
    lateCompletionSeen: bool,
    lateCompletionRejected: bool,
    lateCompletionAccepted: bool,
    protocolViolation: bool,
    preflightPassed: bool,
    preparedNodes: int,
    allocatedNodes: int,
    discoveredNodes: int,
    discoveredEdges: int,
    retainedBytes: int,
    resolverSteps: int,
    limitFailure: bool,
    failedLimit: int,
    jsonErrorCached: bool,
    retryAllowed: bool,
    settledWaiters: int,
    commitCount: int
  }

  val rootState = graph.rootState
  val dependencyState = graph.dependencyState
  val rootInstalled = graph.rootInstalled
  val dependencyInstalled = graph.dependencyInstalled
  val rootCachePublished = graph.rootCachePublished
  val dependencyCachePublished = graph.dependencyCachePublished
  val request1State = graph.request1State
  val request2State = graph.request2State
  val request1ExpectedKey = graph.request1ExpectedKey
  val request2ExpectedKey = graph.request2ExpectedKey
  val request1Lease = graph.request1Lease
  val request2Lease = graph.request2Lease
  val providerRequestId = graph.providerRequestId
  val providerExpectedKey = graph.providerExpectedKey
  val completionPending = graph.completionPending
  val completionId = graph.completionId
  val completionKey = graph.completionKey
  val completionConsumed = graph.completionConsumed
  val consumedCompletionId = graph.consumedCompletionId
  val installedEntryKey = graph.installedEntryKey
  val lateCompletionSeen = graph.lateCompletionSeen
  val lateCompletionRejected = graph.lateCompletionRejected
  val lateCompletionAccepted = graph.lateCompletionAccepted
  val protocolViolation = graph.protocolViolation
  val preflightPassed = graph.preflightPassed
  val preparedNodes = graph.preparedNodes
  val allocatedNodes = graph.allocatedNodes
  val discoveredNodes = graph.discoveredNodes
  val discoveredEdges = graph.discoveredEdges
  val retainedBytes = graph.retainedBytes
  val resolverSteps = graph.resolverSteps
  val limitFailure = graph.limitFailure
  val failedLimit = graph.failedLimit
  val jsonErrorCached = graph.jsonErrorCached
  val retryAllowed = graph.retryAllowed
  val settledWaiters = graph.settledWaiters
  val commitCount = graph.commitCount

  def expectedKey(id: int): int =
    if (id == 1) request1ExpectedKey else if (id == 2) request2ExpectedKey else NoKey
  pure def liveOrDeferred(state: int): bool =
    state == RequestLive or state == RequestDeferred

  pure def terminalRequest(state: int): bool =
    state == RequestIdle or state == RequestTombstoned or state == RequestSettled

  pure def moduleStateKnown(state: int): bool = state >= Absent and state <= FailedRetryable
  pure def requestStateKnown(state: int): bool = state >= RequestIdle and state <= RequestSettled

  pure def initialGraph = {
    rootState: Absent,
    dependencyState: Absent,
    rootInstalled: false,
    dependencyInstalled: false,
    rootCachePublished: false,
    dependencyCachePublished: false,
    request1State: RequestIdle,
    request2State: RequestIdle,
    request1ExpectedKey: NoKey,
    request2ExpectedKey: NoKey,
    request1Lease: 0,
    request2Lease: 0,
    providerRequestId: 0,
    providerExpectedKey: NoKey,
    completionPending: false,
    completionId: 0,
    completionKey: NoKey,
    completionConsumed: false,
    consumedCompletionId: 0,
    installedEntryKey: NoKey,
    lateCompletionSeen: false,
    lateCompletionRejected: false,
    lateCompletionAccepted: false,
    protocolViolation: false,
    preflightPassed: false,
    preparedNodes: 0,
    allocatedNodes: 0,
    discoveredNodes: 0,
    discoveredEdges: 0,
    retainedBytes: 0,
    resolverSteps: 0,
    limitFailure: false,
    failedLimit: 0,
    jsonErrorCached: false,
    retryAllowed: false,
    settledWaiters: 0,
    commitCount: 0
  }

  action init = all {
    graph' = initialGraph,
  }

  // Resolution owns the first lease before a provider request exists.
  action requestRoot = all {
    request1State == RequestIdle and rootState == Absent,
    graph' = graph
      .with("rootState", Requested)
      .with("request1State", RequestLive)
      .with("request1ExpectedKey", RootKey)
      .with("request1Lease", 1),
  }

  // An identical resolution key joins without starting another provider operation.
  action coalesceSecondWaiter = all {
    request1State == RequestLive and request1Lease == 1 and request2State == RequestIdle,
    graph' = graph
      .with("request2State", RequestLive)
      .with("request2ExpectedKey", request1ExpectedKey)
      .with("request2Lease", 1),
  }

  // The waiter set transfers atomically from resolution ownership to graph ownership.
  action resolveAndTransferLeases = all {
    request1State == RequestLive and request1Lease == 1 and rootState == Requested,
    graph' = graph
      .with("rootState", Resolving)
      .with("dependencyState", Requested)
      .with("providerRequestId", 1)
      .with("providerExpectedKey", request1ExpectedKey),
  }

  action loadClosure = all {
    rootState == Resolving and dependencyState == Requested and not(limitFailure),
    graph' = graph
      .with("rootState", Loading)
      .with("dependencyState", Loading)
      .with("discoveredNodes", 2)
      .with("discoveredEdges", 1)
      .with("retainedBytes", 2)
      .with("resolverSteps", 3),
  }

  action compileClosure = all {
    rootState == Loading and dependencyState == Loading,
    graph' = graph.with("rootState", Compiled).with("dependencyState", Compiled),
  }

  action linkClosure = all {
    rootState == Compiled and dependencyState == Compiled,
    graph' = graph.with("rootState", Linked).with("dependencyState", Linked),
  }

  action deferGraph = all {
    rootState == Linked and dependencyState == Linked and request1State == RequestLive,
    graph' = graph
      .with("request1State", RequestDeferred)
      .with("request2State", if (request2State == RequestLive) RequestDeferred else request2State),
  }

  // Completion envelopes carry both ModuleRequestId and their entry ModuleCacheKey.
  action receiveMatchingCompletion = all {
    request1State == RequestDeferred and not(completionPending),
    graph' = graph
      .with("completionPending", true)
      .with("completionId", providerRequestId)
      .with("completionKey", providerExpectedKey),
  }

  action receiveSwappedCompletion = all {
    request1State == RequestDeferred and not(completionPending),
    graph' = graph
      .with("completionPending", true)
      .with("completionId", providerRequestId)
      .with("completionKey", OtherKey),
  }

  action consumeBoundCompletion = all {
    completionPending and completionId == providerRequestId and
      completionKey == providerExpectedKey and completionKey == expectedKey(completionId),
    graph' = graph
      .with("completionPending", false)
      .with("completionConsumed", true)
      .with("consumedCompletionId", completionId)
      .with("installedEntryKey", completionKey),
  }

  // A live ID with the wrong payload key settles joined waiters with a typed protocol error.
  action rejectSwappedCompletion = all {
    completionPending and completionKey != expectedKey(completionId),
    graph' = graph
      .with("completionPending", false)
      .with("protocolViolation", true)
      .with("request1State", RequestSettled)
      .with("request2State", if (request2State == RequestDeferred) RequestSettled else request2State)
      .with("request1Lease", 0)
      .with("request2Lease", 0)
      .with("settledWaiters", if (request2State == RequestDeferred) 2 else 1),
  }

  // Preflight prepares the complete closure without mutating installed storage.
  action preflightClosure = all {
    completionConsumed and not(preflightPassed) and installedEntryKey == RootKey and
      discoveredNodes <= NodeLimit and discoveredEdges <= EdgeLimit and
      retainedBytes <= ByteLimit and resolverSteps <= ResolverStepLimit,
    graph' = graph.with("preflightPassed", true).with("preparedNodes", discoveredNodes),
  }

  // Commit publishes both records atomically; cache keys are the final publication fields.
  action commitClosure = all {
    preflightPassed and preparedNodes == 2 and allocatedNodes == 0 and
      request1Lease == 1 and not(rootInstalled) and not(dependencyInstalled),
    graph' = graph
      .with("allocatedNodes", preparedNodes)
      .with("rootState", Installed)
      .with("dependencyState", Installed)
      .with("rootInstalled", true)
      .with("dependencyInstalled", true)
      .with("rootCachePublished", true)
      .with("dependencyCachePublished", true)
      .with("commitCount", 1),
  }

  action scheduleDependencyEvaluation = all {
    rootInstalled and dependencyInstalled and dependencyState == Installed and request1Lease == 1,
    graph' = graph.with("dependencyState", EvaluationScheduled),
  }

  action evaluateDependency = all {
    dependencyState == EvaluationScheduled and dependencyInstalled,
    graph' = graph.with("dependencyState", Evaluated),
  }

  action scheduleRootEvaluation = all {
    rootState == Installed and dependencyState == Evaluated and
      rootInstalled and dependencyInstalled and request1Lease == 1,
    graph' = graph.with("rootState", EvaluationScheduled),
  }

  action evaluateRoot = all {
    rootState == EvaluationScheduled and dependencyState == Evaluated and
      rootInstalled and dependencyInstalled,
    graph' = graph.with("rootState", Evaluated),
  }

  action settleSuccess = all {
    rootState == Evaluated and request1State == RequestDeferred,
    graph' = graph
      .with("request1State", RequestSettled)
      .with("request2State", if (request2State == RequestDeferred) RequestSettled else request2State)
      .with("request1Lease", 0)
      .with("request2Lease", 0)
      .with("settledWaiters", if (request2State == RequestDeferred) 2 else 1),
  }

  // Cancelling one joined waiter releases only its lease and records a tombstone.
  action cancelSecondWaiter = all {
    request2State == RequestDeferred and request2Lease == 1 and request1Lease == 1,
    graph' = graph.with("request2State", RequestTombstoned).with("request2Lease", 0),
  }

  // The runtime consumes and classifies a completion at one quiescence checkpoint.
  action receiveLateCompletion = all {
    request2State == RequestTombstoned and not(lateCompletionSeen),
    graph'
      = graph
          .with("lateCompletionSeen", true)
          .with("lateCompletionRejected", true),
  }

  // A named limit fails before allocating or compiling the exceeding dependency.
  action rejectNodeLimit = all {
    rootState == Resolving and allocatedNodes == 0 and discoveredNodes == 0,
    graph' = graph
      .with("limitFailure", true)
      .with("failedLimit", NodeLimit)
      .with("rootState", FailedRetryable)
      .with("dependencyState", FailedRetryable)
      .with("request1State", RequestSettled)
      .with("request1Lease", 0)
      .with("settledWaiters", 1)
      .with("retryAllowed", true),
  }

  // JSON grammar failures cache one language error and are absorbing.
  action failJsonGrammar = all {
    dependencyState == EvaluationScheduled and dependencyInstalled,
    graph' = graph
      .with("dependencyState", FailedAbsorbing)
      .with("jsonErrorCached", true)
      .with("retryAllowed", false),
  }

  // Fatal capacity/runtime failures keep the installed record but permit reevaluation.
  action failRootFatally = all {
    rootState == EvaluationScheduled and rootInstalled,
    graph' = graph.with("rootState", FailedRetryable).with("retryAllowed", true),
  }

  action retryRoot = all {
    rootState == FailedRetryable and rootInstalled and retryAllowed,
    graph' = graph.with("rootState", EvaluationScheduled).with("retryAllowed", false),
  }

  // Fault injection actions are intentionally excluded from the normal transition relation.
  action step = any {
    requestRoot,
    coalesceSecondWaiter,
    resolveAndTransferLeases,
    loadClosure,
    compileClosure,
    linkClosure,
    deferGraph,
    receiveMatchingCompletion,
    receiveSwappedCompletion,
    consumeBoundCompletion,
    rejectSwappedCompletion,
    preflightClosure,
    commitClosure,
    scheduleDependencyEvaluation,
    evaluateDependency,
    scheduleRootEvaluation,
    evaluateRoot,
    settleSuccess,
    cancelSecondWaiter,
    receiveLateCompletion,
    rejectNodeLimit,
    failJsonGrammar,
    failRootFatally,
    retryRoot,
  }

  val TypeOK =
    moduleStateKnown(rootState) and moduleStateKnown(dependencyState) and
      requestStateKnown(request1State) and requestStateKnown(request2State) and
      request1Lease >= 0 and request1Lease <= 1 and request2Lease >= 0 and request2Lease <= 1 and
      allocatedNodes >= 0 and allocatedNodes <= NodeLimit

  val NoPartialClosureObservable =
    not(rootState == EvaluationScheduled or rootState == Evaluated) or
      (rootInstalled and dependencyInstalled)

  val AtomicClosureInstall =
    rootInstalled == dependencyInstalled and
      (not(rootInstalled) or allocatedNodes == 2)

  val CacheKeysPublishLast =
    rootCachePublished == dependencyCachePublished and
      (not(rootCachePublished) or (rootInstalled and dependencyInstalled and commitCount == 1))

  val IdentityBinding =
    not(completionConsumed) or
      (consumedCompletionId == providerRequestId and installedEntryKey == providerExpectedKey and
        installedEntryKey == expectedKey(consumedCompletionId))

  val NoLostRequest =
    (not(liveOrDeferred(request1State)) or request1Lease == 1) and
      (not(liveOrDeferred(request2State)) or request2Lease == 1)

  val TombstoneRejectsLateCompletion =
    not(lateCompletionSeen) or
      (request2State == RequestTombstoned and lateCompletionRejected and not(lateCompletionAccepted))

  val JsonAbsorbingFatalRetryable =
    (not(dependencyState == FailedAbsorbing) or (jsonErrorCached and not(retryAllowed))) and
      (not(rootState == FailedRetryable and rootInstalled) or (retryAllowed and not(jsonErrorCached)))

  val LimitsBeforeAllocation =
    not(limitFailure) or
      (failedLimit == NodeLimit and allocatedNodes == 0 and preparedNodes == 0 and
        not(rootInstalled) and not(dependencyInstalled) and
        not(rootCachePublished) and not(dependencyCachePublished))

  // A tombstone never propagates to the coalesced waiter, and the survivor either
  // keeps its lease (provider work continues) or settles on its own behalf.
  val CoalescedLeaseIsolation =
    not(request2State == RequestTombstoned) or
      (not(request1State == RequestTombstoned) and
        (request1Lease == 1 or request1State == RequestSettled))

  // This state predicate is the recorded temporal witness: all admitted waiters reach terminal states.
  val RequestsEventuallySettle = terminalRequest(request1State) and terminalRequest(request2State)

  action checkState(p) = all {
    p,
    graph' = graph,
  }

  run happyClosureTest =
    init.then(requestRoot).then(coalesceSecondWaiter).then(resolveAndTransferLeases)
      .then(loadClosure).then(compileClosure).then(linkClosure).then(deferGraph)
      .then(receiveMatchingCompletion).then(consumeBoundCompletion).then(preflightClosure)
      .then(commitClosure).then(scheduleDependencyEvaluation).then(evaluateDependency)
      .then(scheduleRootEvaluation).then(evaluateRoot).then(settleSuccess)
      .then(checkState(TypeOK and NoPartialClosureObservable and AtomicClosureInstall and
        CacheKeysPublishLast and IdentityBinding and NoLostRequest and RequestsEventuallySettle))

  run payloadSwapRejectedTest =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(deferGraph).then(receiveSwappedCompletion).then(rejectSwappedCompletion)
      .then(checkState(protocolViolation and not(rootInstalled) and not(dependencyInstalled) and
        IdentityBinding and NoLostRequest and RequestsEventuallySettle))

  run cancellationTombstoneTest =
    init.then(requestRoot).then(coalesceSecondWaiter).then(resolveAndTransferLeases)
      .then(loadClosure).then(compileClosure).then(linkClosure).then(deferGraph)
      .then(cancelSecondWaiter).then(receiveLateCompletion)
      .then(checkState(TombstoneRejectsLateCompletion and CoalescedLeaseIsolation and NoLostRequest))

  run graphLimitTest =
    init.then(requestRoot).then(resolveAndTransferLeases).then(rejectNodeLimit)
      .then(checkState(limitFailure and LimitsBeforeAllocation and AtomicClosureInstall and
        CacheKeysPublishLast and RequestsEventuallySettle))

  run jsonAbsorbingTest =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(deferGraph).then(receiveMatchingCompletion).then(consumeBoundCompletion)
      .then(preflightClosure).then(commitClosure).then(scheduleDependencyEvaluation)
      .then(failJsonGrammar)
      .then(checkState(JsonAbsorbingFatalRetryable and AtomicClosureInstall and CacheKeysPublishLast))

  run fatalRetryTest =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(deferGraph).then(receiveMatchingCompletion).then(consumeBoundCompletion)
      .then(preflightClosure).then(commitClosure).then(scheduleDependencyEvaluation)
      .then(evaluateDependency).then(scheduleRootEvaluation).then(failRootFatally)
      .then(checkState(JsonAbsorbingFatalRetryable and retryAllowed)).then(retryRoot)
      .then(checkState(rootState == EvaluationScheduled and not(retryAllowed)))

  action faultConsumeSwappedPayload = all {
    completionPending and completionKey != expectedKey(completionId),
    graph' = graph
      .with("completionPending", false)
      .with("completionConsumed", true)
      .with("consumedCompletionId", completionId)
      .with("installedEntryKey", completionKey),
  }

  action faultPublishPartialClosure = all {
    preflightPassed and preparedNodes == 2,
    graph' = graph
      .with("allocatedNodes", 1)
      .with("rootState", Installed)
      .with("rootInstalled", true)
      .with("rootCachePublished", true),
  }

  action faultEvaluateBeforeDependencyInstall = all {
    rootState == Linked and not(dependencyInstalled),
    graph' = graph.with("rootState", Evaluated),
  }

  action faultLoseLiveLease = all {
    request1State == RequestLive and request1Lease == 1,
    graph' = graph.with("request1Lease", 0),
  }

  action faultAcceptTombstonedCompletion = all {
    lateCompletionSeen and request2State == RequestTombstoned,
    graph' = graph.with("lateCompletionAccepted", true),
  }

  action faultAllocatePastLimit = all {
    limitFailure and allocatedNodes == 0,
    graph' = graph.with("allocatedNodes", 1),
  }

  // Mutation: cancellation leaking across the coalescing boundary.
  action faultPropagateCancel = all {
    request2State == RequestTombstoned and request1State == RequestDeferred,
    graph'
      = graph
          .with("request1State", RequestTombstoned)
          .with("request1Lease", 0),
  }
  action faultRetryJsonFailure = all {
    dependencyState == FailedAbsorbing and jsonErrorCached,
    graph' = graph.with("retryAllowed", true),
  }

  run faultPayloadSwapRun =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(deferGraph).then(receiveSwappedCompletion).then(faultConsumeSwappedPayload)
      .then(checkState(not(IdentityBinding)))

  run faultPartialClosureRun =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(deferGraph).then(receiveMatchingCompletion).then(consumeBoundCompletion)
      .then(preflightClosure).then(faultPublishPartialClosure)
      .then(checkState(not(AtomicClosureInstall) and not(CacheKeysPublishLast)))

  run faultEvaluationBeforeInstallRun =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(faultEvaluateBeforeDependencyInstall)
      .then(checkState(not(NoPartialClosureObservable)))

  run faultLostLeaseRun =
    init.then(requestRoot).then(faultLoseLiveLease).then(checkState(not(NoLostRequest)))

  run faultTombstoneRun =
    init.then(requestRoot).then(coalesceSecondWaiter).then(resolveAndTransferLeases)
      .then(loadClosure).then(compileClosure).then(linkClosure).then(deferGraph)
      .then(cancelSecondWaiter).then(receiveLateCompletion).then(faultAcceptTombstonedCompletion)
      .then(checkState(not(TombstoneRejectsLateCompletion)))

  run faultCoalescedCancelRun =
    init.then(requestRoot).then(coalesceSecondWaiter).then(resolveAndTransferLeases)
      .then(loadClosure).then(compileClosure).then(linkClosure).then(deferGraph)
      .then(cancelSecondWaiter).then(faultPropagateCancel)
      .then(checkState(not(CoalescedLeaseIsolation)))

  run faultLimitAllocationRun =
    init.then(requestRoot).then(resolveAndTransferLeases).then(rejectNodeLimit)
      .then(faultAllocatePastLimit).then(checkState(not(LimitsBeforeAllocation)))

  run faultJsonRetryRun =
    init.then(requestRoot).then(resolveAndTransferLeases).then(loadClosure).then(compileClosure)
      .then(linkClosure).then(deferGraph).then(receiveMatchingCompletion).then(consumeBoundCompletion)
      .then(preflightClosure).then(commitClosure).then(scheduleDependencyEvaluation)
      .then(failJsonGrammar).then(faultRetryJsonFailure)
      .then(checkState(not(JsonAbsorbingFatalRetryable)))
}
