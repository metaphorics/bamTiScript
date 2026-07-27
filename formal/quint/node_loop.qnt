module node_loop {
  val Seed = 301
  val Bound = 3
  val CallbackIds = 1.to(Bound)
  val RuntimeOwners = 1.to(2)

  var nextCallbackId: int
  var readyQueue: List[int]
  var enqueuedIds: List[int]
  var deliveredIds: List[int]
  var discardedIds: List[int]
  var deliveryCounts: int -> int
  var callbackOwner: int
  var runtimeOwner: int
  var callbackActive: bool
  var closed: bool
  var checkpoint: int
  var checkpointIds: List[int]
  var microtaskCheckpoint: int
  var closeAcceptedCount: int
  var closeDeliveredCount: int
  var shutdown: bool
  var shutdownAcceptedCount: int
  var shutdownDeliveredCount: int
  var postCloseEnqueueAttempted: bool
  var postCloseCallbackAttempted: bool
  var rejectedEnqueueAttempts: int
  var rejectedCallbackAttempts: int

  pure def boundedCallbackIds(ids: List[int]): bool =
    ids.length() >= 0 and
    ids.length() <= Bound and
    ids.indices().forall(index => ids.nth(index).in(CallbackIds))

  action init = all {
    nextCallbackId' = 1,
    readyQueue' = List(),
    enqueuedIds' = List(),
    deliveredIds' = List(),
    discardedIds' = List(),
    deliveryCounts' = CallbackIds.mapBy(id => 0),
    callbackOwner' = 0,
    runtimeOwner' = 1,
    callbackActive' = false,
    closed' = false,
    checkpoint' = 0,
    checkpointIds' = List(),
    microtaskCheckpoint' = 0,
    closeAcceptedCount' = 0,
    closeDeliveredCount' = 0,
    shutdown' = false,
    shutdownAcceptedCount' = 0,
    shutdownDeliveredCount' = 0,
    postCloseEnqueueAttempted' = false,
    postCloseCallbackAttempted' = false,
    rejectedEnqueueAttempts' = 0,
    rejectedCallbackAttempts' = 0,
  }

  action enqueue = all {
    not(closed) and
    not(shutdown) and
    readyQueue.length() < Bound and
    nextCallbackId <= Bound,
    nextCallbackId' = nextCallbackId + 1,
    readyQueue' = readyQueue.append(nextCallbackId),
    enqueuedIds' = enqueuedIds.append(nextCallbackId),
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = callbackOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action deliver = all {
    not(closed) and
    not(shutdown) and
    readyQueue.length() > 0 and
    deliveredIds.length() < Bound and
    checkpoint < Bound and
    deliveryCounts.get(readyQueue.head()) == 0,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue.tail(),
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds.append(readyQueue.head()),
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts.set(readyQueue.head(), deliveryCounts.get(readyQueue.head()) + 1),
    callbackOwner' = runtimeOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = true,
    closed' = closed,
    checkpoint' = checkpoint + 1,
    checkpointIds' = checkpointIds.append(readyQueue.head()),
    microtaskCheckpoint' = checkpoint + 1,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action close = all {
    not(closed) and not(shutdown),
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = 0,
    runtimeOwner' = runtimeOwner,
    callbackActive' = false,
    closed' = true,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = enqueuedIds.length(),
    closeDeliveredCount' = deliveredIds.length(),
    shutdown' = false,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action stop = all {
    closed and not(shutdown),
    nextCallbackId' = nextCallbackId,
    readyQueue' = List(),
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds.concat(readyQueue),
    deliveryCounts' = deliveryCounts,
    callbackOwner' = 0,
    runtimeOwner' = runtimeOwner,
    callbackActive' = false,
    closed' = true,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = true,
    shutdownAcceptedCount' = enqueuedIds.length(),
    shutdownDeliveredCount' = deliveredIds.length(),
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action rejectEnqueueAfterClose = all {
    (closed or shutdown) and rejectedEnqueueAttempts < Bound,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = callbackOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = true,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts + 1,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action rejectCallbackAfterClose = all {
    (closed or shutdown) and rejectedCallbackAttempts < Bound,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = callbackOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = true,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts + 1,
  }

  action faultTypeOK = all {
    not(callbackActive),
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = 0,
    runtimeOwner' = 0,
    callbackActive' = false,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultFIFO = all {
    not(closed) and
    not(shutdown) and
    readyQueue.length() == 2 and
    checkpoint < Bound,
    nextCallbackId' = nextCallbackId,
    readyQueue' = List(readyQueue.head()),
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds.append(readyQueue.tail().head()),
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts.set(readyQueue.tail().head(), deliveryCounts.get(readyQueue.tail().head()) + 1),
    callbackOwner' = runtimeOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = true,
    closed' = closed,
    checkpoint' = checkpoint + 1,
    checkpointIds' = checkpointIds.append(readyQueue.tail().head()),
    microtaskCheckpoint' = checkpoint + 1,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultDuplicateDelivery = all {
    not(closed) and
    not(shutdown) and
    readyQueue.length() == 1 and
    checkpoint <= Bound - 2,
    nextCallbackId' = nextCallbackId,
    readyQueue' = List(),
    enqueuedIds' = enqueuedIds.append(readyQueue.head()),
    deliveredIds' = deliveredIds.append(readyQueue.head()).append(readyQueue.head()),
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts.set(readyQueue.head(), deliveryCounts.get(readyQueue.head()) + 2),
    callbackOwner' = runtimeOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = true,
    closed' = closed,
    checkpoint' = checkpoint + 2,
    checkpointIds' = checkpointIds.append(readyQueue.head()).append(readyQueue.head()),
    microtaskCheckpoint' = checkpoint + 2,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultAsyncOwner = all {
    callbackActive and runtimeOwner == 1,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = 2,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultCloseEnqueue = all {
    closed and
    not(shutdown) and
    readyQueue.length() < Bound and
    nextCallbackId <= Bound,
    nextCallbackId' = nextCallbackId + 1,
    readyQueue' = readyQueue.append(nextCallbackId),
    enqueuedIds' = enqueuedIds.append(nextCallbackId),
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = callbackOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = true,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultCloseCallback = all {
    closed and
    not(shutdown) and
    readyQueue.length() > 0 and
    deliveredIds.length() < Bound and
    checkpoint < Bound and
    deliveryCounts.get(readyQueue.head()) == 0,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue.tail(),
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds.append(readyQueue.head()),
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts.set(readyQueue.head(), deliveryCounts.get(readyQueue.head()) + 1),
    callbackOwner' = runtimeOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = true,
    closed' = closed,
    checkpoint' = checkpoint + 1,
    checkpointIds' = checkpointIds.append(readyQueue.head()),
    microtaskCheckpoint' = checkpoint + 1,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = true,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultCheckpointOrder = all {
    deliveredIds.length() > 0 and
    microtaskCheckpoint < Bound,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = callbackOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint + 1,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action faultShutdownCallback = all {
    shutdown and
    discardedIds.length() > 0 and
    deliveredIds.length() < Bound and
    checkpoint < Bound and
    deliveryCounts.get(discardedIds.head()) == 0,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds.append(discardedIds.head()),
    discardedIds' = discardedIds.tail(),
    deliveryCounts' = deliveryCounts.set(discardedIds.head(), deliveryCounts.get(discardedIds.head()) + 1),
    callbackOwner' = runtimeOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = true,
    closed' = closed,
    checkpoint' = checkpoint + 1,
    checkpointIds' = checkpointIds.append(discardedIds.head()),
    microtaskCheckpoint' = checkpoint + 1,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = true,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }

  action step = any {
    enqueue,
    deliver,
    close,
    stop,
    rejectEnqueueAfterClose,
    rejectCallbackAfterClose,
  }

  val TypeOK =
    nextCallbackId >= 1 and
    nextCallbackId <= Bound + 1 and
    boundedCallbackIds(readyQueue) and
    boundedCallbackIds(enqueuedIds) and
    boundedCallbackIds(deliveredIds) and
    boundedCallbackIds(discardedIds) and
    boundedCallbackIds(checkpointIds) and
    RuntimeOwners.contains(runtimeOwner) and
    (callbackOwner == 0 or RuntimeOwners.contains(callbackOwner)) and
    (not(callbackActive) or RuntimeOwners.contains(callbackOwner)) and
    checkpoint >= 0 and
    checkpoint <= Bound and
    microtaskCheckpoint >= 0 and
    microtaskCheckpoint <= Bound and
    closeAcceptedCount >= 0 and
    closeAcceptedCount <= Bound and
    closeDeliveredCount >= 0 and
    closeDeliveredCount <= Bound and
    shutdownAcceptedCount >= 0 and
    shutdownAcceptedCount <= Bound and
    shutdownDeliveredCount >= 0 and
    shutdownDeliveredCount <= Bound and
    rejectedEnqueueAttempts >= 0 and
    rejectedEnqueueAttempts <= Bound and
    rejectedCallbackAttempts >= 0 and
    rejectedCallbackAttempts <= Bound and
    (not(shutdown) or closed) and
    CallbackIds.forall(id =>
      deliveryCounts.keys().contains(id) and
      deliveryCounts.get(id) >= 0 and
      deliveryCounts.get(id) <= Bound
    )

  val InvFIFO = enqueuedIds == deliveredIds.concat(discardedIds).concat(readyQueue)

  val InvExactlyOnce =
    CallbackIds.forall(id =>
      deliveryCounts.get(id) == deliveredIds.select(deliveredId => deliveredId == id).length() and
      deliveryCounts.get(id) <= 1
    )

  val InvAsyncOwner = not(callbackActive) or callbackOwner == runtimeOwner

  val InvClose =
    not(closed) or
    shutdown or
    (
      enqueuedIds.length() == closeAcceptedCount and
      deliveredIds.length() == closeDeliveredCount and
      not(callbackActive) and
      (not(postCloseEnqueueAttempted) or rejectedEnqueueAttempts > 0) and
      (not(postCloseCallbackAttempted) or rejectedCallbackAttempts > 0)
    )

  val InvCheckpointOrder =
    checkpointIds == deliveredIds and
    checkpoint == deliveredIds.length() and
    microtaskCheckpoint == checkpoint

  val InvShutdown =
    not(shutdown) or
    (
      readyQueue == List() and
      enqueuedIds.length() == shutdownAcceptedCount and
      deliveredIds.length() == shutdownDeliveredCount and
      not(callbackActive)
    )

  val Live = shutdown or closed or readyQueue.length() == 0
  // Quint run composition requires checks to frame the model state.
  action checkState(p) = all {
    p,
    nextCallbackId' = nextCallbackId,
    readyQueue' = readyQueue,
    enqueuedIds' = enqueuedIds,
    deliveredIds' = deliveredIds,
    discardedIds' = discardedIds,
    deliveryCounts' = deliveryCounts,
    callbackOwner' = callbackOwner,
    runtimeOwner' = runtimeOwner,
    callbackActive' = callbackActive,
    closed' = closed,
    checkpoint' = checkpoint,
    checkpointIds' = checkpointIds,
    microtaskCheckpoint' = microtaskCheckpoint,
    closeAcceptedCount' = closeAcceptedCount,
    closeDeliveredCount' = closeDeliveredCount,
    shutdown' = shutdown,
    shutdownAcceptedCount' = shutdownAcceptedCount,
    shutdownDeliveredCount' = shutdownDeliveredCount,
    postCloseEnqueueAttempted' = postCloseEnqueueAttempted,
    postCloseCallbackAttempted' = postCloseCallbackAttempted,
    rejectedEnqueueAttempts' = rejectedEnqueueAttempts,
    rejectedCallbackAttempts' = rejectedCallbackAttempts,
  }


  run normalTypeOKTest =
    init.then(enqueue).then(checkState(TypeOK and readyQueue == List(1)))

  run normalFIFOTest =
    init
      .then(enqueue)
      .then(enqueue)
      .then(deliver)
      .then(checkState(InvFIFO and deliveredIds == List(1) and readyQueue == List(2)))

  run normalExactlyOnceTest =
    init
      .then(enqueue)
      .then(deliver)
      .then(checkState(
        InvExactlyOnce and
        deliveredIds == List(1) and
        deliveryCounts.get(1) == 1
      ))

  run normalAsyncOwnerTest =
    init
      .then(enqueue)
      .then(deliver)
      .then(checkState(InvAsyncOwner and callbackActive and callbackOwner == runtimeOwner))

  run normalCloseTest =
    init
      .then(enqueue)
      .then(close)
      .then(rejectEnqueueAfterClose)
      .then(rejectCallbackAfterClose)
      .then(checkState(
        InvClose and
        closed and
        not(shutdown) and
        postCloseEnqueueAttempted and
        postCloseCallbackAttempted and
        rejectedEnqueueAttempts == 1 and
        rejectedCallbackAttempts == 1 and
        readyQueue == List(1)
      ))

  run normalCheckpointOrderTest =
    init
      .then(enqueue)
      .then(deliver)
      .then(checkState(
        InvCheckpointOrder and
        deliveredIds.length() == 1 and
        checkpointIds == List(1)
      ))

  run normalShutdownTest =
    init
      .then(enqueue)
      .then(close)
      .then(stop)
      .then(rejectEnqueueAfterClose)
      .then(rejectCallbackAfterClose)
      .then(checkState(
        InvShutdown and
        shutdown and
        discardedIds == List(1) and
        rejectedEnqueueAttempts == 1 and
        rejectedCallbackAttempts == 1
      ))

  run normalLiveTest =
    init.then(enqueue).then(deliver).then(checkState(Live and readyQueue == List()))

  run faultTypeOKTest =
    init
      .then(faultTypeOK)
      .then(checkState(
        not(TypeOK) and
        InvFIFO and
        InvExactlyOnce and
        InvAsyncOwner and
        InvClose and
        InvCheckpointOrder and
        InvShutdown
      ))

  run faultFIFOTest =
    init
      .then(enqueue)
      .then(enqueue)
      .then(faultFIFO)
      .then(checkState(
        TypeOK and
        not(InvFIFO) and
        InvExactlyOnce and
        InvAsyncOwner and
        InvClose and
        InvCheckpointOrder and
        InvShutdown
      ))

  run faultExactlyOnceTest =
    init
      .then(enqueue)
      .then(faultDuplicateDelivery)
      .then(checkState(
        TypeOK and
        InvFIFO and
        not(InvExactlyOnce) and
        InvAsyncOwner and
        InvClose and
        InvCheckpointOrder and
        InvShutdown
      ))

  run faultAsyncOwnerTest =
    init
      .then(enqueue)
      .then(deliver)
      .then(faultAsyncOwner)
      .then(checkState(
        TypeOK and
        InvFIFO and
        InvExactlyOnce and
        not(InvAsyncOwner) and
        InvClose and
        InvCheckpointOrder and
        InvShutdown
      ))

  run faultCloseEnqueueTest =
    init
      .then(close)
      .then(faultCloseEnqueue)
      .then(checkState(
        TypeOK and
        InvFIFO and
        InvExactlyOnce and
        InvAsyncOwner and
        not(InvClose) and
        InvCheckpointOrder and
        InvShutdown
      ))

  run faultCloseCallbackTest =
    init
      .then(enqueue)
      .then(close)
      .then(faultCloseCallback)
      .then(checkState(
        TypeOK and
        InvFIFO and
        InvExactlyOnce and
        InvAsyncOwner and
        not(InvClose) and
        InvCheckpointOrder and
        InvShutdown
      ))

  run faultCheckpointOrderTest =
    init
      .then(enqueue)
      .then(deliver)
      .then(faultCheckpointOrder)
      .then(checkState(
        TypeOK and
        InvFIFO and
        InvExactlyOnce and
        InvAsyncOwner and
        InvClose and
        not(InvCheckpointOrder) and
        InvShutdown
      ))

  run faultShutdownTest =
    init
      .then(enqueue)
      .then(close)
      .then(stop)
      .then(faultShutdownCallback)
      .then(checkState(
        TypeOK and
        InvFIFO and
        InvExactlyOnce and
        InvAsyncOwner and
        InvClose and
        InvCheckpointOrder and
        not(InvShutdown)
      ))

  run loop_boundsTest =
    init
      .then(enqueue)
      .then(enqueue)
      .then(deliver)
      .then(deliver)
      .then(close)
      .then(rejectEnqueueAfterClose)
      .then(rejectCallbackAfterClose)
      .then(stop)
      .then(checkState(
        TypeOK and
        InvFIFO and
        InvExactlyOnce and
        InvAsyncOwner and
        InvClose and
        InvCheckpointOrder and
        InvShutdown and
        Live
      ))
}
