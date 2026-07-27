module jit_lifecycle {
  val Seed = 307
  val Bound = 3

  var queued: int
  var readyTokens: int
  var compilerOwners: int
  var compilerOwnerId: int
  var arenaSlots: int
  var finalized: bool
  var published: bool
  var writable: bool
  var executable: bool
  var cancelled: bool
  var snapshotGeneration: int
  var entryGeneration: int
  var retiredEpoch: int
  var readerEpoch: int
  var readers: int
  var rejected: bool
  var failed: bool
  var interpreterEntry: bool
  var osrOrdinal: int
  var entryCount: int
  var resumeOrdinal: int
  var stableResume: int
  var retired: bool
  var reclaimed: bool

  action init = all {
    queued' = 0,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = false,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = false,
    snapshotGeneration' = 1,
    entryGeneration' = 1,
    retiredEpoch' = 0,
    readerEpoch' = 1,
    readers' = 0,
    rejected' = false,
    failed' = false,
    interpreterEntry' = true,
    osrOrdinal' = 0,
    entryCount' = 1,
    resumeOrdinal' = 0,
    stableResume' = 0,
    retired' = false,
    reclaimed' = false,
  }

  // A fresh request may enlarge the verified entry table before compilation.
  action queue = all {
    queued < Bound and compilerOwners == 0 and not(published) and arenaSlots == 0,
    readers == 0 and (not(retired) or reclaimed),
    queued' = queued + 1,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = false,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = false,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = false,
    interpreterEntry' = true,
    osrOrdinal' = if (entryCount < Bound) entryCount else entryCount - 1,
    entryCount' = if (entryCount < Bound) entryCount + 1 else entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  // A generator records an existing nonzero entry ordinal once, before it resumes.
  action captureResume = all {
    entryCount > 1 and osrOrdinal > 0 and osrOrdinal < entryCount,
    resumeOrdinal == 0 and stableResume == 0,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = osrOrdinal,
    stableResume' = osrOrdinal,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  // Dispatch may leave the saved resume point and later return to it.
  action chooseEntryZero = all {
    entryCount > 1 and osrOrdinal > 0 and resumeOrdinal > 0,
    resumeOrdinal == stableResume,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = 0,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action resumeAtSavedOrdinal = all {
    entryCount > 1 and osrOrdinal == 0 and resumeOrdinal > 0,
    resumeOrdinal == stableResume and resumeOrdinal < entryCount,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = resumeOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  // Claiming consumes one request, allocates one slot, and captures a new generation.
  action claimCompiler = all {
    queued > 0 and compilerOwners == 0 and compilerOwnerId == 0,
    not(published) and arenaSlots == 0 and not(retired) and readers == 0,
    not(cancelled) and not(rejected) and not(failed),
    queued' = queued - 1,
    readyTokens' = 0,
    compilerOwners' = 1,
    compilerOwnerId' = if (entryGeneration == 1) 1 else 2,
    arenaSlots' = 1,
    finalized' = false,
    published' = false,
    writable' = true,
    executable' = false,
    cancelled' = false,
    snapshotGeneration' = entryGeneration + 1,
    entryGeneration' = entryGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = false,
    interpreterEntry' = true,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  // This is the only normal W-to-X publication transition.
  action finalizePublish = all {
    compilerOwners == 1 and compilerOwnerId > 0 and arenaSlots == 1,
    readyTokens == 0 and writable and not(executable) and not(finalized),
    not(published) and not(cancelled) and not(rejected) and not(failed),
    queued' = queued,
    readyTokens' = 1,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 1,
    finalized' = true,
    published' = true,
    writable' = false,
    executable' = true,
    cancelled' = false,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = snapshotGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = false,
    interpreterEntry' = false,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  // Rejection occurs before a compiler owns a slot and keeps the interpreter live.
  action reject = all {
    queued > 0 and compilerOwners == 0 and compilerOwnerId == 0,
    not(published) and arenaSlots == 0 and not(retired),
    queued' = queued - 1,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = false,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = true,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = true,
    failed' = false,
    interpreterEntry' = true,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  action cancelCompile = all {
    compilerOwners == 1 and compilerOwnerId > 0 and arenaSlots == 1,
    writable and not(published) and not(retired),
    queued' = queued,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = false,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = true,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = false,
    interpreterEntry' = true,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  action failCompile = all {
    compilerOwners == 1 and compilerOwnerId > 0 and arenaSlots == 1,
    writable and not(published) and not(retired),
    queued' = queued,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = false,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = false,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = true,
    interpreterEntry' = true,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  action beginRead = all {
    published and readyTokens == 1 and executable and not(writable),
    not(retired) and readers < Bound,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers + 1,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action endRead = all {
    readers > 0,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = if (readers == 1) readerEpoch + 1 else readerEpoch,
    readers' = readers - 1,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  // Retirement withdraws the ready token but holds its slot until an epoch observes quiescence.
  action retire = all {
    published and readyTokens == 1 and executable and not(retired),
    compilerOwners == 0 and compilerOwnerId == 0 and arenaSlots == 1,
    queued' = queued,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 1,
    finalized' = true,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = false,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = readerEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = false,
    failed' = false,
    interpreterEntry' = true,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = true,
    reclaimed' = false,
  }

  action observeQuiescence = all {
    retired and not(reclaimed) and readers == 0 and readerEpoch == retiredEpoch,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch + 1,
    readers' = 0,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action reclaim = all {
    retired and not(reclaimed) and readers == 0 and readerEpoch > retiredEpoch,
    compilerOwners == 0 and compilerOwnerId == 0 and readyTokens == 0,
    queued' = queued,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = finalized,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = true,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = true,
    reclaimed' = true,
  }

  // Fault injection remains outside this normal transition relation.
  action step = any {
    queue,
    captureResume,
    chooseEntryZero,
    resumeAtSavedOrdinal,
    claimCompiler,
    finalizePublish,
    reject,
    cancelCompile,
    failCompile,
    beginRead,
    endRead,
    retire,
    observeQuiescence,
    reclaim,
  }

  val retirementSlot = if (retired and not(reclaimed)) 1 else 0

  val QueueBound = queued >= 0 and queued <= Bound
  val OneReadyToken =
    readyTokens >= 0 and readyTokens <= 1 and published == (readyTokens == 1)
  val SingleCompilerOwner =
    compilerOwners >= 0 and compilerOwners <= 1 and
      ((compilerOwners == 0 and compilerOwnerId == 0) or
       (compilerOwners == 1 and compilerOwnerId > 0 and compilerOwnerId <= 2))
  val SlotConservation =
    arenaSlots >= 0 and arenaSlots <= Bound and
      arenaSlots == compilerOwners + readyTokens + retirementSlot
  val NoPublishBeforeFinalize = not(published) or finalized
  val WxBeforePublish = not(published) or (executable and not(writable))
  val CancelledNeverLive =
    not(cancelled) or (not(published) and readyTokens == 0 and interpreterEntry)
  val EntryGenerationMatchesSnapshot =
    not(published) or (entryGeneration == snapshotGeneration and entryGeneration > 0)
  val NoReclaimBeforeQuiescence =
    not(reclaimed) or
      (retired and readers == 0 and readerEpoch > retiredEpoch and arenaSlots == 0)
  val FallbackOnEveryRejectOrFailure =
    not(rejected or failed) or
      (interpreterEntry and not(published) and readyTokens == 0)
  val OsrOrdinalInRange =
    entryCount > 0 and osrOrdinal >= 0 and osrOrdinal < entryCount
  val ResumeOrdinalStable =
    resumeOrdinal >= 0 and stableResume >= 0 and
      resumeOrdinal == stableResume and resumeOrdinal < entryCount

  val AllSafety =
    QueueBound and OneReadyToken and SingleCompilerOwner and SlotConservation and
      NoPublishBeforeFinalize and WxBeforePublish and CancelledNeverLive and
      EntryGenerationMatchesSnapshot and NoReclaimBeforeQuiescence and
      FallbackOnEveryRejectOrFailure and OsrOrdinalInRange and ResumeOrdinalStable

  // Quint's run composition requires the check to frame the same state as a transition.
  action checkState(p) = all {
    p,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  // Each normal witness reaches the nontrivial side of its named property.
  run jit_boundsTest =
    init.then(3.reps(i => queue)).then(checkState(queued == Bound and QueueBound))

  run readyTokenNormalWitness =
    init.then(queue).then(claimCompiler).then(finalizePublish)
      .then(checkState(published and readyTokens == 1 and OneReadyToken))

  run singleCompilerOwnerNormalWitness =
    init.then(queue).then(claimCompiler)
      .then(checkState(compilerOwners == 1 and compilerOwnerId == 1 and SingleCompilerOwner))
      .then(finalizePublish).then(retire).then(observeQuiescence).then(reclaim)
      .then(queue).then(claimCompiler)
      .then(checkState(compilerOwners == 1 and compilerOwnerId == 2 and SingleCompilerOwner))

  run slotConservationNormalWitness =
    init.then(queue).then(claimCompiler)
      .then(checkState(arenaSlots == 1 and compilerOwners == 1 and SlotConservation))
      .then(finalizePublish)
      .then(checkState(arenaSlots == 1 and readyTokens == 1 and SlotConservation))

  run finalizeBeforePublishNormalWitness =
    init.then(queue).then(claimCompiler)
      .then(checkState(not(finalized) and not(published) and writable))
      .then(finalizePublish)
      .then(checkState(published and finalized and NoPublishBeforeFinalize))

  run wxBeforePublishNormalWitness =
    init.then(queue).then(claimCompiler)
      .then(checkState(writable and not(executable) and not(published)))
      .then(finalizePublish)
      .then(checkState(published and executable and not(writable) and WxBeforePublish))

  run cancelledNeverLiveNormalWitness =
    init.then(queue).then(claimCompiler).then(cancelCompile)
      .then(checkState(cancelled and not(published) and readyTokens == 0 and
        interpreterEntry and CancelledNeverLive))

  run generationSnapshotNormalWitness =
    init.then(queue).then(claimCompiler)
      .then(checkState(snapshotGeneration == entryGeneration + 1))
      .then(finalizePublish)
      .then(checkState(published and entryGeneration == snapshotGeneration and
        EntryGenerationMatchesSnapshot))

  run reclaimAfterQuiescenceNormalWitness =
    init.then(queue).then(claimCompiler).then(finalizePublish).then(beginRead).then(retire)
      .then(checkState(retired and readers == 1 and not(reclaimed) and arenaSlots == 1))
      .then(endRead).then(reclaim)
      .then(checkState(reclaimed and readers == 0 and readerEpoch > retiredEpoch and
        arenaSlots == 0 and NoReclaimBeforeQuiescence))

  run fallbackNormalWitness =
    init.then(queue).then(reject)
      .then(checkState(rejected and interpreterEntry and FallbackOnEveryRejectOrFailure))
      .then(queue).then(claimCompiler).then(failCompile)
      .then(checkState(failed and interpreterEntry and FallbackOnEveryRejectOrFailure))

  run osrOrdinalNormalWitness =
    init.then(queue)
      .then(checkState(entryCount == 2 and osrOrdinal == 1 and OsrOrdinalInRange))

  run resumeOrdinalNormalWitness =
    init.then(queue).then(captureResume).then(chooseEntryZero).then(resumeAtSavedOrdinal)
      .then(checkState(resumeOrdinal > 0 and resumeOrdinal == stableResume and
        osrOrdinal == resumeOrdinal and ResumeOrdinalStable))

  run normalLifecycleScenario =
    init.then(queue).then(captureResume).then(claimCompiler).then(finalizePublish)
      .then(beginRead).then(retire).then(endRead).then(reclaim)
      .then(checkState(AllSafety and retired and reclaimed and arenaSlots == 0 and
        resumeOrdinal == stableResume))

  // Each fault action violates exactly one named invariant after a normal setup.
  action faultQueueBound = all {
    queued == Bound,
    queued' = Bound + 1,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultOneReadyToken = all {
    published and readyTokens == 1 and arenaSlots == 1,
    queued' = queued,
    readyTokens' = 2,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = 2,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultSingleCompilerOwner = all {
    compilerOwners == 1 and arenaSlots == 1,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = 2,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = 2,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultSlotConservation = all {
    compilerOwners == 1 and arenaSlots == 1,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = 0,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultNoPublishBeforeFinalize = all {
    compilerOwners == 1 and arenaSlots == 1 and writable,
    not(finalized) and not(published),
    queued' = queued,
    readyTokens' = 1,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 1,
    finalized' = false,
    published' = true,
    writable' = false,
    executable' = true,
    cancelled' = false,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = snapshotGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = false,
    interpreterEntry' = false,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  action faultWxBeforePublish = all {
    published and finalized and executable and not(writable),
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = true,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultCancelledNeverLive = all {
    published and readyTokens == 1,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = true,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultEntryGenerationMismatch = all {
    published and entryGeneration == snapshotGeneration,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = snapshotGeneration + 1,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultReclaimBeforeQuiescence = all {
    retired and not(reclaimed) and readers > 0 and arenaSlots == 1,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = 0,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = true,
  }

  action faultFallback = all {
    compilerOwners == 1 and arenaSlots == 1 and writable,
    queued' = queued,
    readyTokens' = 0,
    compilerOwners' = 0,
    compilerOwnerId' = 0,
    arenaSlots' = 0,
    finalized' = false,
    published' = false,
    writable' = false,
    executable' = false,
    cancelled' = false,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = 0,
    readerEpoch' = readerEpoch,
    readers' = 0,
    rejected' = false,
    failed' = true,
    interpreterEntry' = false,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = false,
    reclaimed' = false,
  }

  action faultOsrOrdinalOutOfRange = all {
    entryCount > 1,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = entryCount,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  action faultResumeOrdinalUnstable = all {
    resumeOrdinal > 0 and resumeOrdinal == stableResume,
    queued' = queued,
    readyTokens' = readyTokens,
    compilerOwners' = compilerOwners,
    compilerOwnerId' = compilerOwnerId,
    arenaSlots' = arenaSlots,
    finalized' = finalized,
    published' = published,
    writable' = writable,
    executable' = executable,
    cancelled' = cancelled,
    snapshotGeneration' = snapshotGeneration,
    entryGeneration' = entryGeneration,
    retiredEpoch' = retiredEpoch,
    readerEpoch' = readerEpoch,
    readers' = readers,
    rejected' = rejected,
    failed' = failed,
    interpreterEntry' = interpreterEntry,
    osrOrdinal' = osrOrdinal,
    entryCount' = entryCount,
    resumeOrdinal' = resumeOrdinal,
    stableResume' = stableResume + 1,
    retired' = retired,
    reclaimed' = reclaimed,
  }

  run faultQueueBoundRun =
    init.then(3.reps(i => queue)).then(faultQueueBound)
      .then(checkState(not(QueueBound) and readyTokens >= 0 and arenaSlots >= 0))

  run faultOneReadyTokenRun =
    init.then(queue).then(claimCompiler).then(finalizePublish).then(faultOneReadyToken)
      .then(checkState(not(OneReadyToken) and QueueBound and SlotConservation))

  run faultSingleCompilerOwnerRun =
    init.then(queue).then(claimCompiler).then(faultSingleCompilerOwner)
      .then(checkState(not(SingleCompilerOwner) and QueueBound and SlotConservation))

  run faultSlotConservationRun =
    init.then(queue).then(claimCompiler).then(faultSlotConservation)
      .then(checkState(not(SlotConservation) and QueueBound and SingleCompilerOwner))

  run faultNoPublishBeforeFinalizeRun =
    init.then(queue).then(claimCompiler).then(faultNoPublishBeforeFinalize)
      .then(checkState(not(NoPublishBeforeFinalize) and OneReadyToken and SlotConservation and
        WxBeforePublish))

  run faultWxBeforePublishRun =
    init.then(queue).then(claimCompiler).then(finalizePublish).then(faultWxBeforePublish)
      .then(checkState(not(WxBeforePublish) and NoPublishBeforeFinalize and OneReadyToken and
        SlotConservation))

  run faultCancelledNeverLiveRun =
    init.then(queue).then(claimCompiler).then(finalizePublish).then(faultCancelledNeverLive)
      .then(checkState(not(CancelledNeverLive) and OneReadyToken and SlotConservation and
        NoPublishBeforeFinalize))

  run faultEntryGenerationMismatchRun =
    init.then(queue).then(claimCompiler).then(finalizePublish).then(faultEntryGenerationMismatch)
      .then(checkState(not(EntryGenerationMatchesSnapshot) and OneReadyToken and SlotConservation and
        NoPublishBeforeFinalize))

  run faultReclaimBeforeQuiescenceRun =
    init.then(queue).then(claimCompiler).then(finalizePublish).then(beginRead).then(retire)
      .then(faultReclaimBeforeQuiescence)
      .then(checkState(not(NoReclaimBeforeQuiescence) and QueueBound and SlotConservation))

  run faultFallbackRun =
    init.then(queue).then(claimCompiler).then(faultFallback)
      .then(checkState(not(FallbackOnEveryRejectOrFailure) and QueueBound and SlotConservation and
        CancelledNeverLive))

  run faultOsrOrdinalOutOfRangeRun =
    init.then(queue).then(faultOsrOrdinalOutOfRange)
      .then(checkState(not(OsrOrdinalInRange) and ResumeOrdinalStable and QueueBound))

  run faultResumeOrdinalUnstableRun =
    init.then(queue).then(captureResume).then(faultResumeOrdinalUnstable)
      .then(checkState(not(ResumeOrdinalStable) and OsrOrdinalInRange and QueueBound))
}
