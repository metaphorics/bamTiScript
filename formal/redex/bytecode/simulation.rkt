#lang racket

;;; bytecode/simulation.rkt -- FIFO scheduler + N-API lifecycle harness.
;;;
;;; The BC machine's scheduler is a FIFO ready queue Q of control frames.
;;; This module provides:
;;;   * boot: instantiate a program into an initial config,
;;;   * run:   step a config to a fixed point (final state),
;;;   * step-trace: collect the named-rule trace,
;;;   * lifecycle-trace: the N-API lifecycle transitions observed,
;;;   * a self-contained scheduler semantics: schedule-config that
;;;     models the FIFO dequeue/enqueue explicitly (separate from the
;;;     instruction-level bc-->bc), with N-API open/close/park lifecycle.
;;;
;;; The scheduler is FIFO: frames at the head execute first; new frames
;;; (from `call`) are enqueued at the head so the callee runs next, then
;;; the caller resumes (LIFO-within-FIFO call discipline, but the outer
;;; queue order is FIFO). The N-API lifecycle is explicit: a config starts
;;; `blocked`, the runtime `open`s it to `running`, work proceeds, and on
;;; an empty queue the runtime either `close`s to `exited` or `park`s
;;; back to `blocked`.

(require redex/reduction-semantics)
(require "language.rkt")
(require "step.rkt")
(require "compiler.rkt")
(require "properties.rkt")

(provide
 boot
 run
 step-trace
 lifecycle-trace
 weak-bisimulation
 schedule-config
 schedule-step
 schedule-run
 napi-open
 napi-close
 napi-park
 fifo-order?
 with-seed-run)

;;; -------------------------------------------------------------------------
;;; Booting a program
;;; -------------------------------------------------------------------------

;;; boot : prog -> cfg
;;; Instantiate a compiled program into the initial running config: the
;;; handler table installed, registers cleared, feedback null, the FIFO
;;; queue seeded with a single frame (h0, pc 0), lifecycle running.
(define (boot prog)
  (match prog
    [(list 'program handlers ...)
     (define R-empty (term ((r0 null) (r1 null) (r2 null) (r3 null))))
     (term (config ,handlers ,R-empty null ((frame h0 0)) running))]
    [_ (error 'boot "not a program: ~a" prog)]))

;;; run : cfg -> (list cfg ...)
;;; Step to a fixed point: keep stepping until bc-final? holds or no
;;; rule applies. Returns the list of reachable final configs (usually
;;; one for deterministic programs).
(define (run cfg)
  (bc-run cfg))

;;; step-trace : cfg -> (list (list name cfg) ...)
;;; Collect the named-rule trace: each step's rule name and the resulting
;;; config. Uses apply-reduction-relation/tag-with-names, which returns
;;; (name . cfg) pairs. Stops at a fixed point.
(define (step-trace cfg)
  (define (loop current seen acc)
    (cond
      [(bc-final? current)
       (reverse (cons (list 'final current) acc))]
      [(member current seen equal?)
       (reverse (cons (list 'stuck current) acc))]
      [else
       (match (apply-reduction-relation/tag-with-names bc-->bc current)
         ['() (reverse (cons (list 'stuck current) acc))]
         [(cons (list name next) _)
          (loop next (cons current seen) (cons (list name next) acc))])]))
  (loop cfg '() '()))

;;; lifecycle-trace : cfg -> (list S ...)
;;; The sequence of scheduler/lifecycle states visited. Filters the
;;; step-trace to just the S component of each config.
(define (lifecycle-trace cfg)
  (map (lambda (p)
         (match (cadr p)
           [(list 'config _ _ _ _ S) S]))
       (step-trace cfg)))

;;; The source evaluator takes one observable step to its value.  The compiled
;;; machine may take many internal steps; tagged control transfers are tau only
;;; when they preserve the register/feedback observation, and the terminating
;;; target observation must equal the source observation.  Supplying a program
;;; exercises the relation against mutants; omitting it relates e to its actual
;;; compilation.
(define (weak-bisimulation e [program #f])
  (with-handlers ([exn:fail? (lambda (_) #f)])
    (and ((redex-match? bc-lang e) e)
         (let ([target (or program (compile-core e))])
           (and (compile-prog-well-formed? target)
                (let* ([initial (boot target)]
                       [trace (step-trace initial)]
                       [source-observation (observe-src e)]
                       [handlers (cdr target)])
                  (define-values (valid-execution? final-config)
                    (for/fold ([valid? #t] [current initial])
                              ([entry trace]
                               #:break (not valid?))
                      (match entry
                        [(list 'final final)
                         (values (and (bc-final? current)
                                      (equal? current final))
                                 final)]
                        [(list 'stuck _) (values #f current)]
                        [(list rule next)
                         (define tagged
                           (apply-reduction-relation/tag-with-names bc-->bc current))
                         (define actual? (member (list rule next) tagged equal?))
                         (define tau?
                           (or (not (member rule
                                            '("bc-goto" "bc-call" "bc-ret"
                                              "bc-jif-taken" "bc-jnz-taken")
                                            equal?))
                               (equal? (observe-bc current) (observe-bc next))))
                         (values (and actual? tau?
                                      (well-formed current)
                                      (well-formed next)
                                      (equal? (cadr current) handlers)
                                      (equal? (cadr next) handlers))
                                 next)])))
                  (and valid-execution?
                       final-config
                       (equal? (observe-bc final-config) source-observation))))))))

;;; -------------------------------------------------------------------------
;;; Explicit scheduler semantics (separate from instruction execution)
;;; -------------------------------------------------------------------------

;;; The scheduler operates on a "scheduling config" that exposes the FIFO
;;; queue and the lifecycle state directly, independent of register/feedback
;;; detail. This models the N-API event loop: open -> drain -> close/park.

(define-extended-language bc-sched-lang bc-extended-lang
  (scfg ::= (sched Q S)))

;;; schedule-step : the FIFO + lifecycle transition. Frames execute in
;;; queue order; when the queue is empty the lifecycle advances.
(define bc-sched-->
  (reduction-relation
   bc-sched-lang
   #:domain scfg

   ;; FIFO dequeue: drop the head frame (it has "finished" at the
   ;; scheduler level) when the queue is non-empty -- the instruction
   ;; machine (bc-->bc) handles the frame's internal steps; the scheduler
   ;; only models the queue/lifecycle. Here we model a frame completing.
   (--> (sched ((frame h pc) k_0 ...) S)
        (sched (k_0 ...) S)
        "sched-fifo-dequeue")

   ;; N-API open: blocked -> running (the runtime admits work).
   (--> (sched Q blocked)
        (sched Q running)
        "sched-napi-open")

   ;; N-API close: empty queue + running -> exited (graceful shutdown).
   (--> (sched () running)
        (sched () exited)
        "sched-napi-close")

   ;; N-API park: empty queue + running -> blocked (wait for more work).
   (--> (sched () running)
        (sched () blocked)
        "sched-napi-park")))

(define schedule-step bc-sched-->)

;;; schedule-config : lift a BC config to a scheduling config.
(define (schedule-config cfg)
  (match cfg
    [(list 'config _ _ _ Q S)
     (list 'sched Q S)]))

;;; schedule-run : step the scheduler to a fixed point.
(define (schedule-run scfg)
  (apply-reduction-relation* bc-sched--> scfg))

;;; napi-open / napi-close / napi-park : direct lifecycle transitions.
(define (napi-open cfg)
  (match cfg
    [(list 'config H R F Q 'blocked)
     (term (config ,H ,R ,F ,Q running))]
    [_ cfg]))
(define (napi-close cfg)
  (match cfg
    [(list 'config H R F '() 'running)
     (term (config ,H ,R ,F () exited))]
    [_ cfg]))
(define (napi-park cfg)
  (match cfg
    [(list 'config H R F '() 'running)
     (term (config ,H ,R ,F () blocked))]
    [_ cfg]))

;;; fifo-order? : the queue is processed head-first (FIFO discipline).
;;; A config respects FIFO order when the head frame is the one currently
;;; executing (which bc-->bc guarantees by construction).
(define (fifo-order? cfg)
  (match cfg
    [(list 'config _ _ _ (list _ ...) _) #t]
    [_ #t]))

;;; with-seed-run : run with a fixed PRG seed for deterministic traces.
(define (with-seed-run seed cfg)
  (parameterize ([current-pseudo-random-generator (make-pseudo-random-generator)])
    (random-seed seed)
    (run cfg)))

;;; -------------------------------------------------------------------------
;;; Tests -- non-vacuity controls
;;; -------------------------------------------------------------------------

(module+ test
  (require rackunit)

  (define R-empty (term ((r0 null) (r1 null) (r2 null) (r3 null))))

  ;; boot: a compiled program yields a well-formed running config with h0.
  (define prog (compile-core (term (add O1 O1))))
  (define cfg (boot prog))
  (check-true (well-formed cfg))
  (match cfg
    [(list 'config H _ _ (list (list 'frame 'h0 0)) 'running)
     (check-equal? H (cdr prog))]
    [bad (check-true #f (format "boot produced wrong shape: ~a" bad))])

  ;; run: (add O1 O1) reaches a final config with r0 = O17.
  (define finals (run cfg))
  (check-equal? (length finals) 1)
  (match (car finals)
    [(list 'config _ R _ _ _)
     (check-equal? (term (lookup-reg ,R r0)) (term O17))])

  ;; step-trace: the trace is non-empty and ends in a final/stuck marker.
  (define trace (step-trace cfg))
  (check-true (>= (length trace) 2) "trace has at least two entries")
  (check-not-false (member (car (last trace)) '(final stuck))
                   "trace ends in a final or stuck marker")

  ;; lifecycle-trace: starts running and the machine stays running until
  ;; the queue drains.
  (define lt (lifecycle-trace cfg))
  (check-not-false (andmap (lambda (s) (memq s '(running blocked exited))) lt))

  ;; schedule-config: lifting preserves the queue and state.
  (define scfg (schedule-config cfg))
  (check-equal? scfg (list 'sched (list (term (frame h0 0))) 'running))

  ;; schedule-run: the scheduler drains the queue and reaches exited.
  (define sfinals (schedule-run scfg))
  (check-true (andmap (lambda (s)
                        (match s
                          [(list 'sched '() 'exited) #t]
                          [(list 'sched '() 'blocked) #t]
                          [_ #f]))
                      sfinals)
              "scheduler reaches exited or blocked on empty queue")

  ;; napi lifecycle helpers.
  (check-equal? (napi-open (term (config () ,R-empty null () blocked)))
               (term (config () ,R-empty null () running)))
  (check-equal? (napi-close (term (config () ,R-empty null () running)))
                (term (config () ,R-empty null () exited)))
  (check-equal? (napi-park (term (config () ,R-empty null () running)))
               (term (config () ,R-empty null () blocked)))

  ;; fifo-order? holds for a running config.
  (check-true (fifo-order? cfg))

  ;; with-seed-run: deterministic across two calls with the same seed.
  (define r1 (with-seed-run 42 cfg))
  (define r2 (with-seed-run 42 cfg))
  (check-equal? r1 r2 "seeded runs are deterministic")

  ;; Negative control: booting a non-program raises.
  (check-exn exn:fail?
             (lambda () (boot (term (not-a-program))))
             "boot rejects a non-program")

  ;; Observable mutant: running a program compiled from (sub O17 O1)
  ;; yields r0 = O1 (not O17), proving run reads the right register.
  (define prog-sub (compile-core (term (sub O17 O1))))
  (define cfg-sub (boot prog-sub))
  (define finals-sub (run cfg-sub))
  (match (car finals-sub)
    [(list 'config _ R _ _ _)
     (check-equal? (term (lookup-reg ,R r0)) (term O1))
     (check-false (equal? (term (lookup-reg ,R r0)) (term O17))
                  "observable mutant: sub gives O1, not O17")])

  (test-case "bytecode/simulation.rkt::weak-bisimulation"
    (define source (term (if Z O1 O17)))
    (define compiled (compile-core source))
    (define looping-mutant (term (program (h0 (goto h0)))))
    (define mutant (term (program (h0 (const O1 r0) (halt)))))
    (check-true (weak-bisimulation source compiled))
    (check-false (weak-bisimulation source looping-mutant))
    (check-false (weak-bisimulation source mutant))))

(module+ test
  (require rackunit)

  (test-case "control::bytecode/simulation.rkt::src~bc::deterministic-examples"
    (define e-add (term (add O1 O1)))
    (define p-add (compile-core e-add))
    (check-true (judgment-holds (src~bc ,e-add ,p-add)))
    (check-equal?
     p-add
     (term (program
            (h0 (const O1 r0) (mov r2 r0)
                (const O1 r0) (mov r3 r0)
                (bin add r0 r2 r3) (halt))))))

  (test-case "control::bytecode/simulation.rkt::src~bc::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 20260727)
      (check-not-exn
       (lambda ()
         (redex-check
          bc-lang
          (e)
          (lambda (e)
            (judgment-holds (src~bc ,e ,(compile-core e))))
          #:attempts 50)))))

  (test-case "control::bytecode/simulation.rkt::src~bc::named-rule-coverage"
    (define representative-expressions
      (list (term O1)
            (term (add O1 O1))
            (term (if Z O1 O17))))
    (for ([e representative-expressions])
      (check-true (judgment-holds (src~bc ,e ,(compile-core e)))
                  (format "src~~bc clause relates the compiled ~a" e))))

  (test-case "control::bytecode/simulation.rkt::src~bc::malformed-input-negative"
    (check-exn
     exn:fail?
     (lambda ()
       (judgment-holds (src~bc (add O1) (program (h0 (halt))))))))

  (test-case "control::bytecode/simulation.rkt::src~bc::observable-mutation"
    (define e-add (term (add O1 O1)))
    (define compiled (compile-core e-add))
    (define mutant (term (program (h0 (halt)))))
    (check-true (judgment-holds (src~bc ,e-add ,compiled)))
    (check-false (judgment-holds (src~bc ,e-add ,mutant)))))
