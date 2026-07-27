module DriverSystem {
  val Seed = 113
  val Bound = 3
  val Parse = 0
  val LoadConfig = 1
  val Plan = 2
  val Compile = 3
  val WatchIdle = 4
  val Cancelling = 5
  val Closed = 6
  val ExitSuccess = 0
  val ExitDiagnosticsWithOutputs = 2
  val ExitDiagnosticsNoOutputs = 1
  val ExitGraphCycle = 4
  val ExitInvalidConfig = 3

  var phase: int
  var configValid: bool
  var graphValid: bool
  var dependencyBuilt: bool
  var cancelled: bool
  var compiled: bool
  var staged: bool
  var published: bool
  var exitCode: int
  var dirty: bool
  var active: bool
  var ticks: int

  pure def outputExit(code) =
    code == ExitSuccess or code == ExitDiagnosticsWithOutputs

  action init = all {
    phase' = Parse,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = false,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = false,
    ticks' = 0,
  }

  action parse = all {
    phase == Parse,
    phase' = LoadConfig,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = active,
    ticks' = ticks,
  }

  action loadConfig = all {
    phase == LoadConfig,
    phase' = Plan,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = active,
    ticks' = ticks,
  }

  action invalidateConfig = all {
    phase == LoadConfig,
    phase' = Plan,
    configValid' = false,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = active,
    ticks' = ticks,
  }

  action invalidateGraph = all {
    phase == LoadConfig,
    phase' = Plan,
    configValid' = configValid,
    graphValid' = false,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = active,
    ticks' = ticks,
  }

  action plan = all {
    phase == Plan,
    phase' = if (configValid and graphValid) WatchIdle else Closed,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = false,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = if (configValid) (if (graphValid) ExitSuccess else ExitGraphCycle) else ExitInvalidConfig,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action markDirty = all {
    (phase == WatchIdle and not(active)) or (phase == Compile and active),
    phase' = phase,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = if (phase == WatchIdle) false else dependencyBuilt,
    cancelled' = phase == Compile,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = true,
    active' = active,
    ticks' = ticks,
  }

  action buildDependency = all {
    phase == WatchIdle and dirty and not(active) and configValid and graphValid,
    phase' = WatchIdle,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = true,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = dirty,
    active' = false,
    ticks' = ticks,
  }

  action startCompile = all {
    phase == WatchIdle and dirty and dependencyBuilt and configValid and graphValid and ticks < Bound,
    phase' = Compile,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = true,
    ticks' = ticks + 1,
  }

  action finishCompile = all {
    phase == Compile and active and dependencyBuilt and configValid and graphValid and not(cancelled) and not(dirty),
    phase' = Compile,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = true,
    ticks' = ticks,
  }

  action finishCompileWithDiagnostics = all {
    phase == Compile and active and dependencyBuilt and configValid and graphValid and not(cancelled) and not(dirty),
    phase' = Compile,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = false,
    exitCode' = ExitDiagnosticsWithOutputs,
    dirty' = false,
    active' = true,
    ticks' = ticks,
  }

  action commitEmit = all {
    phase == Compile and active and dependencyBuilt and compiled and staged and not(cancelled) and not(dirty) and configValid and graphValid and outputExit(exitCode),
    phase' = WatchIdle,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = false,
    compiled' = compiled,
    staged' = staged,
    published' = true,
    exitCode' = exitCode,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action cancel = all {
    phase == Compile and active,
    phase' = Cancelling,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = true,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = true,
    ticks' = ticks,
  }

  action cancelDone = all {
    phase == Cancelling and active and cancelled,
    phase' = WatchIdle,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = false,
    cancelled' = true,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = false,
    ticks' = ticks,
  }

  action close = all {
    phase != Closed,
    phase' = Closed,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = false,
    cancelled' = cancelled,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = exitCode,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action step = any {
    parse,
    loadConfig,
    invalidateConfig,
    invalidateGraph,
    plan,
    markDirty,
    buildDependency,
    startCompile,
    finishCompile,
    finishCompileWithDiagnostics,
    commitEmit,
    cancel,
    cancelDone,
    close,
  }

  val inv_phase = phase >= Parse and phase <= Closed
  val inv_graph =
    not(active) or ((phase == Compile or phase == Cancelling) and configValid and graphValid)
  val inv_dependency_first = not(published) or (dependencyBuilt and compiled)
  val inv_cancel_no_publish = not(cancelled and published)
  val inv_atomic_emit = not(published) or (compiled and staged)
  val inv_emit_exit = not(published) or outputExit(exitCode)
  val inv_dirty_cancels = not(dirty and active) or cancelled
  val live_quiescent =
    phase == Closed or (phase == WatchIdle and not(active) and not(dirty))
  val safety =
    inv_phase and inv_graph and inv_dependency_first and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit and inv_dirty_cancels

  action faultPhase = all {
    phase == WatchIdle,
    phase' = Closed + 1,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = active,
    ticks' = ticks,
  }

  action faultConfigDuringCompile = all {
    phase == WatchIdle,
    phase' = Compile,
    configValid' = false,
    graphValid' = true,
    dependencyBuilt' = true,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = true,
    ticks' = ticks + 1,
  }

  action faultGraphDuringCompile = all {
    phase == WatchIdle,
    phase' = Compile,
    configValid' = true,
    graphValid' = false,
    dependencyBuilt' = true,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = true,
    ticks' = ticks + 1,
  }

  action faultDependencyPublish = all {
    phase == WatchIdle,
    phase' = WatchIdle,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = false,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = true,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action faultCancelledPublish = all {
    phase == WatchIdle,
    phase' = WatchIdle,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = true,
    cancelled' = true,
    compiled' = true,
    staged' = true,
    published' = true,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action faultPartialEmit = all {
    phase == WatchIdle,
    phase' = WatchIdle,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = true,
    cancelled' = false,
    compiled' = true,
    staged' = false,
    published' = true,
    exitCode' = ExitSuccess,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action faultEmitExit = all {
    phase == WatchIdle,
    phase' = WatchIdle,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = true,
    cancelled' = false,
    compiled' = true,
    staged' = true,
    published' = true,
    exitCode' = ExitDiagnosticsNoOutputs,
    dirty' = false,
    active' = false,
    ticks' = ticks,
  }

  action faultDirtyWithoutCancel = all {
    phase == WatchIdle,
    phase' = Compile,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = true,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = true,
    active' = true,
    ticks' = ticks + 1,
  }

  action faultNonQuiescent = all {
    phase == WatchIdle,
    phase' = WatchIdle,
    configValid' = true,
    graphValid' = true,
    dependencyBuilt' = false,
    cancelled' = false,
    compiled' = false,
    staged' = false,
    published' = false,
    exitCode' = ExitSuccess,
    dirty' = true,
    active' = false,
    ticks' = ticks,
  }

  action checkState(p) = all {
    p,
    phase' = phase,
    configValid' = configValid,
    graphValid' = graphValid,
    dependencyBuilt' = dependencyBuilt,
    cancelled' = cancelled,
    compiled' = compiled,
    staged' = staged,
    published' = published,
    exitCode' = exitCode,
    dirty' = dirty,
    active' = active,
    ticks' = ticks,
  }

  run driver_boundsTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(markDirty)
      .then(buildDependency)
      .then(startCompile)
      .then(checkState(phase == Compile and active and configValid and graphValid and inv_phase and inv_graph))
      .then(finishCompile)
      .then(checkState(compiled and staged and not(published) and inv_atomic_emit))
      .then(commitEmit)
      .then(checkState(published and dependencyBuilt and outputExit(exitCode) and safety and live_quiescent))

  run driver_invalidConfigTest =
    init
      .then(parse)
      .then(invalidateConfig)
      .then(plan)
      .then(checkState(phase == Closed and not(configValid) and not(published) and exitCode == ExitInvalidConfig and safety and live_quiescent))

  run driver_invalidGraphTest =
    init
      .then(parse)
      .then(invalidateGraph)
      .then(plan)
      .then(checkState(phase == Closed and not(graphValid) and not(published) and exitCode == ExitGraphCycle and safety and live_quiescent))

  run driver_watchCompileTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(markDirty)
      .then(buildDependency)
      .then(startCompile)
      .then(finishCompileWithDiagnostics)
      .then(checkState(compiled and staged and not(published) and exitCode == ExitDiagnosticsWithOutputs and inv_atomic_emit))
      .then(commitEmit)
      .then(checkState(published and exitCode == ExitDiagnosticsWithOutputs and inv_emit_exit and safety and live_quiescent))

  run driver_cancellationTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(markDirty)
      .then(buildDependency)
      .then(startCompile)
      .then(finishCompile)
      .then(markDirty)
      .then(checkState(dirty and active and cancelled and inv_dirty_cancels and not(published)))
      .then(cancel)
      .then(cancelDone)
      .then(checkState(phase == WatchIdle and not(active) and dirty and cancelled and not(published) and inv_cancel_no_publish))
      .then(buildDependency)
      .then(startCompile)
      .then(finishCompile)
      .then(commitEmit)
      .then(checkState(published and safety and live_quiescent))

  run driver_closeTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(markDirty)
      .then(buildDependency)
      .then(startCompile)
      .then(finishCompile)
      .then(close)
      .then(checkState(phase == Closed and not(active) and not(dirty) and not(compiled) and not(staged) and not(published) and safety and live_quiescent))

  run driver_phaseFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultPhase)
      .then(checkState(not(inv_phase) and inv_graph and inv_dependency_first and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit and inv_dirty_cancels))

  run driver_configFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultConfigDuringCompile)
      .then(checkState(not(inv_graph) and inv_phase and inv_dependency_first and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit and inv_dirty_cancels and ticks <= Bound))

  run driver_graphFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultGraphDuringCompile)
      .then(checkState(not(inv_graph) and inv_phase and inv_dependency_first and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit and inv_dirty_cancels and ticks <= Bound))

  run driver_dependencyFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultDependencyPublish)
      .then(checkState(not(inv_dependency_first) and inv_phase and inv_graph and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit and inv_dirty_cancels and live_quiescent))

  run driver_cancelFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultCancelledPublish)
      .then(checkState(not(inv_cancel_no_publish) and inv_phase and inv_graph and inv_dependency_first and inv_atomic_emit and inv_emit_exit and inv_dirty_cancels and live_quiescent))

  run driver_atomicFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultPartialEmit)
      .then(checkState(not(inv_atomic_emit) and inv_phase and inv_graph and inv_dependency_first and inv_cancel_no_publish and inv_emit_exit and inv_dirty_cancels and live_quiescent))

  run driver_emitExitFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultEmitExit)
      .then(checkState(not(inv_emit_exit) and inv_phase and inv_graph and inv_dependency_first and inv_cancel_no_publish and inv_atomic_emit and inv_dirty_cancels and live_quiescent))

  run driver_dirtyFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultDirtyWithoutCancel)
      .then(checkState(not(inv_dirty_cancels) and inv_phase and inv_graph and inv_dependency_first and inv_cancel_no_publish and inv_atomic_emit and inv_emit_exit and ticks <= Bound))

  run driver_quiescenceFaultTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultNonQuiescent)
      .then(checkState(not(live_quiescent) and safety))
}
