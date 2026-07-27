module BuildEpoch {
  val Seed = 107
  val Bound = 3
  val ExitSuccess = 0
  val ExitDiagnosticsWithOutputs = 2
  val ExitDiagnosticsNoOutputs = 1

  var planned: bool
  var active: bool
  var cancelled: bool
  var compiled: bool
  var staged: bool
  var published: bool
  var exitCode: int

  pure def epoch(id, cancelled, planned, compiled, published, staged, exitCode) = {
    id: id,
    cancelled: cancelled,
    planned: planned,
    compiled: compiled,
    published: published,
    staged: staged,
    exitCode: exitCode,
  }
  pure def canPublish(e) = e.planned and e.compiled and e.staged and not(e.cancelled)
  pure def atomicEmit(e) = not(e.published) or (e.compiled and e.staged)
  pure def emitExit(e) =
    not(e.published) or e.exitCode == ExitSuccess or e.exitCode == ExitDiagnosticsWithOutputs
  pure def cancelledDoesNotPublish(e) = not(e.cancelled and e.published)
  val currentEpoch =
    epoch(1, cancelled, planned, compiled, published, staged, exitCode)
  val inv_atomic_emit = atomicEmit(currentEpoch)
  val inv_cancel_no_publish = cancelledDoesNotPublish(currentEpoch)
  val inv_emit_exit = emitExit(currentEpoch)

  action init = all {
    planned' = false,
    active' = false,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
  }

  action plan = all {
    not(planned),
    planned' = true,
    active' = false,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
  }

  action startCompile = all {
    planned and not(active) and not(cancelled) and not(compiled),
    planned' = planned,
    active' = true,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = exitCode,
  }

  action stageEmit = all {
    planned and active and not(cancelled),
    planned' = planned,
    active' = true,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = false,
    exitCode' = ExitSuccess,
  }

  action stageEmitWithDiagnostics = all {
    planned and active and not(cancelled),
    planned' = planned,
    active' = true,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = false,
    exitCode' = ExitDiagnosticsWithOutputs,
  }

  action publish = all {
    planned and active and compiled and staged and not(cancelled),
    planned' = planned,
    active' = false,
    cancelled' = false,
    compiled' = compiled,
    staged' = staged,
    published' = true,
    exitCode' = exitCode,
  }

  action cancel = all {
    active,
    planned' = planned,
    active' = false,
    cancelled' = true,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = exitCode,
  }

  action step = any { plan, startCompile, stageEmit, stageEmitWithDiagnostics, publish, cancel }

  action faultCancelPublish = all {
    not(active),
    planned' = true,
    active' = false,
    cancelled' = true,
    compiled' = true,
    staged' = true,
    published' = true,
    exitCode' = ExitSuccess,
  }

  action faultPartialEmit = all {
    not(active),
    planned' = true,
    active' = false,
    cancelled' = false,
    compiled' = true,
    staged' = false,
    published' = true,
    exitCode' = ExitSuccess,
  }

  action faultBadEmitExit = all {
    not(active),
    planned' = true,
    active' = false,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = true,
    exitCode' = ExitDiagnosticsNoOutputs,
  }

  action checkState(p) = all {
    p,
    planned' = planned,
    active' = active,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
  }

  run epoch_boundsTest =
    init
      .then(plan)
      .then(startCompile)
      .then(stageEmit)
      .then(checkState(compiled and staged and not(published) and canPublish(currentEpoch) and inv_atomic_emit and inv_cancel_no_publish and inv_emit_exit))
      .then(publish)
      .then(checkState(published and inv_atomic_emit and inv_emit_exit and inv_cancel_no_publish))

  run epoch_diagnosticEmitTest =
    init
      .then(plan)
      .then(startCompile)
      .then(stageEmitWithDiagnostics)
      .then(publish)
      .then(checkState(published and exitCode == ExitDiagnosticsWithOutputs and inv_atomic_emit and inv_emit_exit and inv_cancel_no_publish))

  run epoch_cancelTest =
    init
      .then(plan)
      .then(startCompile)
      .then(stageEmit)
      .then(cancel)
      .then(checkState(cancelled and not(published) and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit))

  run epoch_faultCancelTest =
    init
      .then(faultCancelPublish)
      .then(checkState(not(inv_cancel_no_publish) and inv_atomic_emit and inv_emit_exit))

  run epoch_faultAtomicTest =
    init
      .then(faultPartialEmit)
      .then(checkState(not(inv_atomic_emit) and inv_cancel_no_publish and inv_emit_exit))

  run epoch_faultExitTest =
    init
      .then(faultBadEmitExit)
      .then(checkState(not(inv_emit_exit) and inv_atomic_emit and inv_cancel_no_publish))
}
