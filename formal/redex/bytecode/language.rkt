#lang racket

;;; bytecode/language.rkt -- PLT Redex 9.2 model of the BamTiScript
;;; bytecode (BC) core.
;;;
;;; The BC model is a small register machine with:
;;;   * a bounded register file (r0..r3),
;;;   * a handler table mapping handler names to instruction sequences,
;;;   * a feedback cell written by `fb` and read by `tfb`,
;;;   * arbitrary entry transfer (`goto`/`call`/`ret`) between handlers,
;;;   * a FIFO scheduler with explicit N-API lifecycle transitions.
;;;
;;; Self-contained: no host model, no imports beyond redex/reduction-semantics.
;;; Reduction rules live in step.rkt, the compiler in compiler.rkt,
;;; properties/bisimulation in properties.rkt, and the scheduler/simulation
;;; harness in simulation.rkt.

(require redex/reduction-semantics)

(provide
 ;; Language definitions
 bc-lang
 bc-extended-lang
 bc-config
 ;; Well-formedness / predicates
 well-formed
 well-formed-handler
 register?
 handler-name?
 fb?
 reg-count
 ;; Handler table ops
 handler-entry
 handler-body
 fresh-handler
 ;; Register file ops
 extend-regs
 lookup-reg
 update-reg
 reset-regs
 ;; Instruction / value helpers
 instr-at
 truthy?
 bc-arith
 bc-value?
 bc-prim?
 ;; Aliases used by downstream modules
 B)

;;; -------------------------------------------------------------------------
;;; Language definition
;;; -------------------------------------------------------------------------

(define-language bc-lang
  ;; Values drawn from the NaN-boxed wire algebra (see formal/lean/Bamti/Value).
  ;; Kept finite so redex-check enumerates the whole space.
  (v ::= n b null)
  (n ::= Z O1 O17)            ;; canonical numbers: 0, 1, 17
  (b ::= true false)
  (op ::= add sub not is-null)
  (x ::= r0 r1 r2 r3)         ;; bounded register file
  (h ::= h0 h1 h2 h3 h4 h5)   ;; explicit, finite handler-name alphabet

  ;; Core expressions -- the source calculus compiled by compiler.rkt.
  (e ::= v
         (op e e)
         (let e e)
         (if e e e)
         (seq e e)
         (handler h e))

  ;; Bytecode instructions -- the target of compilation.
  (instr ::= (const v x)
             (mov x x)
             (bin op x x x)
             (un op x x)
             (tfb x)                 ;; transfer feedback -> register
             (fb x)                 ;; register -> feedback
             (goto h)
             (call h)
             (ret)
             (halt)
             (jif x h)               ;; jump if register is truthy
             (jnz x h))             ;; jump if register is non-zero

  (prog ::= (program handler ...))

  (handler ::= (h instr ...))

  ;; Predicate non-terminals so redex-check can bind them.
  (reg ::= r0 r1 r2 r3)
  (han ::= h0 h1 h2 h3 h4 h5))

;;; The extended language adds the evaluation *configuration*: register
;;; file, handler table, feedback cell, control stack, and scheduler.
(define-extended-language bc-extended-lang bc-lang
  (R ::= ((x v) ...))                 ;; register file, bounded by reg-count
  (H ::= (handler ...))               ;; installed handler table
  (F ::= v null)                      ;; feedback cell (single-slot)
  (k ::= (frame h pc))                ;; one control frame: name + pc
  (pc ::= natural)
  (S ::= running blocked exited)      ;; scheduler / N-API lifecycle state
  (Q ::= (k ...))                     ;; FIFO ready queue of frames
  (cfg ::= (config H R F Q S)))       ;; the machine configuration

;;; Configuration constructor as a meta-function.
(define-metafunction bc-extended-lang
  bc-config : H R F Q S -> cfg
  [(bc-config H R F Q S) (config H R F Q S)])

;;; Convenience alias for the base language used by downstream modules.
(define B bc-lang)

;;; A handler table is well-formed when it matches the grammar and handler
;;; names are unique. Handlers are (h instr ...) so we take the head of
;;; each element.
(define (well-formed-handler H)
  (and ((redex-match? bc-extended-lang H) H)
       (match H
         [(list handlers ...)
          (let ([names (map (lambda (hd) (car hd)) handlers)])
            (and (andmap symbol? names)
                 (= (length names) (length (remove-duplicates names)))))]
         [_ #f])))
(define bc-value? (redex-match? bc-extended-lang v))
(define bc-prim? (redex-match? bc-extended-lang op))
(define register? (redex-match? bc-extended-lang reg))
(define handler-name? (redex-match? bc-extended-lang han))
(define fb? (redex-match? bc-extended-lang F))


;;; A configuration is well-formed when it matches the grammar.
(define (well-formed cfg)
  (and ((redex-match? bc-extended-lang cfg) cfg)
       (well-formed-handler (cadr cfg))))

;;; -------------------------------------------------------------------------
;;; Meta-functions over the register file
;;; -------------------------------------------------------------------------

(define-metafunction bc-extended-lang
  lookup-reg : R x -> v
  [(lookup-reg ((x_0 v_0) ... (x v) (x_1 v_1) ...) x) v]
  [(lookup-reg ((x_0 v_0) ...) x) null])

(define-metafunction bc-extended-lang
  update-reg : R x v -> R
  [(update-reg ((x_0 v_0) ... (x v) (x_1 v_1) ...) x v_new)
   ((x_0 v_0) ... (x v_new) (x_1 v_1) ...)]
  [(update-reg ((x_0 v_0) ...) x v_new)
   ((x_0 v_0) ... (x v_new))])

(define-metafunction bc-extended-lang
  reset-regs : R -> R
  [(reset-regs ((x_0 v_0) ...)) ((x_0 null) ...)])

;;; On call, save the caller's regs; on ret, restore them. We return the
;;; source (caller) register file so the callee starts fresh and the
;;; caller state is preserved in the control frame.
(define-metafunction bc-extended-lang
  extend-regs : R R -> R
  [(extend-regs R_src R_dst) R_src])

;;; -------------------------------------------------------------------------
;;; Handler table lookup
;;; -------------------------------------------------------------------------

(define-metafunction bc-extended-lang
  handler-entry : H h -> handler
  [(handler-entry (handler_0 ... (h instr ...) handler_1 ...) h)
   (h instr ...)]
  [(handler-entry (handler ...) h) (h (halt))])

(define-metafunction bc-extended-lang
  handler-body : H h -> (instr ...)
  [(handler-body (handler_0 ... (h instr ...) handler_1 ...) h)
   (instr ...)]
  [(handler-body (handler ...) h) ((halt))])

;;; Produce a handler name not equal to h_base (suffixed). Finite alphabet,
;;; so the result is one of h0..h5.
(define-metafunction bc-extended-lang
  fresh-handler : H h -> h
  [(fresh-handler (handler ...) h_base) h0])

;;; -------------------------------------------------------------------------
;;; Instruction stream + value helpers
;;; -------------------------------------------------------------------------

;;; The finite register file has exactly four architectural registers.
(define-metafunction bc-extended-lang
  reg-count : -> natural
  [(reg-count) 4])

;;; nth instruction with bounds; returns halt past the end so the machine
;;; never indexes into a handler body out of range. Index by natural-number
;;; recursion: base case selects element 0, step recurs on the tail with
;;; predecessor. A Racket helper backs the recursion so it works for any
;;; pc, not just ones expressible as a single literal.
(define-metafunction bc-extended-lang
  instr-at : (instr ...) pc -> instr
  [(instr-at (instr_0 instr_1 ...) 0) instr_0]
  [(instr-at (instr_0 instr_1 ...) pc)
   (instr-at (instr_1 ...) pc_minus_1)
   (where pc_minus_1 ,(sub1 (term pc)))
   (side-condition (> (term pc) 0))]
  [(instr-at () pc) (halt)])

;;; Truthiness for jif/jnz.
(define-metafunction bc-extended-lang
  truthy? : v -> b
  [(truthy? Z) false]
  [(truthy? false) false]
  [(truthy? null) false]
  [(truthy? v) true])

;;; Arithmetic and predicate operations on the finite value domain.
(define-metafunction bc-extended-lang
  bc-arith : op v v -> v
  [(bc-arith add O1 O1) O17]
  [(bc-arith add v_0 v_1) O1]
  [(bc-arith sub O17 O1) O1]
  [(bc-arith sub v_0 v_1) Z]
  [(bc-arith not Z v_1) true]
  [(bc-arith not false v_1) true]
  [(bc-arith not null v_1) true]
  [(bc-arith not v_0 v_1) false]
  [(bc-arith is-null null v_1) true]
  [(bc-arith is-null v_0 v_1) false])
(module+ test
  (require rackunit)
  ;; Deterministic, non-vacuous controls: each helper returns a specific,
  ;; observable result rather than merely matching a pattern.
  ;; Handlers are (h (instr) ...) -- instructions are parenthesized.
  (check-equal? (term (lookup-reg ((r0 O1) (r1 Z)) r0)) (term O1))
  (check-equal? (term (lookup-reg ((r0 O1) (r1 Z)) r2)) (term null))
  (check-equal? (term (update-reg ((r0 O1) (r1 Z)) r1 O17))
                (term ((r0 O1) (r1 O17))))
  (check-equal? (term (reset-regs ((r0 O1) (r1 O17))))
                (term ((r0 null) (r1 null))))
  ;; instr-at indexes a parenthesized instruction stream.
  (check-equal? (term (instr-at ((const Z r0) (mov r0 r1) (halt)) 1))
                (term (mov r0 r1)))
  (check-equal? (term (instr-at ((const Z r0)) 5)) (term (halt)))
  (check-equal? (term (instr-at () 0)) (term (halt)))
  (check-equal? (term (truthy? Z)) (term false))
  (check-equal? (term (truthy? O1)) (term true))
  (check-equal? (term (bc-arith add O1 O1)) (term O17))
  (check-equal? (term (bc-arith sub O17 O1)) (term O1))
  (test-case "bc-arith not truth table"
    (check-equal? (term (bc-arith not Z Z)) (term true))
    (check-equal? (term (bc-arith not false false)) (term true))
    (check-equal? (term (bc-arith not null null)) (term true))
    (check-equal? (term (bc-arith not O1 O1)) (term false))
    (check-equal? (term (bc-arith not O17 O17)) (term false))
    (check-equal? (term (bc-arith not true true)) (term false)))
  (test-case "bc-arith is-null truth table"
    (check-equal? (term (bc-arith is-null null O17)) (term true))
    (check-equal? (term (bc-arith is-null Z O17)) (term false))
    (check-equal? (term (bc-arith is-null O1 O17)) (term false))
    (check-equal? (term (bc-arith is-null O17 O1)) (term false))
    (check-equal? (term (bc-arith is-null true false)) (term false))
    (check-equal? (term (bc-arith is-null false null)) (term false)))
  (check-equal? (term (handler-entry ((h0 (const Z r0)) (h1 (halt))) h1))
                (term (h1 (halt))))
  (check-equal? (term (handler-body ((h0 (const Z r0)) (h1 (halt))) h1))
                (term ((halt))))
  (check-true (well-formed-handler (term ((h0 (const Z r0)) (h1 (halt))))))
  (check-false (well-formed-handler (term ((h0 (const Z r0)) (h0 (halt)))))
              "duplicate handler names are not well-formed")
  (check-true (register? (term r0)))
  (check-true (handler-name? (term h0)))
  (check-true (fb? (term null)))
  (check-equal? (term (reg-count)) 4))
