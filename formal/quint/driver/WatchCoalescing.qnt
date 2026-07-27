module WatchCoalescing {
  val Seed = 109
  val Bound = 3

  var dirty: bool
  var active: bool
  var cancelled: bool
  var pending: int

  pure def watch(dirty, active, cancelled, pending) = {
    dirty: dirty,
    active: active,
    cancelled: cancelled,
    pending: pending,
  }
  pure def dirtyCancels(w) = not(w.dirty and w.active) or w.cancelled
  pure def coalesced(w) = w.pending >= 0 and w.pending <= Bound
  pure def quiescent(w) = not(w.active) and not(w.dirty) and w.pending == 0
  val currentWatch = watch(dirty, active, cancelled, pending)
  val inv_dirty_cancels = dirtyCancels(currentWatch)

  action init = all {
    dirty' = false,
    active' = false,
    cancelled' = false,
    pending' = 0,
  }

  action markDirty = all {
    pending < Bound,
    dirty' = true,
    active' = active,
    cancelled' = if (active) true else false,
    pending' = pending + 1,
  }

  action startCompile = all {
    dirty and not(active) and pending > 0,
    dirty' = false,
    active' = true,
    cancelled' = false,
    pending' = pending - 1,
  }

  action finishCompile = all {
    active and not(dirty) and not(cancelled),
    dirty' = false,
    active' = false,
    cancelled' = false,
    pending' = pending,
  }

  action cancel = all {
    active and cancelled,
    dirty' = dirty,
    active' = false,
    cancelled' = true,
    pending' = pending,
  }

  action step = any { markDirty, startCompile, finishCompile, cancel }

  action faultDirtyWithoutCancel = all {
    not(active),
    dirty' = true,
    active' = true,
    cancelled' = false,
    pending' = 1,
  }

  action faultOverfullQueue = all {
    not(active),
    dirty' = false,
    active' = false,
    cancelled' = false,
    pending' = Bound + 1,
  }

  action faultNeverQuiescent = all {
    not(active),
    dirty' = false,
    active' = true,
    cancelled' = false,
    pending' = 0,
  }

  action checkState(p) = all {
    p,
    dirty' = dirty,
    active' = active,
    cancelled' = cancelled,
    pending' = pending,
  }

  run watch_boundsTest =
    init
      .then(markDirty)
      .then(startCompile)
      .then(finishCompile)
      .then(checkState(coalesced(currentWatch) and inv_dirty_cancels and quiescent(currentWatch)))

  run watch_dirtyCancellationTest =
    init
      .then(markDirty)
      .then(startCompile)
      .then(markDirty)
      .then(checkState(dirty and active and cancelled and inv_dirty_cancels and coalesced(currentWatch)))
      .then(cancel)
      .then(startCompile)
      .then(finishCompile)
      .then(checkState(quiescent(currentWatch) and coalesced(currentWatch) and inv_dirty_cancels))

  run watch_faultDirtyTest =
    init
      .then(faultDirtyWithoutCancel)
      .then(checkState(not(inv_dirty_cancels) and coalesced(currentWatch)))

  run watch_faultQueueTest =
    init
      .then(faultOverfullQueue)
      .then(checkState(not(coalesced(currentWatch)) and inv_dirty_cancels))

  run watch_faultQuiescenceTest =
    init
      .then(faultNeverQuiescent)
      .then(checkState(not(quiescent(currentWatch)) and coalesced(currentWatch) and inv_dirty_cancels))
}
