#lang racket

;;; formal/redex/ecmascript/core.rkt
;;; PLT Redex 9.2 model of the BamTiScript ECMAScript core: the surface
;;; expression calculus and the -->JS compilation relation that lowers it to
;;; the register/handler/feedback/entry-point bytecode.
;;;
;;; This module is the root of the ecmascript model. It defines the core
;;; language ES (kept consistent with the bytecode model in
;;; formal/redex/bytecode/language.rkt: same finite value algebra, the same
;;; source calculus (e ::= v (op e ...) (let e e) (if e e e) (seq e e)
;;; (handler h e)), and the same instruction set) and the compilation
;;; relation -->JS.
;;;
;;; Non-cyclic import graph (the ecmascript set):
;;;   core.rkt  <--require--  semantics.rkt
;;;   core.rkt  <--require--  modules.rkt
;;; semantics.rkt and modules.rkt never require each other and never require
;;; back into core beyond `(require "core.rkt")`. ES is defined once, here;
;;; the other two modules extend it with define-extended-language, so no
;;; language definition is duplicated within the set.
;;;
;;; Public surface (all-defined-out): the language ES, the relation -->JS,
;;; the comp / compile-program / handler-blocks metafunctions, and the
;;; helpers lookup-reg / extend-env / fresh-reg / fresh-regs / truthy? /
;;; bc-arith.

(require redex/reduction-semantics)

(provide (all-defined-out))

;;; ===========================================================================
;;; Core language: ES
;;; ===========================================================================
;;;
;;; Values are finite (the NaN-boxed wire algebra): Z=0, O1=1, O17=17, the two
;;; booleans, and null. The op set matches the bytecode model. Registers are
;;; bounded r0..r3. Handler names are strings. The source calculus `e` is the
;;; one the bytecode compiler lowers; -->JS compiles exactly this calculus into
;;; the instruction stream below.

(define-language ES
  ;; --- Values (finite NaN-boxed algebra) ---
  (v ::= n bool null)
  (n ::= Z O1 O17)
  (bool ::= true false)
  (op ::= add sub not is-null)

  ;; --- Registers & handler names ---
  (x ::= r0 r1 r2 r3)
  (h ::= entry if-t if-e f)

  ;; --- Core source calculus (compiled by -->JS) ---
  (e ::= v y
         (op e ...)
         (let e e)
         (if e e e)
         (seq e e)
         (fun h e))

  ;; --- Bytecode instructions (the target of -->JS) ---
  ;; register-aware: every instruction names its destination/explicit registers.
  ;; handler-aware: goto/call name a handler h. feedback-aware: fb/tfb move a
  ;; value to/from the feedback cell. entry-point-aware: a program is an ordered
  ;; handler list whose first handler is the entry point.
  (instr ::= (const v x)
             (mov x x)
             (bin op x x x)
             (un op x x)
             (tfb x)
             (fb x)
             (goto h)
             (call h)
             (ret)
             (halt)
             (jif x h)
             (jnz x h))

  ;; A handler is a name plus a non-empty instruction list. A program is a list
  ;; of handlers; the first is the entry point.
  (block ::= (h instr ...+))
  (prog ::= (program block ...+))

  ;; --- Environment: source variables -> registers (compile-time) ---
  (env ::= ((y x) ...))
  (y ::= a b c d)

  ;; --- Compilation state: (env next-free-reg e) ---
  ;; next-free-reg is a natural index into the bounded register file.
  (idx ::= natural)
  (out ::= (idx instr ...)))

;;; ===========================================================================
;;; Helpers
;;; ===========================================================================

;; Fresh register by index, wrapping mod 4 (the register file is bounded).
(define-metafunction ES
  fresh-reg : idx -> x
  [(fresh-reg 0) r0]
  [(fresh-reg 1) r1]
  [(fresh-reg 2) r2]
  [(fresh-reg 3) r3]
  [(fresh-reg idx) r0])

;; Look a source variable up in the compile-time env; default to r0 so the
;; model is total (the front-end guarantees binding before use).
(define-metafunction ES
  lookup-reg : env y -> x
  [(lookup-reg () y) r0]
  [(lookup-reg ((y_0 x_0) (y_1 x_1) ...) y_0) x_0]
  [(lookup-reg ((y_0 x_0) (y_1 x_1) ...) y_2)
   (lookup-reg ((y_1 x_1) ...) y_2)])

;; Extend env with a binding.
(define-metafunction ES
  extend-env : env y x -> env
  [(extend-env ((y_0 x_0) ...) y_1 x_1)
   ((y_1 x_1) (y_0 x_0) ...)])

;; Truthiness (mirrors the bytecode model).
(define-metafunction ES
  truthy? : v -> bool
  [(truthy? Z) false]
  [(truthy? false) false]
  [(truthy? null) false]
  [(truthy? v) true])

;; Finite arithmetic (mirrors the bytecode model).
(define-metafunction ES
  bc-arith : op v v -> v
  [(bc-arith add O1 O1) O17]
  [(bc-arith add v_0 v_1) O1]
  [(bc-arith sub O17 O1) O1]
  [(bc-arith sub v_0 v_1) Z]
  [(bc-arith not false) true]
  [(bc-arith not v) false]
  [(bc-arith is-null null) true]
  [(bc-arith is-null v) false])

;;; ===========================================================================
;;; Compilation metafunction: comp
;;; ===========================================================================
;;;
;;; comp is the recursive compiler, implemented as a metafunction so that
;;; sub-expressions are compiled by metafunction calls (legal in Redex) rather
;;; than by reduction-relation calls in `where` clauses (which are not legal).
;;; It returns (idx instr ...): the next free index and the instruction
;;; sequence that evaluates e into a result register. The result register is
;;; fresh-reg of the *incoming* index (the first register allocated).
;;;
;;; comp is total: it covers every form in `e`. The -->JS relation below mirrors
;;; comp one-rule-per-construct so the model also exposes a named reduction
;;; relation (required by the acceptance), delegating sub-compilation to comp.

(define-metafunction ES
  comp : env idx e -> out
  ;; --- values ---
  [(comp env idx v)
   (idx_1 (const v x))
   (where x (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))]
  ;; --- variable ---
  [(comp env idx y)
   (idx_1 (mov x_src x_dst))
   (where x_src (lookup-reg env y))
   (where x_dst (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))]
  ;; --- binary op (add sub is-null) ---
  [(comp env idx (add e_0 e_1))
   (idx_4 instr_0 ... instr_1 ... (bin add x_dst x_0 x_1))
   (where x_0 (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))
   (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
   (where x_1 (fresh-reg idx_2))
   (where (idx_3 instr_1 ...) (comp env idx_2 e_1))
   (where x_dst (fresh-reg idx_3))
   (where idx_4 ,(add1 (term idx_3)))]
  [(comp env idx (sub e_0 e_1))
   (idx_4 instr_0 ... instr_1 ... (bin sub x_dst x_0 x_1))
   (where x_0 (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))
   (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
   (where x_1 (fresh-reg idx_2))
   (where (idx_3 instr_1 ...) (comp env idx_2 e_1))
   (where x_dst (fresh-reg idx_3))
   (where idx_4 ,(add1 (term idx_3)))]
  [(comp env idx (is-null e_0 e_1))
   (idx_4 instr_0 ... instr_1 ... (bin is-null x_dst x_0 x_1))
   (where x_0 (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))
   (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
   (where x_1 (fresh-reg idx_2))
   (where (idx_3 instr_1 ...) (comp env idx_2 e_1))
   (where x_dst (fresh-reg idx_3))
   (where idx_4 ,(add1 (term idx_3)))]
  ;; --- unary op ---
  [(comp env idx (not e_0))
   (idx_3 instr_0 ... (un not x_dst x_0))
   (where x_0 (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))
   (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
   (where x_dst (fresh-reg idx_2))
   (where idx_3 ,(add1 (term idx_2)))]
  ;; --- let ---
  [(comp env idx (let e_0 e_1))
   (idx_final instr_0 ... instr_1 ...)
   (where (idx_2 instr_0 ...) (comp env idx e_0))
   (where (idx_final instr_1 ...) (comp env idx_2 e_1))]
  ;; --- if : compile condition inline; dispatch to dedicated then/else blocks.
  ;; The then/else bodies live in handler blocks "if-t"/"if-e" produced by
  ;; handler-blocks; the entry block only tests and dispatches.
  [(comp env idx (if e_c e_t e_f))
   (idx_3 instr_c ... (jif x_c if-t) (call if-e) (ret))
   (where x_c (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))
   (where (idx_2 instr_c ...) (comp env idx_1 e_c))
   (where idx_3 ,(add1 (term idx_2)))]
  ;; --- seq ---
  [(comp env idx (seq e_0 e_1))
   (idx_final instr_0 ... instr_1 ...)
   (where (idx_2 instr_0 ...) (comp env idx e_0))
   (where (idx_final instr_1 ...) (comp env idx_2 e_1))]
  ;; --- handler : compile body inline then goto h; the body block is also
  [(comp env idx (fun h_0 e_0))
   (idx_2 instr_body ... (goto h_0))
   (where (idx_2 instr_body ...) (comp env idx e_0))])

;; Allocate a fresh register per argument, returning the list and next index.
(define-metafunction ES
  fresh-regs : idx (e ...) -> (idx (x ...))
  [(fresh-regs idx ()) (idx ())]
  [(fresh-regs idx (e_0 e_1 ...))
   (idx_final (x_0 x_1 ...))
   (where x_0 (fresh-reg idx))
   (where idx_1 ,(add1 (term idx)))
   (where (idx_final (x_1 ...)) (fresh-regs idx_1 (e_1 ...)))])

;;; ===========================================================================
;;; handler-blocks : collect (handler h instr ...) blocks for each lambda body
;;; ===========================================================================
;;;
;;; Walks the source tree and, for every (handler h e), compiles e and produces
;;; a (handler h instr ... (ret)) block. This is what compile-program stitches
;;; into the program so lambda bodies are dispatchable at runtime.

(define-metafunction ES
  handler-blocks : e -> (block ...)
  [(handler-blocks v) ()]
  [(handler-blocks y) ()]
  [(handler-blocks (op e ...))
   (block_extra ...)
   (where (block_extra ...) (handler-blocks* (e ...)))]
  [(handler-blocks (let e_0 e_1))
   (block_0 ... block_1 ...)
   (where (block_0 ...) (handler-blocks e_0))
   (where (block_1 ...) (handler-blocks e_1))]
  [(handler-blocks (if e_c e_t e_f))
   ((if-t instr_t ... (ret)) (if-e instr_f ... (ret)) block_c ...)
   (where (idx_t instr_t ...) (comp () 0 e_t))
   (where (idx_f instr_f ...) (comp () 0 e_f))
   (where (block_c ...) (handler-blocks e_c))]
  [(handler-blocks (seq e_0 e_1))
   (block_0 ... block_1 ...)
   (where (block_0 ...) (handler-blocks e_0))
   (where (block_1 ...) (handler-blocks e_1))]
  [(handler-blocks (fun h_0 e_0))
   ((h_0 instr_body ... (ret)))
   (where (idx_body instr_body ...) (comp () 0 e_0))])
(define-metafunction ES
  handler-blocks* : (e ...) -> (block ...)
  [(handler-blocks* ()) ()]
  [(handler-blocks* (e_0 e_1 ...))
   (block_0 ... block_1 ...)
   (where (block_0 ...) (handler-blocks e_0))
   (where (block_1 ...) (handler-blocks* (e_1 ...)))])

;;; ===========================================================================
;;; Compilation relation: -->JS  (named rules, delegates to comp)
;;; ===========================================================================
;;;
;;; -->JS is the named reduction relation required by the acceptance. Each rule
;;; corresponds to one source construct and delegates sub-compilation to the
;;; `comp` metafunction (legal in `where`, since comp is a metafunction). The
;;; relation is deterministic and total over well-formed `e`.

(define -->JS
  (reduction-relation ES
    ;; Accept an arbitrary source payload so malformed expressions can be
    ;; observed as having no one-step compilation successor.
    #:domain (env idx any)
    #:codomain out

    (--> (env idx v)
         (idx_1 (const v x))
         (where x (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
        "load-value")

    (--> (env idx y)
         (idx_1 (mov x_src x_dst))
         (where x_src (lookup-reg env y))
         (where x_dst (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
        "load-var")

    (--> (env idx (add e_0 e_1))
         (idx_4 instr_0 ... instr_1 ... (bin add x_dst x_0 x_1))
         (where x_0 (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
         (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
         (where x_1 (fresh-reg idx_2))
         (where (idx_3 instr_1 ...) (comp env idx_2 e_1))
         (where x_dst (fresh-reg idx_3))
         (where idx_4 ,(add1 (term idx_3)))
        "bin-add")

    (--> (env idx (sub e_0 e_1))
         (idx_4 instr_0 ... instr_1 ... (bin sub x_dst x_0 x_1))
         (where x_0 (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
         (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
         (where x_1 (fresh-reg idx_2))
         (where (idx_3 instr_1 ...) (comp env idx_2 e_1))
         (where x_dst (fresh-reg idx_3))
         (where idx_4 ,(add1 (term idx_3)))
        "bin-sub")

    (--> (env idx (is-null e_0 e_1))
         (idx_4 instr_0 ... instr_1 ... (bin is-null x_dst x_0 x_1))
         (where x_0 (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
         (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
         (where x_1 (fresh-reg idx_2))
         (where (idx_3 instr_1 ...) (comp env idx_2 e_1))
         (where x_dst (fresh-reg idx_3))
         (where idx_4 ,(add1 (term idx_3)))
        "bin-is-null")

    (--> (env idx (not e_0))
         (idx_3 instr_0 ... (un not x_dst x_0))
         (where x_0 (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
         (where (idx_2 instr_0 ...) (comp env idx_1 e_0))
         (where x_dst (fresh-reg idx_2))
         (where idx_3 ,(add1 (term idx_2)))
        "un-not")

    (--> (env idx (let e_0 e_1))
         (idx_final instr_0 ... instr_1 ...)
         (where (idx_2 instr_0 ...) (comp env idx e_0))
         (where (idx_final instr_1 ...) (comp env idx_2 e_1))
        "let-bind")

    (--> (env idx (if e_c e_t e_f))
         (idx_3 instr_c ... (jif x_c if-t) (call if-e) (ret))
         (where x_c (fresh-reg idx))
         (where idx_1 ,(add1 (term idx)))
         (where (idx_2 instr_c ...) (comp env idx_1 e_c))
         (where idx_3 ,(add1 (term idx_2)))
        "if-branch")

    (--> (env idx (seq e_0 e_1))
         (idx_final instr_0 ... instr_1 ...)
         (where (idx_2 instr_0 ...) (comp env idx e_0))
         (where (idx_final instr_1 ...) (comp env idx_2 e_1))
        "seq-eval")

    (--> (env idx (fun h_0 e_0))
         (idx_2 instr_body ... (goto h_0))
         (where (idx_2 instr_body ...) (comp env idx e_0))
        "make-handler")))

;;; ===========================================================================
;;; Program assembly: compile-program
;;; ===========================================================================
;;;
;;; compile-program compiles a top-level expression into a (program handler ...)
;;; whose first handler is the entry point "entry". Lambda bodies (handler h e)
;;; become additional handler blocks in the program, so they are dispatchable
;;; at runtime.

(define-metafunction ES
  compile-program : e -> prog
  [(compile-program e)
   (program (entry instr ... (ret)) block_extra ...)
   (where (idx instr ...) (comp () 0 e))
   (where (block_extra ...) (handler-blocks e))])

;;; ===========================================================================
;;; Tests
;;; ===========================================================================
(module+ test
  (require rackunit)

  ;; --- Deterministic, non-vacuous controls (concrete observable terms) ---

  ;; A value compiles to a single const into r0, next index 1.
  (check-equal?
   (apply-reduction-relation* -->JS (term (() 0 O1)))
   (term ((1 (const O1 r0)))))
  (check-equal?
   (apply-reduction-relation* -->JS (term (() 0 null)))
   (term ((1 (const null r0)))))

  ;; A variable in the empty env defaults to r0 (total model): mov r0 r0.
  (check-equal?
   (apply-reduction-relation* -->JS (term (() 0 a)))
   (term ((1 (mov r0 r0)))))

  ;; A bound variable moves from its register: env ((a r2)) -> mov r2 r0.
  (check-equal?
   (apply-reduction-relation* -->JS (term (((a r2)) 0 a)))
   (term ((1 (mov r2 r0)))))

  ;; The comp metafunction agrees with -->JS for a value.
  (check-equal? (term (comp () 0 O1)) (term (1 (const O1 r0))))
  (check-equal? (term (comp () 0 null)) (term (1 (const null r0))))

  ;; A lambda (handler "f" e) compiles its body inline and emits a goto; the
  ;; body block is produced by handler-blocks.
  (check-equal?
   (apply-reduction-relation* -->JS (term (() 0 (fun f O1))))
   (term ((1 (const O1 r0) (goto f)))))
  (check-equal?
   (term (handler-blocks (fun f O1)))
   (term ((f (const O1 r0) (ret)))))

  ;; compile-program assembles the entry block plus the lambda's body block.
  (check-equal?
   (term (compile-program (fun f O1)))
   (term (program (entry (const O1 r0) (goto f) (ret))
                  (f (const O1 r0) (ret)))))

  ;; --- Malformed negative: a register r0 is not a core expression, so the
  ;; one-step relation produces no successor (r0 is in `x`, not `e`).
  (check-equal?
   (apply-reduction-relation -->JS (term (() 0 r0)))
   (term ()))

  ;; --- Fixed-seed redex-check: compiling any value preserves that value,
  ;; increments the index once, and emits exactly one register-targeted const.
  (redex-check
   ES
   (env_0 idx_0 v_0)
   (let ([results
          (apply-reduction-relation* -->JS (term (env_0 idx_0 v_0)))])
     (match results
       [(list (list idx_1 (list 'const value x)))
        (and (equal? idx_1 (add1 (term idx_0)))
             (equal? value (term v_0))
             (equal? x (term (fresh-reg idx_0))))]
       [_ #f]))
   #:attempts 200
   #:source -->JS)

  ;; Each remaining named rule has a concrete, observable transition.
  (for ([source (in-list (list (term (() 0 (add O1 O1)))
                               (term (() 0 (sub O17 O1)))
                               (term (() 0 (is-null null Z)))
                               (term (() 0 (not false)))
                               (term (() 0 (let O1 O17)))
                               (term (() 0 (if O1 O17 Z)))
                               (term (() 0 (seq O1 O17)))))])
    (check-equal? (length (apply-reduction-relation* -->JS source)) 1))

  ;; --- Observable mutant guard: load-value must bump the index. A mutant
  ;; returning the input index breaks this equality.
  (check-not-equal?
   (car (apply-reduction-relation* -->JS (term (() 0 O1))))
   (term (0 (const O1 r0)))))
