module core {
  val Seed = 201
  val Bound = 3

  var gc: {
    live: bool,
    generation: int,
    handleGeneration: int,
    oldToYoung: bool,
    remembered: bool
  }

  val live = gc.live
  val generation = gc.generation
  val handleGeneration = gc.handleGeneration
  val oldToYoung = gc.oldToYoung
  val remembered = gc.remembered

  pure def initialGc() = {
    live: false,
    generation: 0,
    handleGeneration: 0,
    oldToYoung: false,
    remembered: false,
  }

  action init = all {
    gc' = initialGc,
  }

  action checkState = all {
    gc' = gc,
  }

  action allocate = all {
    not(live) and generation < Bound,
    gc' = gc.with("live", true).with("generation", generation + 1).with("handleGeneration", generation + 1),
  }

  action insertOldToYoung = all {
    live and not(oldToYoung),
    gc' = gc.with("oldToYoung", true).with("remembered", true),
  }

  action retire = all {
    live and generation < Bound,
    gc' = gc.with("live", false).with("generation", generation + 1),
  }

  action faultStaleHandle = all {
    live and handleGeneration < Bound,
    gc' = gc.with("handleGeneration", handleGeneration + 1),
  }

  action faultForgetOldToYoung = all {
    oldToYoung and remembered,
    gc' = gc.with("remembered", false),
  }

  action step = any { allocate, insertOldToYoung, retire }

  val TypeOK =
    generation >= 0 and generation <= Bound and
      handleGeneration >= 0 and handleGeneration <= Bound

  val ReachableResolves = not(live) or generation == handleGeneration
  val RememberedSound = not(oldToYoung) or remembered

  run core_boundsTest = init
    .then(allocate)
    .then(insertOldToYoung)
    .then(all { checkState, assert(Seed == 201 and TypeOK and live and oldToYoung and ReachableResolves and RememberedSound) })
    .then(retire)
    .then(all { checkState, assert(TypeOK and not(live)) })

  run reachableResolves_normalWitness = init
    .then(allocate)
    .then(all { checkState, assert(TypeOK and live and ReachableResolves) })

  run rememberedSound_normalWitness = init
    .then(allocate)
    .then(insertOldToYoung)
    .then(all { checkState, assert(TypeOK and oldToYoung and remembered and RememberedSound) })

  run faultReachableResolves = init
    .then(allocate)
    .then(faultStaleHandle)
    .then(all { checkState, assert(TypeOK and not(ReachableResolves) and RememberedSound) })

  run faultRememberedSound = init
    .then(allocate)
    .then(insertOldToYoung)
    .then(faultForgetOldToYoung)
    .then(all { checkState, assert(TypeOK and ReachableResolves and not(RememberedSound)) })
}
