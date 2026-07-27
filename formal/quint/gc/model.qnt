module model {
  val Seed = 223
  val Bound = 3
  val Quantum = 1
  val Idle = 0
  val Mark = 1
  val Close = 2
  val Compact = 3
  val Shutdown = 4

  var gc: {
    phase: int,
    live: bool,
    generation: int,
    handleGeneration: int,
    marked: bool,
    ticketEpoch: int,
    markEpoch: int,
    outstandingTickets: int,
    issuedTickets: int,
    settledTickets: int,
    oldToYoung: bool,
    remembered: bool,
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
    shutdown: bool,
    deficitA: int,
    deficitB: int,
    queueA: int,
    queueB: int,
    drrRound: int,
    servedA: int,
    servedB: int
  }

  val phase = gc.phase
  val live = gc.live
  val generation = gc.generation
  val handleGeneration = gc.handleGeneration
  val marked = gc.marked
  val ticketEpoch = gc.ticketEpoch
  val markEpoch = gc.markEpoch
  val outstandingTickets = gc.outstandingTickets
  val issuedTickets = gc.issuedTickets
  val settledTickets = gc.settledTickets
  val oldToYoung = gc.oldToYoung
  val remembered = gc.remembered
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
  val deficitA = gc.deficitA
  val deficitB = gc.deficitB
  val queueA = gc.queueA
  val queueB = gc.queueB
  val drrRound = gc.drrRound
  val servedA = gc.servedA
  val servedB = gc.servedB

  pure def initialGc() = {
    phase: Idle,
    live: false,
    generation: 0,
    handleGeneration: 0,
    marked: false,
    ticketEpoch: 0,
    markEpoch: 0,
    outstandingTickets: 0,
    issuedTickets: 0,
    settledTickets: 0,
    oldToYoung: false,
    remembered: false,
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
    deficitA: 0,
    deficitB: 0,
    queueA: 0,
    queueB: 0,
    drrRound: 0,
    servedA: 0,
    servedB: 0,
  }

  action init = all {
    gc' = initialGc,
  }

  action checkState = all {
    gc' = gc,
  }

  action allocate = all {
    phase == Idle and not(retired) and not(shutdown) and not(live) and generation < Bound,
    gc' = gc.with("live", true).with("generation", generation + 1).with("handleGeneration", generation + 1),
  }

  action startMark = all {
    phase == Idle and not(shutdown) and markEpoch < Bound,
    gc' = gc.with("phase", Mark).with("markEpoch", markEpoch + 1),
  }

  action rememberOldToYoung = all {
    phase == Mark and live and not(oldToYoung),
    gc' = gc.with("oldToYoung", true).with("remembered", true),
  }

  action shade = all {
    phase == Mark and live and outstandingTickets < Bound and issuedTickets < Bound,
    gc' = gc.with("ticketEpoch", markEpoch)
      .with("issuedTickets", issuedTickets + 1)
      .with("outstandingTickets", outstandingTickets + 1)
      .with("marked", true),
  }

  action settleTicket = all {
    phase == Mark and outstandingTickets > 0 and settledTickets < issuedTickets,
    gc' = gc.with("settledTickets", settledTickets + 1).with("outstandingTickets", outstandingTickets - 1),
  }

  action closeMark = all {
    phase == Mark and outstandingTickets == 0 and (not(live) or marked),
    gc' = gc.with("phase", Close),
  }

  action retire = all {
    live and not(shutdown) and generation < Bound,
    gc' = gc.with("live", false).with("generation", generation + 1).with("retired", true).with("retireEpoch", epoch),
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

  action compact = all {
    phase == Close and compactedPages == 0 and compactPage < Bound,
    gc' = gc.with("phase", Compact).with("compactPage", compactPage + 1).with("compactedPages", 1),
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

  action enqueueDrr = all {
    not(shutdown) and queueA < Bound and queueB < Bound,
    gc' = gc.with("queueA", queueA + 1).with("queueB", queueB + 1),
  }

  action startDrrRound = all {
    not(shutdown) and queueA > 0 and queueB > 0 and deficitA == 0 and deficitB == 0 and drrRound < Bound,
    gc' = gc.with("deficitA", Quantum).with("deficitB", Quantum).with("drrRound", drrRound + 1),
  }

  action serveA = all {
    not(shutdown) and queueA > 0 and deficitA > 0 and servedA < Bound,
    gc' = gc.with("queueA", queueA - 1).with("deficitA", deficitA - 1).with("servedA", servedA + 1),
  }

  action serveB = all {
    not(shutdown) and queueB > 0 and deficitB > 0 and servedB < Bound,
    gc' = gc.with("queueB", queueB - 1).with("deficitB", deficitB - 1).with("servedB", servedB + 1),
  }

  action seal = all {
    not(shutdown) and not(finalizerQueued) and outstandingTickets == 0 and generation < Bound,
    gc' = gc.with("phase", Shutdown)
      .with("live", false)
      .with("generation", generation + 1)
      .with("slotPublished", false)
      .with("shutdown", true),
  }

  action faultStaleHandle = all {
    live and handleGeneration < Bound,
    gc' = gc.with("handleGeneration", handleGeneration + 1),
  }

  action faultDuplicateTicket = all {
    phase == Mark and outstandingTickets > 0 and outstandingTickets < Bound,
    gc' = gc.with("outstandingTickets", outstandingTickets + 1),
  }

  action faultCloseWithOutstandingTicket = all {
    phase == Mark and outstandingTickets > 0,
    gc' = gc.with("phase", Close),
  }

  action faultReclaimLiveAtClose = all {
    phase == Close and live and marked and not(reclaiming),
    gc' = gc.with("retired", true).with("reclaiming", true),
  }

  action faultForgetOldToYoung = all {
    oldToYoung and remembered,
    gc' = gc.with("remembered", false),
  }

  action faultReclaimWithReader = all {
    retired and readers == 1 and not(reclaiming),
    gc' = gc.with("reclaiming", true),
  }

  action faultCompactTwoPages = all {
    phase == Close and compactedPages == 0 and compactPage < Bound,
    gc' = gc.with("phase", Compact).with("compactPage", compactPage + 1).with("compactedPages", 2),
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

  action faultDrrOverService = all {
    drrRound > 0 and servedA < Bound - 1,
    gc' = gc.with("servedA", drrRound + 1),
  }

  action step = any {
    allocate,
    startMark,
    rememberOldToYoung,
    shade,
    settleTicket,
    closeMark,
    retire,
    enterReader,
    advanceEpoch,
    exitReader,
    reclaim,
    compact,
    queueFinalizer,
    fireFinalizer,
    beginAllocation,
    publishAllocation,
    failAlloc,
    enqueueDrr,
    startDrrRound,
    serveA,
    serveB,
    seal,
  }

  val TypeOK =
    phase >= Idle and phase <= Shutdown and
      generation >= 0 and generation <= Bound and
      handleGeneration >= 0 and handleGeneration <= Bound and
      markEpoch >= 0 and markEpoch <= Bound and
      ticketEpoch >= 0 and ticketEpoch <= Bound and
      outstandingTickets >= 0 and outstandingTickets <= Bound and
      issuedTickets >= 0 and issuedTickets <= Bound and
      settledTickets >= 0 and settledTickets <= Bound and
      epoch >= 1 and epoch <= Bound and
      readerEpoch >= 1 and readerEpoch <= Bound and
      retireEpoch >= 0 and retireEpoch <= Bound and
      readers >= 0 and readers <= 1 and
      compactPage >= 0 and compactPage <= Bound and
      compactedPages >= 0 and compactedPages <= Bound and
      finalizerCalls >= 0 and finalizerCalls <= Bound and
      deficitA >= 0 and deficitA <= Bound and
      deficitB >= 0 and deficitB <= Bound and
      queueA >= 0 and queueA <= Bound and
      queueB >= 0 and queueB <= Bound and
      drrRound >= 0 and drrRound <= Bound and
      servedA >= 0 and servedA <= Bound and
      servedB >= 0 and servedB <= Bound

  val ReachableResolves = not(live) or generation == handleGeneration
  val ShadeTicketExact =
    outstandingTickets == issuedTickets - settledTickets and
      settledTickets <= issuedTickets and
      (outstandingTickets == 0 or (ticketEpoch == markEpoch and marked))
  val CloseQuiescent = phase != Close or outstandingTickets == 0
  val NoFalseReclamationAtClose = phase != Close or not(live) or (marked and not(reclaiming))
  val RememberedSound = not(oldToYoung) or remembered
  val EpochSafeReclaim = not(reclaiming) or (retired and readers == 0 and readerEpoch > retireEpoch and epoch > retireEpoch)
  val CompactOnePage = phase != Compact or (compactedPages == 1 and compactPage > 0 and compactPage <= Bound)
  val FinalizerExactlyOnce =
    finalizerCalls <= 1 and
      (not(finalized) or (finalizerCalls == 1 and not(finalizerQueued))) and
      (not(finalizerQueued) or (finalizerCalls == 0 and not(finalized)))
  val FailedAllocPublishesNothing = not(allocationFailed) or not(slotPublished)
  val ShutdownSealed = not(shutdown) or not(slotPublished)
  val DrrRoundBound =
    deficitA >= 0 and deficitA <= Quantum and
      deficitB >= 0 and deficitB <= Quantum and
      servedA <= drrRound and servedB <= drrRound and
      (drrRound == 0 or queueA == 0 or deficitA + servedA >= drrRound) and
      (drrRound == 0 or queueB == 0 or deficitB + servedB >= drrRound)

  run gc_boundsTest = init
    .then(allocate)
    .then(startMark)
    .then(rememberOldToYoung)
    .then(shade)
    .then(all { checkState, assert(Seed == 223 and TypeOK and live and oldToYoung and outstandingTickets == 1 and ReachableResolves and ShadeTicketExact and RememberedSound) })
    .then(settleTicket)
    .then(closeMark)
    .then(all { checkState, assert(TypeOK and phase == Close and CloseQuiescent and NoFalseReclamationAtClose) })
    .then(retire)
    .then(enterReader)
    .then(all { checkState, assert(TypeOK and readers == 1 and readerEpoch == retireEpoch and EpochSafeReclaim) })
    .then(advanceEpoch)
    .then(exitReader)
    .then(reclaim)
    .then(all { checkState, assert(TypeOK and reclaiming and EpochSafeReclaim) })
    .then(compact)
    .then(all { checkState, assert(TypeOK and phase == Compact and CompactOnePage) })
    .then(queueFinalizer)
    .then(fireFinalizer)
    .then(all { checkState, assert(TypeOK and finalized and finalizerCalls == 1 and FinalizerExactlyOnce) })
    .then(beginAllocation)
    .then(failAlloc)
    .then(all { checkState, assert(TypeOK and allocationFailed and not(slotPublished) and FailedAllocPublishesNothing) })
    .then(enqueueDrr)
    .then(startDrrRound)
    .then(all { checkState, assert(TypeOK and deficitA == Quantum and deficitB == Quantum and DrrRoundBound) })
    .then(serveA)
    .then(serveB)
    .then(seal)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run reachableResolves_normalWitness = init
    .then(allocate)
    .then(all { checkState, assert(TypeOK and live and ReachableResolves) })

  run shadeTicketExact_normalWitness = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(all { checkState, assert(TypeOK and outstandingTickets > 0 and ShadeTicketExact) })

  run closeQuiescent_normalWitness = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(all { checkState, assert(TypeOK and phase == Close and CloseQuiescent) })

  run noFalseReclamationAtClose_normalWitness = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(all { checkState, assert(TypeOK and phase == Close and live and marked and NoFalseReclamationAtClose) })

  run rememberedSound_normalWitness = init
    .then(allocate)
    .then(startMark)
    .then(rememberOldToYoung)
    .then(all { checkState, assert(TypeOK and oldToYoung and remembered and RememberedSound) })

  run epochSafeReclaim_normalWitness = init
    .then(allocate)
    .then(retire)
    .then(enterReader)
    .then(advanceEpoch)
    .then(exitReader)
    .then(reclaim)
    .then(all { checkState, assert(TypeOK and reclaiming and EpochSafeReclaim) })

  run compactOnePage_normalWitness = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(compact)
    .then(all { checkState, assert(TypeOK and phase == Compact and compactedPages == 1 and CompactOnePage) })

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

  run drrRoundBound_normalWitness = init
    .then(enqueueDrr)
    .then(startDrrRound)
    .then(all { checkState, assert(TypeOK and drrRound == 1 and deficitA > 0 and deficitB > 0 and DrrRoundBound) })
    .then(serveA)
    .then(serveB)
    .then(all { checkState, assert(TypeOK and DrrRoundBound) })

  run faultReachableResolves = init
    .then(allocate)
    .then(faultStaleHandle)
    .then(all { checkState, assert(TypeOK and not(ReachableResolves) and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultShadeTicketExact = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(faultDuplicateTicket)
    .then(all { checkState, assert(TypeOK and ReachableResolves and not(ShadeTicketExact) and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultCloseQuiescent = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(faultCloseWithOutstandingTicket)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and not(CloseQuiescent) and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultNoFalseReclamationAtClose = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(faultReclaimLiveAtClose)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and not(NoFalseReclamationAtClose) and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultRememberedSound = init
    .then(allocate)
    .then(startMark)
    .then(rememberOldToYoung)
    .then(faultForgetOldToYoung)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and not(RememberedSound) and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultEpochSafeReclaim = init
    .then(allocate)
    .then(retire)
    .then(enterReader)
    .then(faultReclaimWithReader)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and not(EpochSafeReclaim) and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultCompactOnePage = init
    .then(allocate)
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(faultCompactTwoPages)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and not(CompactOnePage) and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultFinalizerExactlyOnce = init
    .then(queueFinalizer)
    .then(fireFinalizer)
    .then(faultFireTwice)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and not(FinalizerExactlyOnce) and FailedAllocPublishesNothing and ShutdownSealed and DrrRoundBound) })

  run faultFailedAllocPublishesNothing = init
    .then(beginAllocation)
    .then(failAlloc)
    .then(faultPublishFailed)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and not(FailedAllocPublishesNothing) and ShutdownSealed and DrrRoundBound) })

  run faultShutdownSealed = init
    .then(beginAllocation)
    .then(publishAllocation)
    .then(seal)
    .then(faultPublishAfterSeal)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and not(ShutdownSealed) and DrrRoundBound) })

  run faultDrrRoundBound = init
    .then(enqueueDrr)
    .then(startDrrRound)
    .then(faultDrrOverService)
    .then(all { checkState, assert(TypeOK and ReachableResolves and ShadeTicketExact and CloseQuiescent and NoFalseReclamationAtClose and RememberedSound and EpochSafeReclaim and CompactOnePage and FinalizerExactlyOnce and FailedAllocPublishesNothing and ShutdownSealed and not(DrrRoundBound)) })
}
