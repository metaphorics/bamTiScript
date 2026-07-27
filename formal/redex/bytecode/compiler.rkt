#lang racket

;;; bytecode/compiler.rkt -- compile-core: source calculus -> BC program.
;;;
;;; The source calculus `e` (defined in language.rkt as part of bc-lang)
;;; is a small expression language:
;;;
;;;   e ::= v | (op e e) | (let e e) | (if e e e) | (seq e e) | (handler h e)
;;;
;;; compile-core compiles a closed expression `e` into a BC `prog`
;;; `(program handler ...)`. The compilation is register-directed: the
;;; result of an expression is left in r0. `let` binds into r1 (matching
;;; the bounded 4-register file). `if` compiles to jif/goto plus two
;;; hoisted handlers (h2 then, h3 else). `handler` introduces a named
;;; handler that compiles its body and ends in `halt`.
;;;
;;; src~bc is the compilation-correctness relation: it holds when
;;; compile-core produces exactly a given program. Used by properties.rkt
;;; for the weak-bisimulation check.

(require redex/reduction-semantics)
(require "language.rkt")

(provide compile-core
         compile-expr*
         src~bc
         compile-correct?
         compile-prog-well-formed?)

;;; Register allocation: r0 = result, r1 = let-bound, r2/r3 = op operands.
(define result-reg 'r0)
(define let-reg 'r1)
(define op-arg-a 'r2)
(define op-arg-b 'r3)

;;; The set of binary op symbols recognised by the source calculus.
(define bin-ops '(add sub is-null))

;;; literal? : is e a value literal (a leaf of the source AST)?
(define (literal? e)
  (memq e '(Z O1 O17 true false null)))

;;; compile-expr* : e -> (values (instr ...) (handler ...))
;;; Returns the instruction sequence that evaluates e (result in r0)
;;; plus a list of extra handler definitions hoisted to the top-level
;;; program. Pure Racket recursion over the source AST.
(define (compile-expr* e)
  (cond
    ;; Literal value: const into r0.
    [(literal? e)
     (values (list (list 'const e result-reg)) '())]
    ;; Binary op.
    [(and (list? e) (memq (car e) bin-ops))
     (define op (car e))
     (define e1 (cadr e))
     (define e2 (caddr e))
     (define-values (i1 h1) (compile-expr* e1))
     (define-values (i2 h2) (compile-expr* e2))
     (values
      (append i1
              (list (list 'mov op-arg-a result-reg))
              i2
              (list (list 'mov op-arg-b result-reg))
              (list (list 'bin op result-reg op-arg-a op-arg-b)))
      (append h1 h2))]
    ;; Unary not.
    [(and (list? e) (eq? (car e) 'not))
     (define-values (i1 h1) (compile-expr* (cadr e)))
     (values (append i1 (list (list 'un 'not result-reg result-reg))) h1)]
    ;; let: bind the value into r1, evaluate the body.
    [(and (list? e) (eq? (car e) 'let))
     (define-values (iv hv) (compile-expr* (cadr e)))
     (define-values (ib hb) (compile-expr* (caddr e)))
     (values (append iv (list (list 'mov let-reg result-reg)) ib)
             (append hv hb))]
    ;; if: compile test, jif to then-handler, goto else-handler; hoist both.
    [(and (list? e) (eq? (car e) 'if))
     (define e-test (cadr e))
     (define e-then (caddr e))
     (define e-else (cadddr e))
     (define then-handler 'h2)
     (define else-handler 'h3)
     (define-values (it ht) (compile-expr* e-test))
     (define-values (ith hth) (compile-expr* e-then))
     (define-values (ieh heh) (compile-expr* e-else))
     (values
      (append it
              (list (list 'jif result-reg then-handler))
              (list (list 'goto else-handler)))
      (append ht hth heh
              (list (append (list then-handler) ith (list (list 'halt))))
              (list (append (list else-handler) ieh (list (list 'halt))))))]
    ;; seq: evaluate both, keep the last result.
    [(and (list? e) (eq? (car e) 'seq))
     (define-values (i1 h1) (compile-expr* (cadr e)))
     (define-values (i2 h2) (compile-expr* (caddr e)))
     (values (append i1 i2) (append h1 h2))]
    ;; handler: a named handler. Body compiles, ends in halt. No
    ;; instructions are emitted inline; the whole handler is hoisted.
    [(and (list? e) (eq? (car e) 'handler))
     (define h (cadr e))
     (define body (caddr e))
     (define-values (ib hb) (compile-expr* body))
     (values '() (append hb (list (append (list h) ib (list (list 'halt))))))]
    [else
     (error 'compile-expr* "malformed source expression: ~a" e)]))

;;; compile-core : e -> prog
;;; Top-level driver: compile e, hoist any extra handlers, and wrap in
;;; (program main-handler extra-handlers ...). The main handler is h0.
(define (compile-core e)
  (define-values (main-instrs extra-handlers) (compile-expr* e))
  (define main-handler (append (list 'h0) main-instrs (list (list 'halt))))
  (apply list 'program main-handler extra-handlers))

;;; -------------------------------------------------------------------------
;;; src~bc : the compilation-correctness relation
;;; -------------------------------------------------------------------------

;;; A metafunction bridge: compile-of returns the program compile-core
;;; produces for e. This lets src~bc be a pure Redex judgment.
(define-metafunction bc-lang
  compile-of : e -> prog
  [(compile-of e) ,(compile-core (term e))])

;;; src~bc holds when compiling e yields exactly prog.
(define-judgment-form bc-lang
  #:mode (src~bc I I)
  #:contract (src~bc e prog)
  [(src~bc e prog)
   (where prog (compile-of e))])

;;; compile-correct? : a Racket-level predicate for direct testing.
(define (compile-correct? e prog)
  (equal? (compile-core e) prog))

;;; compile-prog-well-formed? : the compiled program's handler table is
;;; well-formed (unique names, bounded registers).
(define (compile-prog-well-formed? prog)
  (match prog
    [(list 'program handlers ...)
     (well-formed-handler handlers)]
    [_ #f]))

(module+ test
  (require rackunit)

  ;; Deterministic example: (add O1 O1) compiles to a fixed sequence.
  (define e-add (term (add O1 O1)))
  (define prog-add (compile-core e-add))
  (check-equal?
   prog-add
   (term (program
          (h0 (const O1 r0) (mov r2 r0)
              (const O1 r0) (mov r3 r0)
              (bin add r0 r2 r3) (halt)))))
  (check-true (compile-prog-well-formed? prog-add))

  ;; let binds the value into r1.
  (define e-let (term (let O1 O17)))
  (check-equal?
   (compile-core e-let)
   (term (program (h0 (const O1 r0) (mov r1 r0) (const O17 r0) (halt)))))

  ;; if compiles to jif/goto plus two hoisted handlers (h2 then, h3 else).
  (define e-if (term (if O1 O17 Z)))
  (define prog-if (compile-core e-if))
  (check-true (compile-prog-well-formed? prog-if))
  ;; The main handler must contain jif r0 h2 and goto h3.
  (define main-instrs (cdr (cadr prog-if)))
  (check-not-false (member (term (jif r0 h2)) main-instrs) "if emits jif r0 h2")
  (check-not-false (member (term (goto h3)) main-instrs) "if emits goto h3")
  ;; h2 and h3 are hoisted as handlers.
  (define handler-names (map car (cddr prog-if)))
  (check-not-false (member 'h2 handler-names) "then handler h2 is hoisted")
  (check-not-false (member 'h3 handler-names) "else handler h3 is hoisted")

  ;; src~bc holds for the compiled program.
  (check-true (judgment-holds (src~bc ,e-add ,prog-add)))
  (check-false (judgment-holds (src~bc ,e-add (program (h0 (halt)))))
               "src~bc rejects a program that is not the compilation of e")

  ;; Negative control: a malformed source term is rejected.
  (check-exn exn:fail?
             (lambda () (compile-expr* (term (add O1))))
             "malformed source is rejected by the compiler")

  ;; compile-correct? mirrors src~bc.
  (check-true (compile-correct? e-add prog-add))
  (check-false (compile-correct? e-add (term (program (h0 (halt)))))))
