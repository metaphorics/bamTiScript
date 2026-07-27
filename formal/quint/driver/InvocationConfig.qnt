module InvocationConfig {
  val Seed = 101
  val Bound = 3
  val Parse = 0
  val LoadConfig = 1
  val Plan = 2
  val Compile = 3
  val WatchIdle = 4
  val Cancelling = 5
  val Closed = 6
  val ExitSuccess = 0
  val ExitGraphCycle = 4
  val ExitInvalidConfig = 3

  var phase: int
  var configValid: bool
  var graphValid: bool
  var loaded: bool
  var exitCode: int
  var sourceRead: bool

  pure def config(valid, graphOk) = { valid: valid, graphOk: graphOk }
  pure def phaseKnown(p) = p >= Parse and p <= Closed
  pure def mayCompile(c) = c.valid and c.graphOk
  pure def terminal(p) = p == Closed
  val inv_phase = phaseKnown(phase)
  val configReady =
    not(phase == WatchIdle) or (loaded and configValid and graphValid)

  action init = all {
    phase' = Parse,
    configValid' = true,
    graphValid' = true,
    loaded' = false,
    exitCode' = ExitSuccess,
    sourceRead' = false,
  }

  action parse = all {
    phase == Parse,
    phase' = LoadConfig,
    configValid' = configValid,
    graphValid' = graphValid,
    loaded' = loaded,
    exitCode' = exitCode,
    sourceRead' = sourceRead,
  }

  action loadConfig = all {
    phase == LoadConfig,
    phase' = Plan,
    configValid' = configValid,
    graphValid' = graphValid,
    loaded' = true,
    exitCode' = exitCode,
    sourceRead' = true,
  }

  action invalidateConfig = all {
    phase == LoadConfig,
    phase' = Plan,
    configValid' = false,
    graphValid' = graphValid,
    loaded' = true,
    exitCode' = exitCode,
    sourceRead' = true,
  }

  action invalidateGraph = all {
    phase == LoadConfig,
    phase' = Plan,
    configValid' = configValid,
    graphValid' = false,
    loaded' = true,
    exitCode' = exitCode,
    sourceRead' = true,
  }

  action plan = all {
    phase == Plan,
    phase' = if (configValid and graphValid) WatchIdle else Closed,
    configValid' = configValid,
    graphValid' = graphValid,
    loaded' = loaded,
    exitCode' = if (configValid) (if (graphValid) ExitSuccess else ExitGraphCycle) else ExitInvalidConfig,
    sourceRead' = sourceRead,
  }

  action close = all {
    phase != Closed,
    phase' = Closed,
    configValid' = configValid,
    graphValid' = graphValid,
    loaded' = loaded,
    exitCode' = exitCode,
    sourceRead' = sourceRead,
  }

  action step = any { parse, loadConfig, invalidateConfig, invalidateGraph, plan, close }

  action faultReadyWithInvalidConfig = all {
    phase == WatchIdle,
    phase' = WatchIdle,
    configValid' = false,
    graphValid' = true,
    loaded' = true,
    exitCode' = ExitSuccess,
    sourceRead' = true,
  }

  action faultPhase = all {
    phase == Parse,
    phase' = Closed + 1,
    configValid' = configValid,
    graphValid' = graphValid,
    loaded' = loaded,
    exitCode' = exitCode,
    sourceRead' = sourceRead,
  }

  action checkState(p) = all {
    p,
    phase' = phase,
    configValid' = configValid,
    graphValid' = graphValid,
    loaded' = loaded,
    exitCode' = exitCode,
    sourceRead' = sourceRead,
  }

  run config_boundsTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(checkState(inv_phase and configReady and mayCompile(config(configValid, graphValid)) and sourceRead))

  run config_invalidConfigTest =
    init
      .then(parse)
      .then(invalidateConfig)
      .then(plan)
      .then(checkState(terminal(phase) and not(configValid) and not(mayCompile(config(configValid, graphValid))) and exitCode == ExitInvalidConfig and sourceRead))

  run config_invalidGraphTest =
    init
      .then(parse)
      .then(invalidateGraph)
      .then(plan)
      .then(checkState(terminal(phase) and not(graphValid) and not(mayCompile(config(configValid, graphValid))) and exitCode == ExitGraphCycle and sourceRead))

  run config_faultConfigTest =
    init
      .then(parse)
      .then(loadConfig)
      .then(plan)
      .then(faultReadyWithInvalidConfig)
      .then(checkState(not(configReady) and phaseKnown(phase) and loaded and sourceRead))
  run config_faultPhaseTest =
    init
      .then(faultPhase)
      .then(checkState(not(inv_phase)))

}
