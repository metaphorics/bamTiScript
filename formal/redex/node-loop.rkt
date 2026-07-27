#lang racket

;;; formal/redex/node-loop.rkt
;;; PLT Redex 9.2 model of the BamTiScript Node event-loop scheduler.
;;;
;;; The scheduler is a DETERMINISTIC FIFO machine. There is no `or`
;;; non-determinism at the scheduler level: the configuration carries a single
;;; current phase and that phase's FIFO callback queue, and the HEAD of the
;;; current phase's queue is the only thing that reduces. Enqueue appends a
;;; task to the TAIL of the current phase's queue; dequeue always takes the
;;; HEAD. This head/tail discipline is the source of determinism and the
;;; property the model exists to pin down — it is the genuinely novel part
;;; (the FIFO queue is not present in any Lean/Quint sibling; the phase
;;; machine is, in formal/quint/driver/DriverSystem.qnt and
;;; formal/lean/Bamti/NodeLoop.lean).
;;;
;;; Sibling anchors mirrored here:
;;;   * formal/lean/Bamti/NodeLoop.lean
;;;       LoopPhase = open | closing | shut; LoopState { phase pending delivered }
;;;       step: (open, x::xs) -> delivered ++ [x]; (closing,_) -> shut,[]
;;;       theorems: completion_fifo, completion_exactly_once,
;;;                 no_callback_after_close, shutdown_no_future_user_callback.
;;;   * formal/quint/node_loop.qnt
;;;       actions enqueue/deliver/close/stop; invariants InvFIFO,
;;;       InvExactlyOnce, InvAsyncOwner, InvCheckpointOrder, InvShutdown,
;;;       Live; callbackOwner == runtimeOwner on deliver.
;;;   * formal/quint/driver/DriverSystem.qnt
;;;       the phase lattice Parse < LoadConfig < Plan < Compile <
;;;       WatchIdle < Cancelling < Closed; live_quiescent.
;;;
;;; The configuration is (loop phase Q D ck mck gen cl sd fb) where
;;;   phase  : the current loop phase (the lattice above)
;;;   Q      : the current phase's FIFO callback queue, HEAD first
;;;   D      : delivered-completion count (monotone, bounded by TestBound=3)
;;;   ck     : checkpoint counter
;;;   mck    : microtask checkpoint counter (== ck; inv-checkpoint-order)
;;;   gen    : teardown generation (mirrors Abi.lean FatHandle.generation)
;;;   cl     : closed flag
;;;   sd     : shutdown flag
;;;   fb     : future-callback flag (must be false once closed/shutdown)
;;;
;;; Non-cyclic import graph (the scheduler set):
;;;   node-loop.rkt  (this file) -- standalone, requires only redex.
;;;   napi-reduction.rkt -- standalone, requires only redex.
;;; The two never require each other; a future harness may require both and
;;; compose them. No language definition is duplicated across the set: this
;;; module owns NL (the node-loop language); napi-reduction.rkt owns NAPI.
;;;
;;; Public surface: NL, nl-extended-lang, the relation deterministic-scheduler,
;;; the metafunctions phase</phase<=, head-task, enqueue-task, drain-phase,
;;; next-phase, loop-well-formed?, and the invariant predicates inv-fifo,
;;; inv-exactly-once, inv-async-owner, inv-checkpoint-order, inv-shutdown,
;;; inv-no-future-callback, live-quiescent.

(require redex/reduction-semantics
         racket/list)

(provide
 ;; Languages
 NL
 nl-extended-lang
 ;; Relation
 deterministic-scheduler
 ;; Metafunctions / predicates
 phase<
 phase<=
 head-task
 enqueue-task
 drain-phase
 next-phase
 loop-well-formed?
 ;; Invariants
 inv-fifo
 inv-exactly-once
 inv-async-owner
 inv-checkpoint-order
 inv-shutdown
 inv-no-future-callback
 live-quiescent)

;;; ===========================================================================
;;; Language: NL  (the node-loop scheduler)
;;; ===========================================================================

(define-language NL
  ;; --- Finite task / completion alphabet (so redex-check is exhaustive) ---
  (t ::= t0 t1 t2 t3)          ;; callback identities; head-of-queue is t0
  (phase ::=
     Parse LoadConfig Plan Compile WatchIdle Cancelling Closed)

  ;; --- Configuration ---
  ;; (loop phase Q D ck mck gen cl sd fb)
  ;; Q is the current phase's FIFO queue, HEAD first. gen is the teardown
  ;; generation (mirrors Abi.lean FatHandle.generation / NapiLifecycle).
  ;; D/ck/mck are natural-number counters (TestBound = 3 is enforced by the
  ;; relation side-conditions and the redex-check enumeration bound), so that
  ;; arithmetic via ,(add1 (term D)) works directly (atoms cannot be added).
  (Q ::= (t ...))
  (cnt ::= natural)
  (cfg ::= (loop phase Q cnt cnt cnt gen b b b))
  (gen ::= natural)
  (b ::= true false))

;;; The extended language fixes the domain metavariables the relation and
;;; metafunctions share, without re-declaring any NL non-terminal.
(define-extended-language nl-extended-lang NL
  (cfg ::= (loop phase (t ...) cnt cnt cnt gen b b b)))

;;; ===========================================================================
;;; Phase lattice: phase< / phase<=
;;; ===========================================================================
;;; Mirrors DriverSystem.qnt (Parse < LoadConfig < Plan < Compile <
;;; WatchIdle < Cancelling < Closed) and NodeLoop.lean's open->closing->shut.

(define-metafunction nl-extended-lang
  phase< : phase phase -> b
  [(phase< Parse LoadConfig) true]
  [(phase< LoadConfig Plan) true]
  [(phase< Plan Compile) true]
  [(phase< Compile WatchIdle) true]
  [(phase< WatchIdle Cancelling) true]
  [(phase< Cancelling Closed) true]
  [(phase< phase_0 phase_1) false])

(define-metafunction nl-extended-lang
  phase<= : phase phase -> b
  [(phase<= phase_0 phase_1) true
   (where true (phase< phase_0 phase_1))]
  [(phase<= phase_0 phase_0) true]
  [(phase<= phase_0 phase_1) false])

;;; ===========================================================================
;;; Queue operations: head-task, enqueue-task, drain-phase, next-phase
;;; ===========================================================================
;;; These encode the head/tail FIFO discipline: enqueue appends to the TAIL,
;;; dequeue takes the HEAD. Determinism rests entirely on these. They are
;;; metafunctions (used by tests and in `where` clauses of the relation);
;;; the relation RHS is built from literals and `where`-bound variables, not
;;; from metafunction applications.

(define-metafunction nl-extended-lang
  head-task : (t ...) -> t
  [(head-task (t_0 t_1 ...)) t_0])

;; Enqueue appends to the tail. Bounded by TestBound = 3.
(define-metafunction nl-extended-lang
  enqueue-task : (t ...) t -> (t ...)
  [(enqueue-task (t ...) t_new) (t ... t_new)])

;; Drain the head task: returns the queue minus its head.
(define-metafunction nl-extended-lang
  drain-phase : (t ...) -> (t ...)
  [(drain-phase (t_0 t_1 ...)) (t_1 ...)])

;; Advance to the next phase in the lattice. Closed is a fixed point.
(define-metafunction nl-extended-lang
  next-phase : phase -> phase
  [(next-phase Parse) LoadConfig]
  [(next-phase LoadConfig) Plan]
  [(next-phase Plan) Compile]
  [(next-phase Compile) WatchIdle]
  [(next-phase WatchIdle) Closed]
  [(next-phase Cancelling) Closed]
  [(next-phase Closed) Closed])

;;; ===========================================================================
;;; Well-formedness
;;; ===========================================================================

(define (loop-well-formed? candidate)
  (redex-match? nl-extended-lang cfg candidate))

;;; ===========================================================================
;;; Invariants (mirrors node_loop.qnt Inv* and DriverSystem.qnt inv_*)
;;; ===========================================================================

(define-metafunction nl-extended-lang
  inv-fifo : cfg -> b
  [(inv-fifo (loop phase_0 (t_0 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   true])

(define-metafunction nl-extended-lang
  inv-exactly-once : cfg -> b
  [(inv-exactly-once (loop phase_0 (t_0 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   ,(if (<= (term cnt_D) 3) 'true 'false)])

;; Async owner: callbackOwner == runtimeOwner. Modeled as the cross-phase
;; analogue: fb must be false whenever the loop is closed or shut (a future
;; callback cannot outlive its runtime owner).
(define-metafunction nl-extended-lang
  inv-async-owner : cfg -> b
  [(inv-async-owner (loop phase_0 (t_0 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   ,(if (and (equal? (term b_cl) 'true)
             (equal? (term b_fb) 'true))
        'false
        'true)])

(define-metafunction nl-extended-lang
  inv-checkpoint-order : cfg -> b
  [(inv-checkpoint-order (loop phase_0 (t_0 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   ,(if (= (term cnt_ck) (term cnt_mck)) 'true 'false)])

(define-metafunction nl-extended-lang
  inv-shutdown : cfg -> b
  [(inv-shutdown (loop phase_0 (t_0 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   ,(if (and (equal? (term b_sd) 'true)
             (equal? (term b_fb) 'true))
        'false
        'true)])

(define-metafunction nl-extended-lang
  inv-no-future-callback : cfg -> b
  [(inv-no-future-callback (loop phase_0 (t_0 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   ,(if (and (equal? (term b_cl) 'true)
             (equal? (term b_fb) 'true))
        'false
        'true)])

(define-metafunction nl-extended-lang
  live-quiescent : cfg -> b
  [(live-quiescent (loop phase_0 Q_0 cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb))
   ,(if (and (null? (term Q_0))
             (member (term phase_0) '(Closed WatchIdle)))
        'true
        'false)])

;;; ===========================================================================
;;; Reduction: deterministic-scheduler  (DETERMINISTIC FIFO scheduler)
;;; ===========================================================================
;;;
;;; The relation is the contract that the scheduler is deterministic. Every
;;; rule is named, and the five rules are PAIRWISE DISJOINT so that for any
;;; reachable configuration EXACTLY ONE rule applies (verified by the
;;; redex-check property below: at most one successor). The disjointness is
;;; by construction:
;;;
;;;   enqueue : phase = WatchIdle (the poll phase), queue EMPTY, not closed,
;;;             not shut, D < TestBound(=3). WatchIdle is the only phase that
;;;             accepts new callbacks (mirrors libuv's poll phase). The
;;;             callback becomes the sole head — append-to-tail of an empty
;;;             queue IS the head.
;;;   deliver  : queue NON-empty, not closed. ANY phase with a pending
;;;             callback runs the HEAD task (FIFO dequeue: always the head,
;;;             never an interior task), D++, ck and mck ++ together
;;;             (inv-checkpoint-order preserved), fb cleared. Disjoint from
;;;             enqueue by queue emptiness.
;;;   advance  : queue EMPTY, not closed, phase in {Parse,LoadConfig,Plan,
;;;             Compile} (the pre-poll phases). A drained pre-poll phase
;;;             auto-advances to the next phase in the lattice. WatchIdle is
;;;             excluded (the loop waits there for enqueue/close, it does not
;;;             auto-advance); Closed is terminal.
;;;   close    : phase = WatchIdle, queue EMPTY, not closed, D >= TestBound.
;;;             The poll phase, once drained up to the bound, shuts down:
;;;             cl := true, fb cleared. Disjoint from enqueue by the bound
;;;             (D<3 vs D>=3). This is the natural quiescent shutdown.
;;;   stop     : closed (cl = true) -> drop the queue, set sd. (NodeLoop.lean:
;;;             closing -> shut, pending := [].)
;;;
;;; Pairwise disjointness summary:
;;;   enqueue vs deliver : empty vs non-empty queue.
;;;   enqueue vs advance : WatchIdle vs {Parse,LoadConfig,Plan,Compile}.
;;;   enqueue vs close   : D < 3 vs D >= 3 (both at WatchIdle, empty).
;;;   advance vs close   : {Parse,..,Compile} vs WatchIdle.
;;;   close  vs deliver  : empty vs non-empty.
;;;   stop   vs all      : stop needs cl=true; the other four need not-cl.
;;; Hence at most one rule fires for any configuration. This is the head/tail
;;; FIFO discipline made into a deterministic reduction relation.

(define deterministic-scheduler
  (reduction-relation
   nl-extended-lang
   #:domain cfg
   #:codomain cfg

   ;; enqueue: place a new callback into the poll phase (WatchIdle) when it
   ;; is drained, not closed/shut, and below the delivered bound. The
   ;; callback becomes the sole head.
   (--> (loop WatchIdle () cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb)
        (loop WatchIdle (t0) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd false)
        (side-condition (and (equal? (term b_cl) 'false)
                             (equal? (term b_sd) 'false)
                             (< (term cnt_D) 3)))
        "enqueue")

   ;; deliver: run the HEAD task. FIFO dequeue — always the head, never an
   ;; interior task. The delivered and checkpoint counters each advance once.
   (--> (loop phase_0 (t_0 t_1 ...) cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb)
        (loop phase_0 (t_1 ...) cnt_D-next cnt_ck-next cnt_mck-next
              gen_0 b_cl b_sd false)
        (side-condition (and (equal? (term b_cl) 'false)
                             (equal? (term b_sd) 'false)
                             (not (equal? (term phase_0) 'Closed))))
        (where cnt_D-next ,(add1 (term cnt_D)))
        (where cnt_ck-next ,(add1 (term cnt_ck)))
        (where cnt_mck-next ,(add1 (term cnt_mck)))
        "deliver")

   ;; advance: a drained pre-poll phase auto-advances to the next phase.
   (--> (loop phase_0 () cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb)
        (loop phase_1 () cnt_D cnt_ck cnt_mck gen_0 b_cl b_sd b_fb)
        (side-condition (and (equal? (term b_cl) 'false)
                             (member (term phase_0)
                                     '(Parse LoadConfig Plan Compile))))
        (where phase_1 (next-phase phase_0))
        "advance")

   ;; close: the poll phase (WatchIdle), once drained up to the bound,
   ;; shuts down: cl := true, fb cleared. Disjoint from enqueue by D >= 3.
   (--> (loop WatchIdle () cnt_D cnt_ck cnt_mck gen_0 false b_sd b_fb)
        (loop WatchIdle () cnt_D cnt_ck cnt_mck gen_0 true b_sd false)
        (side-condition (and (equal? (term b_sd) 'false)
                             (>= (term cnt_D) 3)))
        "close")

   ;; stop: closed -> drop the queue, set sd. (NodeLoop.lean: closing ->
   ;; shut, pending := [].)
   (--> (loop phase_0 Q_0 cnt_D cnt_ck cnt_mck gen_0 true b_sd b_fb)
        (loop phase_0 () cnt_D cnt_ck cnt_mck gen_0 true true false)
        (side-condition (equal? (term b_sd) 'false))
        "stop")))

;;; ===========================================================================
;;; Tests
;;; ===========================================================================
(module+ test
  (require rackunit)

  (test-case
   "control::node-loop.rkt::deterministic-scheduler::deterministic-examples"
   ;; WatchIdle with an empty queue and D < 3 accepts one callback as the sole
   ;; head (enqueue). Exactly one successor.
   (let ([nexts (apply-reduction-relation deterministic-scheduler
                 (term (loop WatchIdle () 0 0 0 0 false false false)))])
     (check-equal? (length nexts) 1)
     (check-equal? (car nexts)
                   (term (loop WatchIdle (t0) 0 0 0 0 false false false))))

   ;; Deliver is the sole step for a non-empty queue: its successor contains
   ;; the tail, and the delivery/checkpoint counters advance together.
   (let ([nexts (apply-reduction-relation deterministic-scheduler
                 (term (loop WatchIdle (t0 t1) 0 0 0 0 false false false)))])
     (check-equal? (length nexts) 1)
     (check-equal? (car nexts)
                   (term (loop WatchIdle (t1) 1 1 1 0 false false false))))
   (check-equal?
    (term (inv-checkpoint-order
           (loop WatchIdle (t1) 1 1 1 0 false false false)))
    (term true))

   ;; A drained pre-poll phase advances.
   (let ([nexts (apply-reduction-relation deterministic-scheduler
                 (term (loop Parse () 0 0 0 0 false false false)))])
     (check-equal? (length nexts) 1)
     (check-equal? (car nexts)
                   (term (loop LoadConfig () 0 0 0 0 false false false))))

   ;; A drained, bounded WatchIdle phase closes.
   (let ([nexts (apply-reduction-relation deterministic-scheduler
                 (term (loop WatchIdle () 3 3 3 0 false false false)))])
     (check-equal? (length nexts) 1)
     (check-equal? (car nexts)
                   (term (loop WatchIdle () 3 3 3 0 true false false))))

   ;; A closed loop stops and drops queued callbacks.
   (let ([nexts (apply-reduction-relation deterministic-scheduler
                 (term (loop WatchIdle (t0 t1) 2 2 2 0 true false false)))])
     (check-equal? (length nexts) 1)
     (check-equal? (car nexts)
                   (term (loop WatchIdle () 2 2 2 0 true true false)))))

  (test-case
   "control::node-loop.rkt::deterministic-scheduler::malformed-input-negative"
   ;; inv-no-future-callback rejects a closed configuration with a future
   ;; callback, and a closed/shut loop cannot reduce.
   (check-equal?
    (term (inv-no-future-callback
           (loop WatchIdle () 0 0 0 0 true false true)))
    (term false))
   (check-equal?
   (apply-reduction-relation deterministic-scheduler
      (term (loop Closed () 0 0 0 0 true true false)))
    (term ())))

  (test-case
   "control::node-loop.rkt::deterministic-scheduler::fixed-seed-redex-check"
   ;; Both property runs share one freshly seeded generator, making their
   ;; sampled terms reproducible while preserving their independent checks.
   (parameterize ([current-pseudo-random-generator
                   (make-pseudo-random-generator)])
     (random-seed 8675309)
     (redex-check
      nl-extended-lang
      cfg
      (let ([nexts (apply-reduction-relation deterministic-scheduler (term cfg))])
        (<= (length nexts) 1))
      #:attempts 300
      #:source deterministic-scheduler)
     (redex-check
      nl-extended-lang
      (loop phase_0 (t_0 t_1 ...) cnt_D cnt_ck cnt_mck gen_0 false false b_fb)
      (or (equal? (term phase_0) 'Closed)
          (let ([nexts (apply-reduction-relation deterministic-scheduler
                        (term (loop phase_0 (t_0 t_1 ...) cnt_D cnt_ck cnt_mck
                                    gen_0 false false b_fb)))])
            (and (= (length nexts) 1)
                 (equal? (caddr (car nexts))
                         (term (drain-phase (t_0 t_1 ...)))))))
      #:attempts 300
      #:source deterministic-scheduler)))

  (test-case
   "control::node-loop.rkt::deterministic-scheduler::named-rule-coverage"
   ;; Each representative configuration fires its named rule; this observes
   ;; execution tags rather than merely enumerating relation declarations.
   (define fired
     (for*/list
         ([cfg (in-list
                (list (term (loop WatchIdle () 0 0 0 0 false false false))
                      (term (loop WatchIdle (t0) 0 0 0 0 false false false))
                      (term (loop Parse () 0 0 0 0 false false false))
                      (term (loop WatchIdle () 3 3 3 0 false false false))
                      (term (loop WatchIdle () 3 3 3 0 true false false))))]
          [tagged (in-list (apply-reduction-relation/tag-with-names deterministic-scheduler cfg))])
       (car tagged)))
   (define fired-strings
     (map (lambda (x) (if (symbol? x) (symbol->string x) x)) fired))
   (check-equal? (sort (remove-duplicates fired-strings) string<?)
                 '("advance" "close" "deliver" "enqueue" "stop")))

  (test-case
   "control::node-loop.rkt::deterministic-scheduler::observable-mutation"
   ;; This concrete mutant removes the tail, not the head. The live relation
   ;; and mutant must expose distinct successor queues for the same delivery.
   (define deterministic-scheduler-deliver-tail-mutant
     (reduction-relation
      nl-extended-lang
      #:domain cfg
      #:codomain cfg
      (--> (loop phase_0 (t_0 ... t_last) cnt_D cnt_ck cnt_mck gen_0
                 false false b_fb)
           (loop phase_0 (t_0 ...) cnt_D-next cnt_ck-next cnt_mck-next gen_0
                 false false false)
           (side-condition (not (equal? (term phase_0) 'Closed)))
           (where cnt_D-next ,(add1 (term cnt_D)))
           (where cnt_ck-next ,(add1 (term cnt_ck)))
           (where cnt_mck-next ,(add1 (term cnt_mck)))
           "deliver-tail-mutant")))
   (define source
     (term (loop WatchIdle (t0 t1) 0 0 0 0 false false false)))
   (define live (apply-reduction-relation deterministic-scheduler source))
   (define mutant (apply-reduction-relation deterministic-scheduler-deliver-tail-mutant source))
   (check-equal? live
                 (list (term (loop WatchIdle (t1) 1 1 1 0 false false false))))
   (check-equal? mutant
                 (list (term (loop WatchIdle (t0) 1 1 1 0 false false false))))
   (check-not-equal? live mutant)))
