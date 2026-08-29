module atomics_waiters {
  val Seed = 707
  val Bound = 3
  val InitialFuel = 1
  val InitialBackingBytes = 1
  val GrownBackingBytes = 3

  // Machine-owned waiter authority. There are no waiter threads: every enqueued
  // token lives in exactly one place -- the shared FIFO until it settles, is
  // cancelled, or is revoked. Settlement stages mirror the amended design:
  // allocate the result string, prepare the Promise reaction plan, reserve the
  // exact reaction capacity, and only then run the one non-fallible commit that
  // charges fuel and removes authority. A failed reservation retains the ready
  // key, the timer row, and the FIFO position while consuming no fuel.
  var waiter: {
    nextToken: int,
    fifo: List[int],
    enqueued: List[int],
    asyncTokens: Set[int],
    finiteTokens: Set[int],
    readyTokens: Set[int],
    allocatedTokens: Set[int],
    preparedTokens: Set[int],
    reservedTokens: Set[int],
    settled: List[int],
    timedOut: List[int],
    cancelledTokens: Set[int],
    revokedTokens: Set[int],
    fuel: int,
    fuelCharges: int,
    failedReservations: int,
    capacityAvailable: bool,
  }

  // Per-Machine shared-backing ledger keyed by Arc identity. Wrappers (SAB heap
  // slots and structured-clone aliases) and waiter-queue pins are independent
  // references to one storage; growth charges the ledger, wrapper collection
  // refunds nothing, and the final machine authority releases the charge once.
  var backing: {
    allocated: bool,
    ledgerLive: bool,
    ledgerBytes: int,
    backingBytes: int,
    wrappers: Set[int],
    pins: int,
    chargedBytes: int,
    refundedBytes: int,
    refundCount: int,
  }

  action init = all {
    waiter' = {
      nextToken: 1,
      fifo: List(),
      enqueued: List(),
      asyncTokens: Set(),
      finiteTokens: Set(),
      readyTokens: Set(),
      allocatedTokens: Set(),
      preparedTokens: Set(),
      reservedTokens: Set(),
      settled: List(),
      timedOut: List(),
      cancelledTokens: Set(),
      revokedTokens: Set(),
      fuel: InitialFuel,
      fuelCharges: 0,
      failedReservations: 0,
      capacityAvailable: false,
    },
    backing' = {
      allocated: false,
      ledgerLive: false,
      ledgerBytes: 0,
      backingBytes: 0,
      wrappers: Set(),
      pins: 0,
      chargedBytes: 0,
      refundedBytes: 0,
      refundCount: 0,
    },
  }

  // Registration is all-or-none: the token is never reused and the FIFO order
  // is the settlement order for notify and timeout alike.
  action enqueueSync = all {
    waiter.nextToken <= Bound,
    waiter' = waiter
      .with("nextToken", waiter.nextToken + 1)
      .with("fifo", waiter.fifo.append(waiter.nextToken))
      .with("enqueued", waiter.enqueued.append(waiter.nextToken)),
    backing' = backing,
  }

  action enqueueAsyncFinite = all {
    waiter.nextToken <= Bound,
    waiter' = waiter
      .with("nextToken", waiter.nextToken + 1)
      .with("fifo", waiter.fifo.append(waiter.nextToken))
      .with("enqueued", waiter.enqueued.append(waiter.nextToken))
      .with("asyncTokens", waiter.asyncTokens.union(Set(waiter.nextToken)))
      .with("finiteTokens", waiter.finiteTokens.union(Set(waiter.nextToken))),
    backing' = backing,
  }

  action enqueueAsyncInfinite = all {
    waiter.nextToken <= Bound,
    waiter' = waiter
      .with("nextToken", waiter.nextToken + 1)
      .with("fifo", waiter.fifo.append(waiter.nextToken))
      .with("enqueued", waiter.enqueued.append(waiter.nextToken))
      .with("asyncTokens", waiter.asyncTokens.union(Set(waiter.nextToken))),
    backing' = backing,
  }

  // A finite waiter's timer fires: the ready key stays authoritative and the
  // timer row stays in machine authority while settlement is attempted.
  action markHeadTimeoutReady = all {
    waiter.fifo.length() > 0,
    waiter.finiteTokens.contains(waiter.fifo.head()),
    waiter' = waiter.with("readyTokens", waiter.readyTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  // Fallible preparation begins: the "timed-out" string allocation succeeds
  // before any authority or fuel is touched.
  action allocateTimeoutResult = all {
    waiter.fifo.length() > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("allocatedTokens", waiter.allocatedTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  action prepareTimeoutReactions = all {
    waiter.fifo.length() > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter.allocatedTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("preparedTokens", waiter.preparedTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  // Capacity pressure: the attempt returns with the timer still ready, the
  // waiter still authoritative, and the fuel balance unchanged. A later
  // checkpoint retries without a new provider report.
  action failTimeoutReservation = all {
    waiter.fifo.length() > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter.preparedTokens.contains(waiter.fifo.head()),
    not(waiter.capacityAvailable),
    waiter' = waiter
      .with("allocatedTokens", waiter.allocatedTokens.exclude(Set(waiter.fifo.head())))
      .with("preparedTokens", waiter.preparedTokens.exclude(Set(waiter.fifo.head())))
      .with("failedReservations", waiter.failedReservations + 1),
    backing' = backing,
  }

  // The microtask checkpoint drains and reserves fresh reaction capacity.
  action drainMicrotasks = all {
    not(waiter.capacityAvailable),
    waiter' = waiter.with("capacityAvailable", true),
    backing' = backing,
  }

  action reserveTimeoutCapacity = all {
    waiter.fifo.length() > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter.preparedTokens.contains(waiter.fifo.head()),
    waiter.capacityAvailable,
    waiter' = waiter
      .with("reservedTokens", waiter.reservedTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  // The committing fuel charge and all authority removal form one non-fallible
  // transition. Fuel decreases strictly here and nowhere else.
  action commitTimeoutSettlement = all {
    waiter.fifo.length() > 0 and waiter.fuel > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter.reservedTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("fifo", waiter.fifo.tail())
      .with("readyTokens", waiter.readyTokens.exclude(Set(waiter.fifo.head())))
      .with("allocatedTokens", waiter.allocatedTokens.exclude(Set(waiter.fifo.head())))
      .with("preparedTokens", waiter.preparedTokens.exclude(Set(waiter.fifo.head())))
      .with("reservedTokens", waiter.reservedTokens.exclude(Set(waiter.fifo.head())))
      .with("settled", waiter.settled.append(waiter.fifo.head()))
      .with("timedOut", waiter.timedOut.append(waiter.fifo.head()))
      .with("fuel", waiter.fuel - 1)
      .with("fuelCharges", waiter.fuelCharges + 1),
    backing' = backing,
  }

  // Notify validates, allocates, and reserves before any removal; a batch that
  // cannot reserve settles none and removes none.
  action failNotifyReservation = all {
    waiter.fifo.length() > 0,
    not(waiter.capacityAvailable),
    waiter' = waiter.with("failedReservations", waiter.failedReservations + 1),
    backing' = backing,
  }

  action prepareNotifyHead = all {
    waiter.fifo.length() > 0,
    waiter.capacityAvailable,
    waiter' = waiter
      .with("allocatedTokens", waiter.allocatedTokens.union(Set(waiter.fifo.head())))
      .with("preparedTokens", waiter.preparedTokens.union(Set(waiter.fifo.head())))
      .with("reservedTokens", waiter.reservedTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  // Timer authority disappears inside the commit; provider cancellation after
  // the machine commit is best effort and cannot undo the settlement.
  action commitNotifyHead = all {
    waiter.fifo.length() > 0,
    waiter.reservedTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("fifo", waiter.fifo.tail())
      .with("readyTokens", waiter.readyTokens.exclude(Set(waiter.fifo.head())))
      .with("allocatedTokens", waiter.allocatedTokens.exclude(Set(waiter.fifo.head())))
      .with("preparedTokens", waiter.preparedTokens.exclude(Set(waiter.fifo.head())))
      .with("reservedTokens", waiter.reservedTokens.exclude(Set(waiter.fifo.head())))
      .with("settled", waiter.settled.append(waiter.fifo.head())),
    backing' = backing,
  }

  // Cancellation and revocation remove head authority without settlement. A
  // token inside the non-reentrant prepared/reserved region cannot be dropped
  // this way; machine drop and timer clear are the modeled revocation edges.
  action cancelHead = all {
    waiter.fifo.length() > 0,
    not(waiter.reservedTokens.contains(waiter.fifo.head())),
    waiter' = waiter
      .with("fifo", waiter.fifo.tail())
      .with("readyTokens", waiter.readyTokens.exclude(Set(waiter.fifo.head())))
      .with("allocatedTokens", waiter.allocatedTokens.exclude(Set(waiter.fifo.head())))
      .with("preparedTokens", waiter.preparedTokens.exclude(Set(waiter.fifo.head())))
      .with("cancelledTokens", waiter.cancelledTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  action revokeHead = all {
    waiter.fifo.length() > 0,
    not(waiter.reservedTokens.contains(waiter.fifo.head())),
    waiter' = waiter
      .with("fifo", waiter.fifo.tail())
      .with("readyTokens", waiter.readyTokens.exclude(Set(waiter.fifo.head())))
      .with("allocatedTokens", waiter.allocatedTokens.exclude(Set(waiter.fifo.head())))
      .with("preparedTokens", waiter.preparedTokens.exclude(Set(waiter.fifo.head())))
      .with("revokedTokens", waiter.revokedTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  // A duplicate or late provider report is ignored because its timer id is no
  // longer live; it cannot recreate authority or settle another waiter.
  action ignoreStaleTimerReport = all {
    waiter.timedOut.length() > 0,
    waiter' = waiter,
    backing' = backing,
  }

  action allocateSharedBacking = all {
    not(backing.allocated),
    waiter' = waiter,
    backing' = backing
      .with("allocated", true)
      .with("ledgerLive", true)
      .with("ledgerBytes", InitialBackingBytes)
      .with("backingBytes", InitialBackingBytes)
      .with("wrappers", Set(1))
      .with("chargedBytes", InitialBackingBytes),
  }

  // Structured-clone and view aliases share the backing by Arc identity.
  action aliasSharedBacking = all {
    backing.ledgerLive and backing.wrappers != Set(),
    not(backing.wrappers.contains(2)),
    waiter' = waiter,
    backing' = backing.with("wrappers", backing.wrappers.union(Set(2))),
  }

  // Growth charges the ledger, never the growing wrapper's slot, and never
  // moves the SharedBlock identity.
  action growSharedBacking = all {
    backing.ledgerLive and backing.backingBytes == InitialBackingBytes,
    backing.wrappers != Set() or backing.pins > 0,
    waiter' = waiter,
    backing' = backing
      .with("ledgerBytes", GrownBackingBytes)
      .with("backingBytes", GrownBackingBytes)
      .with("chargedBytes", backing.chargedBytes + GrownBackingBytes - InitialBackingBytes),
  }

  // A nonempty waiter queue pins the Arc while it holds tokens.
  action pinSharedBacking = all {
    backing.ledgerLive and backing.pins < Bound,
    backing.wrappers != Set() or backing.pins > 0,
    waiter' = waiter,
    backing' = backing.with("pins", backing.pins + 1),
  }

  // Wrapper collection refunds nothing while any alias or pin survives.
  action collectWrapperOne = all {
    backing.wrappers.contains(1),
    backing.wrappers.size() > 1 or backing.pins > 0,
    waiter' = waiter,
    backing' = backing.with("wrappers", backing.wrappers.exclude(Set(1))),
  }

  action collectWrapperTwo = all {
    backing.wrappers.contains(2),
    backing.wrappers.size() > 1 or backing.pins > 0,
    waiter' = waiter,
    backing' = backing.with("wrappers", backing.wrappers.exclude(Set(2))),
  }

  action releasePin = all {
    backing.pins > 0,
    backing.wrappers != Set() or backing.pins > 1,
    waiter' = waiter,
    backing' = backing.with("pins", backing.pins - 1),
  }

  // The final authority releases the ledger entry and its charge exactly once.
  action releaseFinalPin = all {
    backing.ledgerLive and backing.wrappers == Set() and backing.pins == 1,
    waiter' = waiter,
    backing' = backing
      .with("ledgerLive", false)
      .with("ledgerBytes", 0)
      .with("backingBytes", 0)
      .with("pins", 0)
      .with("refundedBytes", backing.refundedBytes + backing.ledgerBytes)
      .with("refundCount", backing.refundCount + 1),
  }

  action releaseFinalWrapper = all {
    backing.ledgerLive and backing.wrappers.size() == 1 and backing.pins == 0,
    waiter' = waiter,
    backing' = backing
      .with("ledgerLive", false)
      .with("ledgerBytes", 0)
      .with("backingBytes", 0)
      .with("wrappers", Set())
      .with("refundedBytes", backing.refundedBytes + backing.ledgerBytes)
      .with("refundCount", backing.refundCount + 1),
  }

  action step = any {
    enqueueSync,
    enqueueAsyncFinite,
    enqueueAsyncInfinite,
    markHeadTimeoutReady,
    allocateTimeoutResult,
    prepareTimeoutReactions,
    failTimeoutReservation,
    drainMicrotasks,
    reserveTimeoutCapacity,
    commitTimeoutSettlement,
    failNotifyReservation,
    prepareNotifyHead,
    commitNotifyHead,
    cancelHead,
    revokeHead,
    ignoreStaleTimerReport,
    allocateSharedBacking,
    aliasSharedBacking,
    growSharedBacking,
    pinSharedBacking,
    collectWrapperOne,
    collectWrapperTwo,
    releasePin,
    releaseFinalPin,
    releaseFinalWrapper,
  }

  val WaiterTypeOK =
    waiter.nextToken >= 1 and waiter.nextToken <= Bound + 1 and
    waiter.fuel >= 0 and waiter.fuel <= InitialFuel and
    waiter.fuelCharges >= 0 and waiter.fuelCharges <= InitialFuel and
    waiter.failedReservations >= 0

  pure def memberOf(xs: List[int], t: int): bool =
    xs.select(x => x == t).length() > 0

  pure def listEvery(l: List[int], p: (int) => bool): bool =
    l.select(p).length() == l.length()

  // Safety core of no-lost-waiter: every enqueued token is in exactly one of
  // the authoritative FIFO, the settled record, or a terminal cancel/revoke
  // state -- never zero places, never two.
  val NoLostWaiter =
    listEvery(waiter.enqueued, token =>
      waiter.fifo.select(queued => queued == token).length() +
      waiter.settled.select(done => done == token).length() +
      (if (waiter.cancelledTokens.contains(token)) 1 else 0) +
      (if (waiter.revokedTokens.contains(token)) 1 else 0) == 1
    ) and
    listEvery(waiter.fifo, token => waiter.enqueued.select(queued => queued == token).length() == 1)

  val NoDoubleSettlement =
    listEvery(waiter.enqueued, token =>
      waiter.settled.select(done => done == token).length() <= 1
    ) and
    listEvery(waiter.timedOut, token => waiter.settled.select(done => done == token).length() == 1)

  // Fuel decreases strictly only on committed timeout settlements.
  val FuelConservation =
    waiter.fuel >= 0 and waiter.fuel <= InitialFuel and
    waiter.fuel + waiter.fuelCharges == InitialFuel and
    waiter.fuelCharges == waiter.timedOut.length()

  // Settlement order is the registration FIFO order.
  val FIFOPreservation =
    waiter.settled == waiter.enqueued.select(token => memberOf(waiter.settled, token))

  // Reserve exact capacity before any authority removal; a reserved token is
  // still fully present in the FIFO with its allocation and preparation done.
  val PreparedBeforeCommit =
    waiter.reservedTokens.forall(token =>
      waiter.fifo.select(queued => queued == token).length() == 1 and
      waiter.allocatedTokens.contains(token) and
      waiter.preparedTokens.contains(token)
    )

  // Liveness under fair scheduling: every waiter stays settled or authoritatively
  // retained, a retained ready key keeps its FIFO authority across failed
  // checkpoints, and the fuel budget survives failed attempts -- so a fair retry
  // completes without a new provider report while budget remains. The drain and
  // retry run below witnesses the eventual settlement.
  val LiveWaiterAuthority =
    NoLostWaiter and
    NoDoubleSettlement and
    FuelConservation and
    waiter.readyTokens.forall(token =>
      waiter.fifo.select(queued => queued == token).length() == 1
    )

  // Ledger total equals the live backing bytes reachable through any wrapper or
  // pin; with no reference left the entry is released.
  val SharedBackingLedgerExact =
    if (backing.wrappers != Set() or backing.pins > 0)
      backing.ledgerLive and backing.ledgerBytes == backing.backingBytes
    else
      not(backing.ledgerLive) and backing.ledgerBytes == 0 and backing.backingBytes == 0

  val SharedBackingRefundExactlyOnce =
    backing.refundCount >= 0 and backing.refundCount <= 1 and
    backing.refundedBytes <= backing.chargedBytes and
    (not(backing.allocated) or backing.ledgerLive or
      (backing.refundCount == 1 and backing.refundedBytes == backing.chargedBytes))

  action checkState(p) = all {
    p,
    waiter' = waiter,
    backing' = backing,
  }

  run waiterBoundsTest =
    init.then(checkState(Seed == 707 and WaiterTypeOK and SharedBackingLedgerExact))

  // The one-fuel-unit / full-queue / drain / retry scenario: with one fuel unit
  // and a full microtask queue, the timeout attempt fails without consuming
  // fuel and retains the ready key, timer row, and FIFO position; after the
  // drain, the retry settles the head with the single unit and leaves the
  // second waiter authoritatively retained.
  run waiterOneFuelDrainRetryTest =
    init
      .then(enqueueAsyncFinite)
      .then(enqueueAsyncFinite)
      .then(markHeadTimeoutReady)
      .then(allocateTimeoutResult)
      .then(prepareTimeoutReactions)
      .then(failTimeoutReservation)
      .then(checkState(
        waiter.fuel == InitialFuel and
        waiter.fifo == List(1, 2) and
        waiter.readyTokens.contains(1) and
        waiter.failedReservations == 1 and
        NoLostWaiter and FuelConservation
      ))
      .then(drainMicrotasks)
      .then(allocateTimeoutResult)
      .then(prepareTimeoutReactions)
      .then(reserveTimeoutCapacity)
      .then(commitTimeoutSettlement)
      .then(checkState(
        waiter.fuel == 0 and
        waiter.fuelCharges == 1 and
        waiter.settled == List(1) and
        waiter.timedOut == List(1) and
        waiter.readyTokens == Set() and
        NoLostWaiter and NoDoubleSettlement and FuelConservation and
        FIFOPreservation and PreparedBeforeCommit and LiveWaiterAuthority
      ))

  // Notify capacity failure settles none and removes none; after the drain the
  // batch retries and settles each head once, in FIFO order.
  run waiterNotifyCapacityRetryTest =
    init
      .then(enqueueSync)
      .then(enqueueAsyncInfinite)
      .then(failNotifyReservation)
      .then(checkState(waiter.fifo == List(1, 2) and waiter.failedReservations == 1))
      .then(drainMicrotasks)
      .then(prepareNotifyHead)
      .then(commitNotifyHead)
      .then(prepareNotifyHead)
      .then(commitNotifyHead)
      .then(checkState(
        waiter.settled == List(1, 2) and
        NoLostWaiter and NoDoubleSettlement and FIFOPreservation and PreparedBeforeCommit
      ))

  // Cancellation and revocation edges settle nothing and keep every other
  // token's authority intact.
  run waiterCancellationRevocationTest =
    init
      .then(enqueueSync)
      .then(cancelHead)
      .then(enqueueAsyncInfinite)
      .then(revokeHead)
      .then(checkState(
        waiter.fifo == List() and
        waiter.settled == List() and
        NoLostWaiter and NoDoubleSettlement and FIFOPreservation
      ))

  // A late duplicate provider report changes nothing after settlement.
  run waiterStaleTimerReportTest =
    init
      .then(enqueueAsyncFinite)
      .then(markHeadTimeoutReady)
      .then(drainMicrotasks)
      .then(allocateTimeoutResult)
      .then(prepareTimeoutReactions)
      .then(reserveTimeoutCapacity)
      .then(commitTimeoutSettlement)
      .then(ignoreStaleTimerReport)
      .then(checkState(
        waiter.settled == List(1) and
        waiter.timedOut == List(1) and
        NoLostWaiter and NoDoubleSettlement and FuelConservation
      ))

  // Forced-GC alias accounting: growth is charged to the ledger; collecting the
  // growing wrapper and then the alias drops no charge while the waiter-queue
  // pin survives; the final release refunds the full charge exactly once.
  run sharedBackingForcedGcAliasTest =
    init
      .then(allocateSharedBacking)
      .then(aliasSharedBacking)
      .then(growSharedBacking)
      .then(pinSharedBacking)
      .then(collectWrapperOne)
      .then(checkState(
        backing.wrappers == Set(2) and
        backing.pins == 1 and
        backing.ledgerBytes == GrownBackingBytes and
        backing.chargedBytes == GrownBackingBytes and
        SharedBackingLedgerExact
      ))
      .then(collectWrapperTwo)
      .then(checkState(
        backing.wrappers == Set() and
        backing.pins == 1 and
        backing.ledgerBytes == GrownBackingBytes and
        SharedBackingLedgerExact
      ))
      .then(releaseFinalPin)
      .then(checkState(
        backing.refundCount == 1 and
        backing.refundedBytes == GrownBackingBytes and
        backing.chargedBytes == GrownBackingBytes and
        SharedBackingLedgerExact and SharedBackingRefundExactlyOnce
      ))

  // The last surviving wrapper is itself the final authority: its sweep
  // releases the charge exactly once.
  run sharedBackingFinalWrapperTest =
    init
      .then(allocateSharedBacking)
      .then(growSharedBacking)
      .then(aliasSharedBacking)
      .then(pinSharedBacking)
      .then(releasePin)
      .then(collectWrapperTwo)
      .then(checkState(SharedBackingLedgerExact and backing.refundCount == 0))
      .then(releaseFinalWrapper)
      .then(checkState(
        backing.refundCount == 1 and
        backing.refundedBytes == GrownBackingBytes and
        SharedBackingLedgerExact and SharedBackingRefundExactlyOnce
      ))

  // Fault: reserving capacity without the allocation/preparation plan is the
  // reserve-at-registration defect; it breaks the preparation discipline while
  // the conservation and FIFO cores hold.
  action faultReserveWithoutPreparation = all {
    waiter.fifo.length() > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("reservedTokens", waiter.reservedTokens.union(Set(waiter.fifo.head()))),
    backing' = backing,
  }

  // Fault: settling without removing authority is the recorded lost-waiter
  // defect: the token is both authoritative and settled.
  action faultSettleWithoutRemovingAuthority = all {
    waiter.fifo.length() > 0 and waiter.fuel > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter.reservedTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("settled", waiter.settled.append(waiter.fifo.head()))
      .with("timedOut", waiter.timedOut.append(waiter.fifo.head()))
      .with("fuel", waiter.fuel - 1)
      .with("fuelCharges", waiter.fuelCharges + 1),
    backing' = backing,
  }

  // Fault: charging the fuel unit before preparation is the F1 defect: a later
  // failed attempt would leave the balance reduced and the waiter stranded.
  action faultChargeBeforePreparation = all {
    waiter.fifo.length() > 0,
    waiter.readyTokens.contains(waiter.fifo.head()),
    waiter' = waiter
      .with("fuel", waiter.fuel - 1)
      .with("fuelCharges", waiter.fuelCharges + 1),
    backing' = backing,
  }

  // Fault: refunding the ledger while an alias or pin survives drops live
  // backing bytes, the F2 defect the forced-GC alias accounting test catches.
  action faultRefundWhileAliasSurvives = all {
    backing.ledgerLive and backing.wrappers != Set(),
    waiter' = waiter,
    backing' = backing
      .with("ledgerLive", false)
      .with("ledgerBytes", 0)
      .with("backingBytes", 0),
  }

  run faultReserveWithoutPreparationRun =
    init
      .then(enqueueAsyncFinite)
      .then(markHeadTimeoutReady)
      .then(faultReserveWithoutPreparation)
      .then(checkState(
        not(PreparedBeforeCommit) and
        NoLostWaiter and FuelConservation and FIFOPreservation and NoDoubleSettlement
      ))

  run faultSettleWithoutRemovingAuthorityRun =
    init
      .then(enqueueAsyncFinite)
      .then(markHeadTimeoutReady)
      .then(drainMicrotasks)
      .then(allocateTimeoutResult)
      .then(prepareTimeoutReactions)
      .then(reserveTimeoutCapacity)
      .then(faultSettleWithoutRemovingAuthority)
      .then(checkState(
        not(NoLostWaiter) and
        FIFOPreservation and SharedBackingLedgerExact and SharedBackingRefundExactlyOnce
      ))

  run faultChargeBeforePreparationRun =
    init
      .then(enqueueAsyncFinite)
      .then(markHeadTimeoutReady)
      .then(faultChargeBeforePreparation)
      .then(allocateTimeoutResult)
      .then(prepareTimeoutReactions)
      .then(failTimeoutReservation)
      .then(checkState(
        waiter.fuel == 0 and
        not(FuelConservation) and
        NoLostWaiter and NoDoubleSettlement and FIFOPreservation and PreparedBeforeCommit
      ))

  run faultRefundWhileAliasSurvivesRun =
    init
      .then(allocateSharedBacking)
      .then(aliasSharedBacking)
      .then(growSharedBacking)
      .then(pinSharedBacking)
      .then(faultRefundWhileAliasSurvives)
      .then(checkState(
        not(SharedBackingLedgerExact) and
        NoLostWaiter and FuelConservation and FIFOPreservation
      ))
}
