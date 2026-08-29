module temporal_zone_resolution {
  val Seed = 707

  // Ordered representatives preserve the complete Temporal/Jiff partition and
  // both dependency boundaries without asking Quint to enumerate 1.7e22 values.
  val TemporalMin = -3
  val JiffMin = -1
  val JiffMax = 1
  val TemporalMax = 3

  val NoHostObservation = 0
  val NamedHostZone = 1
  val UtcHostZone = 2
  val FixedHostZone = 3
  val UnnamedHostZone = 4
  val FailedHostDetection = 5

  val NoResolution = 0
  val NamedResolution = 1
  val UtcResolution = 2
  val FixedResolution = 3
  val UnnamedSystemZoneError = 4
  val DetectionFailedError = 5

  val NoEngine = 0
  val JiffEngine = 1
  val TzifWalkerEngine = 2

  type ZoneState = {
    hostObservation: int,
    resolution: int,
    offsetSign: int,
    offsetHours: int,
    offsetMinutes: int,
    offsetSeconds: int,
    offsetHasSeconds: bool,
    namedRequest: bool,
    epochNanoseconds: int,
    engine: int,
    rangeRejected: bool,
    answered: bool,
    jiffOffset: int,
    walkerOffset: int,
    selectedOffset: int,
  }

  var state: ZoneState

  pure def initialState(): ZoneState = {
    hostObservation: NoHostObservation,
    resolution: NoResolution,
    offsetSign: 0,
    offsetHours: 0,
    offsetMinutes: 0,
    offsetSeconds: 0,
    offsetHasSeconds: false,
    namedRequest: false,
    epochNanoseconds: 0,
    engine: NoEngine,
    rangeRejected: false,
    answered: false,
    jiffOffset: 0,
    walkerOffset: 0,
    selectedOffset: 0,
  }

  action init = all {
    state' = initialState,
  }

  pure def sharedTransitionOffset(epoch: int): int =
    if (epoch < 0) -18000 else -14400

  pure def validTemporalEpoch(epoch: int): bool =
    epoch >= TemporalMin and epoch <= TemporalMax

  pure def inJiffRange(epoch: int): bool =
    epoch >= JiffMin and epoch <= JiffMax

  def hostResult(observation: int, resolution: int): ZoneState =
    state.with("hostObservation", observation)
      .with("resolution", resolution)
      .with("namedRequest", false)
      .with("engine", NoEngine)
      .with("rangeRejected", false)
      .with("answered", true)
      .with("jiffOffset", 0)
      .with("walkerOffset", 0)
      .with("selectedOffset", 0)

  action resolveNamedHost = all {
    state' = hostResult(NamedHostZone, NamedResolution),
  }

  action resolveUtcHost = all {
    state' = hostResult(UtcHostZone, UtcResolution),
  }

  // +05:30: seconds are absent exactly when the remainder is zero.
  action resolveFixedMinuteHost = all {
    state' = hostResult(FixedHostZone, FixedResolution)
      .with("offsetSign", 1)
      .with("offsetHours", 5)
      .with("offsetMinutes", 30)
      .with("offsetSeconds", 0)
      .with("offsetHasSeconds", false),
  }

  // -05:30:45 preserves a historical sub-minute fixed offset.
  action resolveFixedSecondHost = all {
    state' = hostResult(FixedHostZone, FixedResolution)
      .with("offsetSign", -1)
      .with("offsetHours", 5)
      .with("offsetMinutes", 30)
      .with("offsetSeconds", 45)
      .with("offsetHasSeconds", true),
  }

  action resolveUnnamedHost = all {
    state' = hostResult(UnnamedHostZone, UnnamedSystemZoneError),
  }

  action resolveFailedHost = all {
    state' = hostResult(FailedHostDetection, DetectionFailedError),
  }

  def namedResult(epoch: int, selectedEngine: int): ZoneState =
    state.with("hostObservation", NoHostObservation)
      .with("resolution", NoResolution)
      .with("offsetSign", 0)
      .with("offsetHours", 0)
      .with("offsetMinutes", 0)
      .with("offsetSeconds", 0)
      .with("offsetHasSeconds", false)
      .with("namedRequest", true)
      .with("epochNanoseconds", epoch)
      .with("engine", selectedEngine)
      .with("rangeRejected", false)
      .with("answered", true)
      .with("jiffOffset", sharedTransitionOffset(epoch))
      .with("walkerOffset", sharedTransitionOffset(epoch))
      .with("selectedOffset", sharedTransitionOffset(epoch))

  action requestTemporalMin = all {
    state' = namedResult(TemporalMin, TzifWalkerEngine),
  }

  action requestBeforeJiffMin = all {
    state' = namedResult(JiffMin - 1, TzifWalkerEngine),
  }

  action requestJiffMin = all {
    state' = namedResult(JiffMin, JiffEngine),
  }

  action requestJiffInterior = all {
    state' = namedResult(0, JiffEngine),
  }

  action requestJiffMax = all {
    state' = namedResult(JiffMax, JiffEngine),
  }

  action requestAfterJiffMax = all {
    state' = namedResult(JiffMax + 1, TzifWalkerEngine),
  }

  action requestTemporalMax = all {
    state' = namedResult(TemporalMax, TzifWalkerEngine),
  }

  // Fault actions are excluded from step and witness one violated obligation.
  action faultSilentUtcFallback = all {
    state' = hostResult(UnnamedHostZone, UtcResolution),
  }

  action faultNonCanonicalFixedOffset = all {
    state' = hostResult(FixedHostZone, FixedResolution)
      .with("offsetSign", -1)
      .with("offsetHours", 5)
      .with("offsetMinutes", 30)
      .with("offsetSeconds", 0)
      .with("offsetHasSeconds", true),
  }

  action faultRejectValidNamedRange = all {
    state' = namedResult(TemporalMax, NoEngine)
      .with("rangeRejected", true)
      .with("answered", false),
  }

  action faultOverlappingEngines = all {
    state' = namedResult(0, NoEngine),
  }

  action faultDifferentialDisagreement = all {
    state' = namedResult(0, JiffEngine)
      .with("walkerOffset", sharedTransitionOffset(0) + 1),
  }

  action faultIgnoreTransitionTable = all {
    state' = namedResult(TemporalMax, TzifWalkerEngine)
      .with("selectedOffset", sharedTransitionOffset(TemporalMax) + 1),
  }

  // Fault: a named host observed but answered with the Utc resolution is the
  // recorded host-mapping defect.
  action faultHostMismatchResolution = all {
    state' = hostResult(NamedHostZone, UtcResolution),
  }

  // Fault injection remains outside this normal transition relation.
  action step = any {
    resolveNamedHost,
    resolveUtcHost,
    resolveFixedMinuteHost,
    resolveFixedSecondHost,
    resolveUnnamedHost,
    resolveFailedHost,
    requestTemporalMin,
    requestBeforeJiffMin,
    requestJiffMin,
    requestJiffInterior,
    requestJiffMax,
    requestAfterJiffMax,
    requestTemporalMax,
  }

  val HostResolutionExact =
    (state.hostObservation == NoHostObservation and state.resolution == NoResolution) or
    (state.hostObservation == NamedHostZone and state.resolution == NamedResolution) or
    (state.hostObservation == UtcHostZone and state.resolution == UtcResolution) or
    (state.hostObservation == FixedHostZone and state.resolution == FixedResolution) or
    (state.hostObservation == UnnamedHostZone and state.resolution == UnnamedSystemZoneError) or
    (state.hostObservation == FailedHostDetection and state.resolution == DetectionFailedError)

  val NoSilentUtcFallback =
    state.resolution != UtcResolution or state.hostObservation == UtcHostZone

  val CanonicalFixedOffset =
    state.resolution != FixedResolution or
      ((state.offsetSign == -1 or state.offsetSign == 1) and
       state.offsetHours >= 0 and state.offsetHours <= 23 and
       state.offsetMinutes >= 0 and state.offsetMinutes <= 59 and
       state.offsetSeconds >= 0 and state.offsetSeconds <= 59 and
       state.offsetHasSeconds == (state.offsetSeconds != 0))

  val NamedDomainTotal =
    not(state.namedRequest) or
      (validTemporalEpoch(state.epochNanoseconds) and state.answered and
       not(state.rangeRejected))

  val ExactlyOneNamedEngine =
    not(state.namedRequest) or
      ((inJiffRange(state.epochNanoseconds) and state.engine == JiffEngine) or
       (not(inJiffRange(state.epochNanoseconds)) and state.engine == TzifWalkerEngine))

  val DifferentialAgreement =
    not(state.namedRequest and inJiffRange(state.epochNanoseconds)) or
      (state.jiffOffset == state.walkerOffset and
       state.jiffOffset == sharedTransitionOffset(state.epochNanoseconds))

  val NamedOffsetFromSharedTable =
    not(state.namedRequest) or
      state.selectedOffset == sharedTransitionOffset(state.epochNanoseconds)

  val live_resolved = state.answered

  val AllSafety =
    HostResolutionExact and NoSilentUtcFallback and CanonicalFixedOffset and
      NamedDomainTotal and ExactlyOneNamedEngine and DifferentialAgreement and
      NamedOffsetFromSharedTable

  action checkState(p) = all {
    p,
    state' = state,
  }

  run zone_boundsTest =
    init.then(requestJiffMin)
      .then(checkState(state.answered and state.engine == JiffEngine and
        NamedDomainTotal and ExactlyOneNamedEngine and DifferentialAgreement))
  run hostResolutionNormalWitness = init
    .then(resolveNamedHost)
    .then(checkState(state.answered and HostResolutionExact and NoSilentUtcFallback))
    .then(resolveUtcHost)
    .then(checkState(state.resolution == UtcResolution and AllSafety))
    .then(resolveUnnamedHost)
    .then(checkState(state.resolution == UnnamedSystemZoneError and AllSafety))
    .then(resolveFailedHost)
    .then(checkState(state.resolution == DetectionFailedError and AllSafety))

  run canonicalFixedOffsetNormalWitness = init
    .then(resolveFixedMinuteHost)
    .then(checkState(not(state.offsetHasSeconds) and CanonicalFixedOffset))
    .then(resolveFixedSecondHost)
    .then(checkState(state.offsetHasSeconds and state.offsetSeconds == 45 and AllSafety))

  run namedRangeNormalWitness = init
    .then(requestTemporalMin)
    .then(checkState(state.engine == TzifWalkerEngine and NamedDomainTotal))
    .then(requestBeforeJiffMin)
    .then(checkState(state.engine == TzifWalkerEngine and AllSafety))
    .then(requestJiffMin)
    .then(checkState(state.engine == JiffEngine and DifferentialAgreement))
    .then(requestJiffInterior)
    .then(checkState(state.engine == JiffEngine and AllSafety))
    .then(requestJiffMax)
    .then(checkState(state.engine == JiffEngine and DifferentialAgreement))
    .then(requestAfterJiffMax)
    .then(checkState(state.engine == TzifWalkerEngine and AllSafety))
    .then(requestTemporalMax)
    .then(checkState(state.engine == TzifWalkerEngine and NamedDomainTotal and AllSafety))

  run noSilentUtcFallbackNormalWitness = init
    .then(resolveUtcHost)
    .then(checkState(state.resolution == UtcResolution and NoSilentUtcFallback and AllSafety))

  run exactlyOneNamedEngineNormalWitness = init
    .then(requestJiffInterior)
    .then(checkState(state.engine == JiffEngine and ExactlyOneNamedEngine and AllSafety))

  run differentialAgreementNormalWitness = init
    .then(requestJiffMin)
    .then(checkState(state.engine == JiffEngine and DifferentialAgreement and AllSafety))

  run namedOffsetFromSharedTableNormalWitness = init
    .then(requestTemporalMax)
    .then(checkState(state.engine == TzifWalkerEngine and NamedOffsetFromSharedTable and AllSafety))

  run faultSilentUtcFallbackRun = init
    .then(faultSilentUtcFallback)
    .then(checkState(not(HostResolutionExact) and not(NoSilentUtcFallback)))

  run faultNonCanonicalFixedOffsetRun = init
    .then(faultNonCanonicalFixedOffset)
    .then(checkState(not(CanonicalFixedOffset) and HostResolutionExact))

  run faultRejectValidNamedRangeRun = init
    .then(faultRejectValidNamedRange)
    .then(checkState(not(NamedDomainTotal) and not(ExactlyOneNamedEngine)))

  run faultOverlappingEnginesRun = init
    .then(faultOverlappingEngines)
    .then(checkState(not(ExactlyOneNamedEngine) and NamedDomainTotal))

  run faultDifferentialDisagreementRun = init
    .then(faultDifferentialDisagreement)
    .then(checkState(not(DifferentialAgreement) and ExactlyOneNamedEngine))

  run faultIgnoreTransitionTableRun = init
    .then(faultIgnoreTransitionTable)
    .then(checkState(not(NamedOffsetFromSharedTable) and NamedDomainTotal))

  run faultHostMismatchResolutionRun = init
    .then(faultHostMismatchResolution)
    .then(checkState(not(HostResolutionExact) and not(NoSilentUtcFallback)))
}
