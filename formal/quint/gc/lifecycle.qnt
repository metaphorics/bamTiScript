module lifecycle {
  val Seed = 211
  val Bound = 3
  val Idle = 0
  val CompactReady = 1
  val CompactStop = 2
  val Shutdown = 3

  var gc: {
    phase: int,
    epoch: int,
    readerEpoch: int,
    retireEpoch: int,
    readers: int,
    retired: bool,
    reclaiming: bool,
    compactPage: int,
    compactedPages: int,
    finalizerQueued: bool,
    finalized: bool,
    finalizerCalls: int,
    allocationFailed: bool,
    slotPublished: bool,
    shutdown: bool
  }

  val phase = gc.phase
  val epoch = gc.epoch
  val readerEpoch = gc.readerEpoch
  val retireEpoch = gc.retireEpoch
  val readers = gc.readers
  val retired = gc.retired
  val reclaiming = gc.reclaiming
  val compactPage = gc.compactPage
  val compactedPages = gc.compactedPages
  val finalizerQueued = gc.finalizerQueued
  val finalized = gc.finalized
  val finalizerCalls = gc.finalizerCalls
  val allocationFailed = gc.allocationFailed
  val slotPublished = gc.slotPublished
  val shutdown = gc.shutdown

  pure def initialGc() = {
    phase: Idle,
    epoch: 1,
    readerEpoch: 1,
    retireEpoch: 0,
    readers: 0,
    retired: false,
    reclaiming: false,
    compactPage: 0,
    compactedPages: 0,
    finalizerQueued: false,
    finalized: false,
    finalizerCalls: 0,
    allocationFailed: false,
    slotPublished: false,
    shutdown: false,
  }

  action init = all {
    gc' = initialGc,
  }

  action checkState = all {
    gc' = gc,
  }

  action retire = all {
    not(shutdown) and not(retired),
    gc' = gc.with("retired", true).with("retireEpoch", epoch),
  }

  action enterReader = all {
    retired and not(reclaiming) and readers == 0 and not(shutdown),
    gc' = gc.with("readers", 1).with("readerEpoch", epoch),
  }

  action advanceEpoch = all {
    not(shutdown) and epoch < Bound,
    gc' = gc.with("epoch", epoch + 1),
  }

  action exitReader = all {
    readers == 1,
    gc' = gc.with("readers", 0).with("readerEpoch", epoch),
  }

  action reclaim = all {
    retired and not(reclaiming) and readers == 0 and readerEpoch > retireEpoch and epoch > retireEpoch,
    gc' = gc.with("reclaiming", true),
  }

  action prepareCompact = all {
    phase == Idle and not(shutdown),
    gc' = gc.with("phase", CompactReady),
  }

  action compact = all {
    phase == CompactReady and compactedPages == 0 and compactPage < Bound,
    gc' = gc.with("phase", CompactStop).with("compactPage", compactPage + 1).with("compactedPages", 1),
  }

  action queueFinalizer = all {
    not(shutdown) and not(finalizerQueued) and not(finalized),
    gc' = gc.with("finalizerQueued", true),
  }

  action fireFinalizer = all {
    finalizerQueued and finalizerCalls == 0,
    gc' = gc.with("finalizerQueued", false).with("finalized", true).with("finalizerCalls", 1),
  }

  action beginAllocation = all {
    not(shutdown) and not(slotPublished),
    gc' = gc.with("allocationFailed", false),
  }

  action publishAllocation = all {
    not(shutdown) and not(allocationFailed) and not(slotPublished),
    gc' = gc.with("slotPublished", true),
  }

  action failAlloc = all {
    not(shutdown) and not(slotPublished),
    gc' = gc.with("allocationFailed", true).with("slotPublished", false),
  }

  action seal = all {
    not(shutdown) and not(finalizerQueued),
    gc' = gc.with("phase", Shutdown).with("slotPublished", false).with("shutdown", true),
  }

  action faultReclaimWithReader = all {
    retired and readers == 1 and not(reclaiming),
    gc' = gc.with("reclaiming", true),
  }

  action faultCompactTwoPages = all {
    phase == CompactReady and compactPage < Bound,
    gc' = gc.with("phase", CompactStop).with("compactPage", compactPage + 1).with("compactedPages", 2),
  }

  action faultFireTwice = all {
    finalized and finalizerCalls == 1 and finalizerCalls < Bound,
    gc' = gc.with("finalizerCalls", finalizerCalls + 1),
  }

  action faultPublishFailed = all {
    allocationFailed and not(shutdown) and not(slotPublished),
    gc' = gc.with("slotPublished", true),
  }

  action faultPublishAfterSeal = all {
    shutdown and not(slotPublished),
    gc' = gc.with("slotPublished", true),
  }

  action step = any {
    retire,
    enterReader,
    advanceEpoch,
    exitReader,
    reclaim,
    prepareCompact,
    compact,
    queueFinalizer,
    fireFinalizer,
    beginAllocation,
    publishAllocation,
    failAlloc,
    seal,
  }

  val TypeOK =
    phase >= Idle and phase <= Shutdown and
      epoch >= 1 and epoch <= Bound and
      readerEpoch >= 1 and readerEpoch <= Bound and
      retireEpoch >= 0 and retireEpoch <= Bound and
      readers >= 0 and readers <= 1 and
      compactPage >= 0 and compactPage <= Bound and
      compactedPages >= 0 and compactedPages <= Bound and
      finalizerCalls >= 0 and finalizerCalls <= Bound

  val CompactOnePage = phase != CompactStop or (compactedPages == 1 and compactPage > 0 and compactPage <= Bound)
  val EpochSafeReclaim = not(reclaiming) or (retired and readers == 0 and readerEpoch > retireEpoch and epoch > retireEpoch)
  val FinalizerExactlyOnce =
    finalizerCalls <= 1 and
      (not(finalized) or (finalizerCalls == 1 and not(finalizerQueued))) and
      (not(finalizerQueued) or (finalizerCalls == 0 and not(finalized)))
  val FailedAllocPublishesNothing = not(allocationFailed) or not(slotPublished)
  val ShutdownSealed = not(shutdown) or not(slotPublished)

  run lifecycle_boundsTest = init
    .then(retire)
    .then(enterReader)
    .then(all { checkState, assert(Seed == 211 and TypeOK and readers == 1 and readerEpoch == retireEpoch and EpochSafeReclaim) })
    .then(advanceEpoch)
    .then(exitReader)
    .then(reclaim)
    .then(all { checkState, assert(TypeOK and reclaiming and EpochSafeReclaim) })
    .then(prepareCompact)
    .then(compact)
    .then(all { checkState, assert(TypeOK and phase == CompactStop and CompactOnePage) })
    .then(queueFinalizer)
    .then(fireFinalizer)
    .then(all { checkState, assert(TypeOK and finalized and FinalizerExactlyOnce) })
    .then(beginAllocation)
    .then(failAlloc)
    .then(all { checkState, assert(TypeOK and allocationFailed and FailedAllocPublishesNothing) })
    .then(seal)
    .then(all { checkState, assert(TypeOK and shutdown and ShutdownSealed) })

  run compactOnePage_normalWitness = init
    .then(prepareCompact)
    .then(compact)
    .then(all { checkState, assert(TypeOK and phase == CompactStop and compactedPages == 1 and CompactOnePage) })

  run epochSafeReclaim_normalWitness = init
    .then(retire)
    .then(enterReader)
    .then(advanceEpoch)
    .then(exitReader)
    .then(reclaim)
    .then(all { checkState, assert(TypeOK and reclaiming and EpochSafeReclaim) })

  run finalizerExactlyOnce_normalWitness = init
    .then(queueFinalizer)
    .then(fireFinalizer)
    .then(all { checkState, assert(TypeOK and finalized and finalizerCalls == 1 and FinalizerExactlyOnce) })

  run failedAllocPublishesNothing_normalWitness = init
    .then(beginAllocation)
    .then(failAlloc)
    .then(all { checkState, assert(TypeOK and allocationFailed and not(slotPublished) and FailedAllocPublishesNothing) })

  run shutdownSealed_normalWitness = init
    .then(beginAllocation)
    .then(publishAllocation)
    .then(seal)
    .then(all { checkState, assert(TypeOK and shutdown and not(slotPublished) and ShutdownSealed) })

  run faultCompactOnePage = init
    .then(prepareCompact)
    .then(faultCompactTwoPages)
    .then(all { checkState, assert(TypeOK and not(CompactOnePage) and EpochSafeReclaim and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed) })

  run faultEpochSafeReclaim = init
    .then(retire)
    .then(enterReader)
    .then(faultReclaimWithReader)
    .then(all { checkState, assert(TypeOK and CompactOnePage and not(EpochSafeReclaim) and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed) })

  run faultFinalizerExactlyOnce = init
    .then(queueFinalizer)
    .then(fireFinalizer)
    .then(faultFireTwice)
    .then(all { checkState, assert(TypeOK and CompactOnePage and EpochSafeReclaim and not(FinalizerExactlyOnce) and FailedAllocPublishesNothing and ShutdownSealed) })

  run faultFailedAllocPublishesNothing = init
    .then(beginAllocation)
    .then(failAlloc)
    .then(faultPublishFailed)
    .then(all { checkState, assert(TypeOK and CompactOnePage and EpochSafeReclaim and FinalizerExactlyOnce and not(FailedAllocPublishesNothing) and ShutdownSealed) })

  run faultShutdownSealed = init
    .then(beginAllocation)
    .then(publishAllocation)
    .then(seal)
    .then(faultPublishAfterSeal)
    .then(all { checkState, assert(TypeOK and CompactOnePage and EpochSafeReclaim and FinalizerExactlyOnce and FailedAllocPublishesNothing and not(ShutdownSealed)) })
}
