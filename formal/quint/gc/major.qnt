module major {
  val Seed = 203
  val Bound = 3
  val Idle = 0
  val Mark = 1
  val Close = 2

  var gc: {
    phase: int,
    markEpoch: int,
    ticketEpoch: int,
    issuedTickets: int,
    settledTickets: int,
    outstandingTickets: int,
    reachable: bool,
    marked: bool,
    reclaimed: bool
  }

  val phase = gc.phase
  val markEpoch = gc.markEpoch
  val ticketEpoch = gc.ticketEpoch
  val issuedTickets = gc.issuedTickets
  val settledTickets = gc.settledTickets
  val outstandingTickets = gc.outstandingTickets
  val reachable = gc.reachable
  val marked = gc.marked
  val reclaimed = gc.reclaimed

  pure def initialGc() = {
    phase: Idle,
    markEpoch: 0,
    ticketEpoch: 0,
    issuedTickets: 0,
    settledTickets: 0,
    outstandingTickets: 0,
    reachable: true,
    marked: false,
    reclaimed: false,
  }

  action init = all {
    gc' = initialGc,
  }

  action checkState = all {
    gc' = gc,
  }

  action startMark = all {
    phase == Idle and markEpoch < Bound,
    gc' = gc.with("phase", Mark).with("markEpoch", markEpoch + 1),
  }

  action shade = all {
    phase == Mark and reachable and outstandingTickets < Bound and issuedTickets < Bound,
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
    phase == Mark and outstandingTickets == 0 and (not(reachable) or marked),
    gc' = gc.with("phase", Close),
  }

  action dropRoot = all {
    phase == Close and reachable,
    gc' = gc.with("reachable", false),
  }

  action reclaimUnreachable = all {
    phase == Close and not(reachable),
    gc' = gc.with("reclaimed", true),
  }

  action faultDuplicateTicket = all {
    phase == Mark and outstandingTickets > 0 and outstandingTickets < Bound,
    gc' = gc.with("outstandingTickets", outstandingTickets + 1),
  }

  action faultCloseWithOutstandingTicket = all {
    phase == Mark and outstandingTickets > 0,
    gc' = gc.with("phase", Close),
  }

  action faultReclaimReachable = all {
    phase == Close and reachable and marked and not(reclaimed),
    gc' = gc.with("reclaimed", true),
  }

  action step = any { startMark, shade, settleTicket, closeMark, dropRoot, reclaimUnreachable }

  val TypeOK =
    phase >= Idle and phase <= Close and
      markEpoch >= 0 and markEpoch <= Bound and
      ticketEpoch >= 0 and ticketEpoch <= Bound and
      issuedTickets >= 0 and issuedTickets <= Bound and
      settledTickets >= 0 and settledTickets <= Bound and
      outstandingTickets >= 0 and outstandingTickets <= Bound

  val ShadeTicketExact =
    outstandingTickets == issuedTickets - settledTickets and
      settledTickets <= issuedTickets and
      (outstandingTickets == 0 or (ticketEpoch == markEpoch and marked))
  val CloseQuiescent = phase != Close or outstandingTickets == 0
  val NoFalseReclamationAtClose = phase != Close or not(reachable) or (marked and not(reclaimed))

  run major_boundsTest = init
    .then(startMark)
    .then(shade)
    .then(all { checkState, assert(Seed == 203 and TypeOK and outstandingTickets == 1 and ShadeTicketExact) })
    .then(settleTicket)
    .then(closeMark)
    .then(all { checkState, assert(TypeOK and phase == Close and CloseQuiescent and NoFalseReclamationAtClose) })
    .then(dropRoot)
    .then(reclaimUnreachable)
    .then(all { checkState, assert(TypeOK and NoFalseReclamationAtClose) })

  run shadeTicketExact_normalWitness = init
    .then(startMark)
    .then(shade)
    .then(all { checkState, assert(TypeOK and outstandingTickets > 0 and ShadeTicketExact) })

  run closeQuiescent_normalWitness = init
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(all { checkState, assert(TypeOK and phase == Close and CloseQuiescent) })

  run noFalseReclamationAtClose_normalWitness = init
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(all { checkState, assert(TypeOK and phase == Close and reachable and marked and NoFalseReclamationAtClose) })

  run faultShadeTicketExact = init
    .then(startMark)
    .then(shade)
    .then(faultDuplicateTicket)
    .then(all { checkState, assert(TypeOK and not(ShadeTicketExact) and CloseQuiescent and NoFalseReclamationAtClose) })

  run faultCloseQuiescent = init
    .then(startMark)
    .then(shade)
    .then(faultCloseWithOutstandingTicket)
    .then(all { checkState, assert(TypeOK and ShadeTicketExact and not(CloseQuiescent) and NoFalseReclamationAtClose) })

  run faultNoFalseReclamationAtClose = init
    .then(startMark)
    .then(shade)
    .then(settleTicket)
    .then(closeMark)
    .then(faultReclaimReachable)
    .then(all { checkState, assert(TypeOK and ShadeTicketExact and CloseQuiescent and not(NoFalseReclamationAtClose)) })
}
