module ProjectGraph {
  val Seed = 103
  val Bound = 3
  val Plan = 0
  val Compile = 1
  val Closed = 2
  val ExitSuccess = 0
  val ExitGraphCycle = 4

  var phase: int
  var graphValid: bool
  var aDependsOnB: bool
  var bDependsOnA: bool
  var dependencyBuilt: bool
  var rootBuilt: bool
  var exitCode: int

  pure def graph(aDependsOnB, bDependsOnA, valid) = {
    aDependsOnB: aDependsOnB,
    bDependsOnA: bDependsOnA,
    valid: valid,
  }
  pure def acyclic(g) = not(g.aDependsOnB and g.bDependsOnA)
  pure def graphAccepted(g) = g.valid and acyclic(g)
  pure def dependencyFirst(g, builtA, builtB) =
    not(g.aDependsOnB) or not(builtA) or builtB
  val graphSafety =
    not(phase == Compile) or graphAccepted(graph(aDependsOnB, bDependsOnA, graphValid))
  val dependencySafety =
    not(rootBuilt) or dependencyFirst(graph(aDependsOnB, bDependsOnA, graphValid), rootBuilt, dependencyBuilt)
  val inv_graph = graphSafety
  val inv_dependency_first = dependencySafety

  action init = all {
    phase' = Plan,
    graphValid' = true,
    aDependsOnB' = true,
    bDependsOnA' = false,
    dependencyBuilt' = false,
    rootBuilt' = false,
    exitCode' = ExitSuccess,
  }

  action invalidateGraph = all {
    phase == Plan,
    phase' = Plan,
    graphValid' = false,
    aDependsOnB' = true,
    bDependsOnA' = true,
    dependencyBuilt' = false,
    rootBuilt' = false,
    exitCode' = ExitSuccess,
  }

  action plan = all {
    phase == Plan,
    phase' = if (graphAccepted(graph(aDependsOnB, bDependsOnA, graphValid))) Compile else Closed,
    graphValid' = graphValid,
    aDependsOnB' = aDependsOnB,
    bDependsOnA' = bDependsOnA,
    dependencyBuilt' = false,
    rootBuilt' = false,
    exitCode' = if (graphAccepted(graph(aDependsOnB, bDependsOnA, graphValid))) ExitSuccess else ExitGraphCycle,
  }

  action buildDependency = all {
    phase == Compile and graphAccepted(graph(aDependsOnB, bDependsOnA, graphValid)) and not(dependencyBuilt),
    phase' = Compile,
    graphValid' = graphValid,
    aDependsOnB' = aDependsOnB,
    bDependsOnA' = bDependsOnA,
    dependencyBuilt' = true,
    rootBuilt' = false,
    exitCode' = exitCode,
  }

  action buildRoot = all {
    phase == Compile and graphAccepted(graph(aDependsOnB, bDependsOnA, graphValid)) and dependencyBuilt and not(rootBuilt),
    phase' = Compile,
    graphValid' = graphValid,
    aDependsOnB' = aDependsOnB,
    bDependsOnA' = bDependsOnA,
    dependencyBuilt' = dependencyBuilt,
    rootBuilt' = true,
    exitCode' = exitCode,
  }

  action close = all {
    phase != Closed,
    phase' = Closed,
    graphValid' = graphValid,
    aDependsOnB' = aDependsOnB,
    bDependsOnA' = bDependsOnA,
    dependencyBuilt' = dependencyBuilt,
    rootBuilt' = rootBuilt,
    exitCode' = exitCode,
  }

  action step = any { invalidateGraph, plan, buildDependency, buildRoot, close }

  action faultAcceptCycle = all {
    phase == Plan,
    phase' = Compile,
    graphValid' = true,
    aDependsOnB' = true,
    bDependsOnA' = true,
    dependencyBuilt' = false,
    rootBuilt' = false,
    exitCode' = ExitSuccess,
  }

  action faultBuildRootFirst = all {
    phase == Compile,
    phase' = Compile,
    graphValid' = true,
    aDependsOnB' = true,
    bDependsOnA' = false,
    dependencyBuilt' = false,
    rootBuilt' = true,
    exitCode' = ExitSuccess,
  }

  action checkState(p) = all {
    p,
    phase' = phase,
    graphValid' = graphValid,
    aDependsOnB' = aDependsOnB,
    bDependsOnA' = bDependsOnA,
    dependencyBuilt' = dependencyBuilt,
    rootBuilt' = rootBuilt,
    exitCode' = exitCode,
  }

  run graph_boundsTest =
    init
      .then(plan)
      .then(buildDependency)
      .then(buildRoot)
      .then(checkState(inv_graph and inv_dependency_first and rootBuilt and dependencyBuilt and exitCode == ExitSuccess))

  run graph_invalidGraphTest =
    init
      .then(invalidateGraph)
      .then(plan)
      .then(checkState(phase == Closed and not(graphValid) and exitCode == ExitGraphCycle and not(rootBuilt) and not(dependencyBuilt) and inv_graph and inv_dependency_first))

  run graph_faultAcceptanceTest =
    init
      .then(faultAcceptCycle)
      .then(checkState(not(inv_graph) and inv_dependency_first and not(rootBuilt) and not(dependencyBuilt)))

  run graph_faultDependencyOrderTest =
    init
      .then(plan)
      .then(faultBuildRootFirst)
      .then(checkState(inv_graph and not(inv_dependency_first) and rootBuilt and not(dependencyBuilt)))
}
