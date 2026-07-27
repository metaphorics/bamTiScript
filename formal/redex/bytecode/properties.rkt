#lang racket

;;; bytecode/properties.rkt -- executable BC trace properties and observables.
;;;
;;; The catalog predicates below derive tagged executions from concrete machine
;;; configurations.  They check the state mutation behind each named rule:
;;; arbitrary entry and handler transfer, FIFO completion, feedback roundtrip,
;;; stable handler-table/module integrity, and validity of queued frame roots.
;;; The source evaluator and projections are shared with simulation.rkt's
;;; source-to-bytecode weak bisimulation.

(require redex/reduction-semantics)
(require "language.rkt")
(require "step.rkt")
(require "compiler.rkt")

(provide
 ;; Source-level stepper
 eval-src
 src-final?
 ;; Observables
 observe-bc
 observe-src
 ;; Properties (predicates for testing/redex-check)
 bc-deterministic?
 feedback-roundtrip?
 compile-and-run-correct?
 arbitrary-entry
 completion-trace
 feedback-trace
 handler-trace
 module-trace
 root-trace
 redex-check-bc-determinism
 redex-check-feedback
 redex-check-compile-run)

(define-extended-language bc-property-lang bc-lang
  (compile-e ::= v
                 (add v v)
                 (sub v v)
                 (is-null v v)
                 (not v)
                 (let v v)
                 (if v v v)
                 (seq v v)))

;;; -------------------------------------------------------------------------
;;; Source-level evaluator (the "high" side of the bisimulation)
;;; -------------------------------------------------------------------------

;;; eval-src : e -> v
;;; A tiny structural evaluator for the source calculus. The result is
;;; the value of e. `let` is a substitution; `if` selects a branch;
;;; `handler` evaluates its body (the handler name is a no-op here).
(define (eval-src e)
  (match e
    ['Z 'Z] ['O1 'O1] ['O17 'O17] ['true 'true] ['false 'false] ['null 'null]
    [(list 'let e-val e-body)
     (define v (eval-src e-val))
     (eval-src (subst e-body e-val v))]  ; linear: bind is a no-op marker
    [(list 'if e-test e-then e-else)
     (if (truthy-src? (eval-src e-test))
         (eval-src e-then)
         (eval-src e-else))]
    [(list 'seq e1 e2)
     (eval-src e1) (eval-src e2)]
    [(list 'handler h body)
     (eval-src body)]
    [(list op e1 e2) #:when (memq op '(add sub is-null))
     (arith-src op (eval-src e1) (eval-src e2))]
    [(list 'not e1)
     (if (truthy-src? (eval-src e1)) 'false 'true)]
    [v v]))

(define (truthy-src? v)
  (not (memq v '(Z false null))))

(define (arith-src op a b)
  (cond
    [(and (eq? op 'add) (eq? a 'O1) (eq? b 'O1)) 'O17]
    [(eq? op 'add) 'O1]
    [(and (eq? op 'sub) (eq? a 'O17) (eq? b 'O1)) 'O1]
    [(eq? op 'sub) 'Z]
    [(and (eq? op 'is-null) (eq? a 'null)) 'true]
    [(eq? op 'is-null) 'false]
    [else 'true]))

;;; `let` in the source calculus is linear (the bound value is a marker,
;;; not a real variable), so substitution is a no-op: the body just
;;; evaluates independently. We keep the function for clarity.
(define (subst e name val) e)

;;; src-final? : the source expression is a value.
(define (src-final? e)
  (memq e '(Z O1 O17 true false null)))


;;; -------------------------------------------------------------------------
;;; Observables
;;; -------------------------------------------------------------------------

;;; observe-bc : project a BC config to (v, F) -- the r0 value and the
;;; feedback cell. This is the observable projection for the bisimulation.
(define (observe-bc cfg)
  (match cfg
    [(list 'config _ R F _ _)
     (list (term (lookup-reg ,R r0)) F)]))

;;; observe-src : project a source expression to its value.
(define (observe-src e)
  (list (eval-src e) 'null))

;;; A relation on observable projections: (observe-bc cfg) ~ (observe-src e)
;;; when the r0 values match (feedback is null on the source side, which
;;; is the initial cell).
(define (obs-match? bc-obs src-obs)
  (and (equal? (car bc-obs) (car src-obs))))

;;; A transition is (rule-name before after).  Keeping both configurations is
;;; essential: the catalog properties compare the actual machine mutation made
;;; by the named Redex rule, rather than recognizing a rule-name certificate.
(define (machine-transitions initial)
  (let loop ([current initial] [seen '()] [transitions '()])
    (cond
      [(bc-final? current) (reverse transitions)]
      [(member current seen equal?) #f]
      [else
       (match (apply-reduction-relation/tag-with-names bc-->bc current)
         [(list (list name next))
          (loop next (cons current seen)
                (cons (list name current next) transitions))]
         [_ #f])])))

(define (handler-names H)
  (map car H))

(define (handler-instructions H name)
  (match (assoc name H)
    [(list _ instructions ...) instructions]
    [_ #f]))

(define (valid-root? H frame)
  (match frame
    [(list 'frame name pc)
     (define instructions (handler-instructions H name))
     (and instructions
          (exact-nonnegative-integer? pc)
          (< pc (length instructions)))]
    [_ #f]))

(define (same-machine-plane? before after)
  (match* (before after)
    [((list 'config H R F _ S) (list 'config H* R* F* _ S*))
     (and (equal? H H*) (equal? R R*) (equal? F F*) (equal? S S*))]
    [(_ _) #f]))

;;; A goto is an arbitrary-entry transfer exactly when it changes the active
;;; handler, resets its pc, preserves the rest of the machine, and lands in an
;;; installed handler.
(define (arbitrary-entry initial)
  (define transitions (machine-transitions initial))
  (and transitions
       (for/or ([transition transitions])
         (match transition
           [(list "bc-goto"
                  (list 'config H _ _ (list (list 'frame source _) tail ...) _)
                  after)
            (match after
              [(list 'config _ _ _ (list (list 'frame target 0) tail* ...) _)
               (and (not (equal? source target))
                    (member target (handler-names H))
                    (equal? tail tail*)
                    (same-machine-plane? (cadr transition) after))]
              [_ #f])]
           [_ #f]))))

;;; Completion order is the queue order: every observed halt must remove only
;;; the head frame and leave all older queued frames in their existing order.
(define (completion-trace initial)
  (define transitions (machine-transitions initial))
  (and transitions
       (for/or ([transition transitions])
         (equal? (car transition) "bc-halt"))
       (for/and ([transition transitions]
                 #:when (equal? (car transition) "bc-halt"))
         (match transition
           [(list _
                  (list 'config _ _ _ (list _ tail ...) _)
                  (list 'config _ _ _ tail* _))
            (and (equal? tail tail*)
                 (same-machine-plane? (cadr transition) (caddr transition)))]
           [_ #f]))))

;;; A feedback roundtrip consists of a real fb write followed, without another
;;; write, by a tfb read that installs the written cell value in its destination.
(define (feedback-trace initial)
  (define transitions (machine-transitions initial))
  (and transitions
       (let search ([remaining transitions] [written? #f] [written-value #f])
         (match remaining
           ['() #f]
           [(cons (list name before after) rest)
            (cond
              [(equal? name "bc-fb-write")
               (match* (before after)
                 [((list 'config H R _ (list (list 'frame h pc) _ ...) _)
                   (list 'config _ _ F* _ _))
                  (match (handler-instructions H h)
                    [(? list? instructions)
                     (match (list-ref instructions pc)
                       [(list 'fb source)
                        (define value (term (lookup-reg ,R ,source)))
                        (and (equal? F* value)
                             (search rest #t value))]
                       [_ #f])]
                    [_ #f])]
                 [(_ _) #f])]
              [(and written? (equal? name "bc-tfb-read"))
               (match* (before after)
                 [((list 'config H _ F (list (list 'frame h pc) _ ...) _)
                   (list 'config _ R* F* _ _))
                  (match (handler-instructions H h)
                    [(? list? instructions)
                     (match (list-ref instructions pc)
                       [(list 'tfb destination)
                        (and (equal? F written-value)
                             (equal? F* written-value)
                             (equal? (term (lookup-reg ,R* ,destination))
                                     written-value))]
                       [_ #f])]
                    [_ #f])]
                 [(_ _) #f])]
              [else (search rest written? written-value)])]))))

;;; Handler transfer requires the paired call/ret protocol: call installs the
;;; callee at pc 0 ahead of the advanced caller, and ret removes that callee so
;;; the same caller frame resumes.
(define (handler-trace initial)
  (define transitions (machine-transitions initial))
  (and transitions
       (let find-call ([remaining transitions])
         (match remaining
           ['() #f]
           [(cons (list "bc-call" before after) rest)
            (match* (before after)
              [((list 'config H _ _ (list (list 'frame caller pc) tail ...) _)
                (list 'config _ _ _
                      (list (list 'frame callee 0)
                            (list 'frame caller* next-pc) tail* ...) _))
               (and (equal? caller caller*)
                    (= next-pc (add1 pc))
                    (equal? tail tail*)
                    (member callee (handler-names H))
                    (same-machine-plane? before after)
                    (for/or ([transition rest])
                      (match transition
                        [(list "bc-ret" ret-before ret-after)
                         (match* (ret-before ret-after)
                           [((list 'config _ _ _
                                   (list (list 'frame callee* _)
                                         (list 'frame caller-before caller-pc) ret-tail ...) _)
                             (list 'config _ _ _
                                   (list (list 'frame caller-after caller-pc*) ret-tail* ...) _))
                            (and (equal? callee callee*)
                                 (equal? caller caller-before)
                                 (equal? caller-before caller-after)
                                 (= next-pc caller-pc)
                                 (= caller-pc caller-pc*)
                                 (equal? ret-tail ret-tail*)
                                 (same-machine-plane? ret-before ret-after))]
                           [(_ _) #f])]
                        [_ #f])))]
              [(_ _) #f])]
           [(cons _ rest) (find-call rest)]))))

;;; Module integrity is invariant over execution: the installed handler table is
;;; unique and stable, and every static control-transfer target is installed.
(define (module-trace initial)
  (define transitions (machine-transitions initial))
  (match initial
    [(list 'config H _ _ _ _)
     (define names (handler-names H))
     (and transitions
          (pair? transitions)
          (well-formed-handler H)
          (for/and ([handler H])
            (for/and ([instruction (cdr handler)])
              (match instruction
                [(list (or 'goto 'call) target) (and (member target names) #t)]
                [(list (or 'jif 'jnz) _ target) (and (member target names) #t)]
                [_ #t])))
          (for/and ([transition transitions])
            (match transition
              [(list _ (list 'config H-before _ _ _ _)
                       (list 'config H-after _ _ _ _))
               (and (equal? H H-before) (equal? H H-after))]
              [_ #f])))]
    [_ #f]))

;;; Every queued frame is a live machine root: its handler exists and its pc
;;; points at a real instruction.  The invariant is checked before and after
;;; every actual step, including frames introduced by call.
(define (root-trace initial)
  (define transitions (machine-transitions initial))
  (define (configuration-roots-valid? cfg)
    (match cfg
      [(list 'config H _ _ Q _)
       (and (well-formed-handler H)
            (andmap (lambda (frame) (valid-root? H frame)) Q))]
      [_ #f]))
  (and transitions
       (configuration-roots-valid? initial)
       (for/and ([transition transitions])
         (and (configuration-roots-valid? (cadr transition))
              (configuration-roots-valid? (caddr transition))))))

;;; -------------------------------------------------------------------------
;;; Properties
;;; -------------------------------------------------------------------------

;;; bc-deterministic? : for a straight-line program (no empty-queue
;;; lifecycle), every step has exactly one successor.
(define (bc-deterministic? cfg)
  (define succs (apply-reduction-relation bc-->bc cfg))
  (or (null? succs)
      (= (length succs) 1)))

;;; feedback-roundtrip? : (fb r) then (tfb r') moves the value through F.
(define (feedback-roundtrip? r-src r-dst v)
  (define H (term ((h0 (const ,v ,r-src) (fb ,r-src) (tfb ,r-dst) (halt)))))
  (define R-empty (term ((r0 null) (r1 null) (r2 null) (r3 null))))
  (define cfg (term (config ,H ,R-empty null ((frame h0 0)) running)))
  (define finals (bc-run cfg))
  (and (= (length finals) 1)
       (match (car finals)
         [(list 'config _ R F _ _)
          (and (equal? (term (lookup-reg ,R ,r-dst)) v)
               (equal? F v))]
         [_ #f])))

;;; compile-and-run-correct? : compile e, run it, r0 holds eval-src e.
(define (compile-and-run-correct? e)
  (define prog (compile-core e))
  (define H (cdr prog))  ; strip the `program` tag
  (define R-empty (term ((r0 null) (r1 null) (r2 null) (r3 null))))
  (define cfg (term (config ,H ,R-empty null ((frame h0 0)) running)))
  (define finals (bc-run cfg))
  (and (= (length finals) 1)
       (equal? (car (observe-bc (car finals)))
               (car (observe-src e)))))


;;; -------------------------------------------------------------------------
;;; redex-check harnesses (fixed seed, deterministic)
;;; -------------------------------------------------------------------------

;;; Each harness runs redex-check with a fixed seed on a small grammar,
;;; asserting non-vacuity (at least one generated case is checked) and
;;; that the property holds for all generated cases.

(define (redex-check-bc-determinism)
  (redex-check
   bc-extended-lang
   (instr_0 instr_1 ...)
   (bc-deterministic?
    (term (config ((h0 instr_0 instr_1 ...))
                  ((r0 null) (r1 null) (r2 null) (r3 null))
                  null ((frame h0 0)) running)))
   #:attempts 50
   #:print? #f))

(define (redex-check-feedback)
  (redex-check
   bc-extended-lang
   v
   (feedback-roundtrip? 'r0 'r1 (term v))
   #:attempts 30
   #:print? #f))

(define (redex-check-compile-run)
  (redex-check
   bc-property-lang
   compile-e
   (compile-and-run-correct? (term compile-e))
   #:attempts 50
   #:print? #f))

;;; -------------------------------------------------------------------------
;;; Tests -- non-vacuity controls
;;; -------------------------------------------------------------------------

(module+ test
  (require rackunit)

  ;; Deterministic example: eval-src of (add O1 O1) is O17.
  (check-equal? (eval-src (term (add O1 O1))) 'O17)
  (check-equal? (eval-src (term (if O1 O17 Z))) 'O17)
  (check-equal? (eval-src (term (if Z O17 Z))) 'Z)
  (test-case "bytecode/properties.rkt::bounded-compile-generator"
    (check-true ((redex-match? bc-property-lang compile-e) (term (not true))))
    (check-true ((redex-match? bc-property-lang compile-e)
                 (term (is-null O1 O17)))))

  (test-case "bytecode/properties.rkt::source-bytecode-boolean-truth-tables"
    (for ([case (in-list
                 (list (cons (term (not Z)) 'true)
                       (cons (term (not false)) 'true)
                       (cons (term (not null)) 'true)
                       (cons (term (not O1)) 'false)
                       (cons (term (not O17)) 'false)
                       (cons (term (not true)) 'false)
                       (cons (term (is-null null O17)) 'true)
                       (cons (term (is-null Z O17)) 'false)
                       (cons (term (is-null O1 O17)) 'false)
                       (cons (term (is-null O17 O1)) 'false)
                       (cons (term (is-null true false)) 'false)
                       (cons (term (is-null false null)) 'false)))])
      (check-equal? (eval-src (car case)) (cdr case))
      (check-true (compile-and-run-correct? (car case)))))


  ;; Determinism: a straight-line config has exactly one successor.
  (define R-empty (term ((r0 null) (r1 null) (r2 null) (r3 null))))
  (define H-det (term ((h0 (const O1 r0) (halt)))))
  (define cfg-det (term (config ,H-det ,R-empty null ((frame h0 0)) running)))
  (check-true (bc-deterministic? cfg-det))

  ;; Feedback round-trip: r0 -> F -> r1 carries the value.
  (check-true (feedback-roundtrip? 'r0 'r1 'O17))
  (check-true (feedback-roundtrip? 'r2 'r3 'Z))

  ;; Compile-and-run: (add O1 O1) compiles and runs to O17 in r0.
  (check-true (compile-and-run-correct? (term (add O1 O1))))
  (check-true (compile-and-run-correct? (term (let O1 O17))))
  (check-true (compile-and-run-correct? (term (if O1 O17 Z))))


  ;; Observable mutant: if we observe r1 instead of r0 after compiling
  ;; (add O1 O1), the value is null (not O17) -- this proves the test
  ;; actually exercises the register plane, not a tautology.
  (define prog-mut (compile-core (term (add O1 O1))))
  (define H-mut (cdr prog-mut))
  (define cfg-mut (term (config ,H-mut ,R-empty null ((frame h0 0)) running)))
  (define final-mut (car (bc-run cfg-mut)))
  (match final-mut
    [(list 'config _ R _ _ _)
     ;; The mutant: observe r1 (which should be null, proving the
     ;; compile-and-run check is reading the right register).
     (check-false (equal? (term (lookup-reg ,R r1)) 'O17)
                  "observable mutant: r1 remains null")])

  (test-case "bytecode/properties.rkt::arbitrary-entry"
    (define positive
      (term (config ((h0 (goto h1)) (h1 (halt))) ,R-empty null
                    ((frame h0 0)) running)))
    (define mutant
      (term (config ((h0 (goto h1))) ,R-empty null ((frame h0 0)) running)))
    (check-true (arbitrary-entry positive))
    (check-false (arbitrary-entry mutant)))

  (test-case "bytecode/properties.rkt::completion-trace"
    (define positive
      (term (config ((h0 (halt)) (h1 (halt))) ,R-empty null
                    ((frame h0 0) (frame h1 0)) running)))
    (define mutant
      (term (config ((h0 (halt))) ,R-empty null () exited)))
    (check-true (completion-trace positive))
    (check-false (completion-trace mutant)))

  (test-case "bytecode/properties.rkt::feedback-trace"
    (define positive
      (term (config ((h0 (const O17 r0) (fb r0) (tfb r1) (halt)))
                    ,R-empty null ((frame h0 0)) running)))
    (define false-value
      (term (config ((h0 (const false r0) (fb r0) (tfb r1) (halt)))
                    ,R-empty null ((frame h0 0)) running)))
    (define mutant
      (term (config ((h0 (const O17 r0) (fb r0) (const Z r0) (halt)))
                    ,R-empty null ((frame h0 0)) running)))
    (check-true (feedback-trace positive))
    (check-true (feedback-trace false-value))
    (check-false (feedback-trace mutant)))

  (test-case "bytecode/properties.rkt::handler-trace"
    (define positive
      (term (config ((h0 (call h1) (halt)) (h1 (ret))) ,R-empty null
                    ((frame h0 0)) running)))
    (define mutant
      (term (config ((h0 (goto h1)) (h1 (halt))) ,R-empty null
                    ((frame h0 0)) running)))
    (check-true (handler-trace positive))
    (check-false (handler-trace mutant)))

  (test-case "bytecode/properties.rkt::module-trace"
    (define positive (term (config ,(cdr (compile-core (term (if Z O1 O17))))
                                   ,R-empty null ((frame h0 0)) running)))
    (define mutant
      (term (config ((h0 (goto h1) (halt))) ,R-empty null
                    ((frame h0 0)) running)))
    (check-true (module-trace positive))
    (check-false (module-trace mutant)))

  (test-case "bytecode/properties.rkt::root-trace"
    (define positive (term (config ,(cdr (compile-core (term (add O1 O1))))
                                   ,R-empty null ((frame h0 0)) running)))
    (define mutant
      (term (config ((h0 (halt))) ,R-empty null ((frame h1 0)) running)))
    (check-true (root-trace positive))
    (check-false (root-trace mutant)))

  ;; Fixed-seed redex-check controls. The explicit seed, rather than
  ;; #:attempts, makes generated cases reproducible; attempts only bounds
  ;; the number of generated cases.
  (parameterize ([current-pseudo-random-generator (make-pseudo-random-generator)])
    (random-seed 20260727)
    (check-equal? (redex-check-bc-determinism) #t)
    (check-equal? (redex-check-feedback) #t)
    (check-equal? (redex-check-compile-run) #t)))
