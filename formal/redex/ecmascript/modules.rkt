#lang racket

;;; formal/redex/ecmascript/modules.rkt
;;; PLT Redex 9.2 model of the BamTiScript module system: binding, linking,
;;; module evaluation, and dynamic import.
;;;
;;; Requires core.rkt (non-cyclic) and extends ES with the module-system
;;; configuration. Defines four named reduction relations:
;;;   -->bind     bind a module's exported names to import slots
;;;   -->link     link two modules: resolve an import against an export table
;;;   -->eval     evaluate a module's entry handler to a completion value
;;;   -->dimport  dynamic import: enqueue an import request and settle it
;;;
;;; Public surface: ES-mod, the four relations, and the helper metafunctions
;;; (module-exports, module-imports, lookup-export, bind-one, bind-all,
;;; link-resolve, enqueue-import, settle-import).

(require redex/reduction-semantics)
(require "core.rkt")

(provide (all-defined-out))

;;; ===========================================================================
;;; Extended language: ES-mod
;;; ===========================================================================
;;;
;;; Extends ES (from core.rkt) with module records, binding tables, link
;;; configurations, and the dynamic-import request queue. ES is not redefined
;;; (no duplication within the set).

(define-extended-language ES-mod ES
  ;; --- Module identifiers & specifiers ---
  (mid ::= m0 m1 m2 m3)          ;; module identifiers (finite)
  (spec ::= string)              ;; import specifiers (URLs/paths)

  ;; --- Binding tables ---
  ;; An export binds a name to a value; an import slot is a name awaiting a
  ;; binding. A binding is a resolved (name value) pair.
  (nm ::= a b c d)
  (export ::= (exp nm v))
  (imp-slot ::= (imp nm))
  (binding ::= (bind nm v))
  (exptab ::= (export ...))
  (imptab ::= (imp-slot ...))
  (bindtab ::= (binding ...))

  ;; --- Module records ---
  ;; A module is (module mid exptab imptab e): an id, an export table, an import
  ;; table, and the entry expression. `e` is a core expression (from ES).
  (module-rec ::= (module mid exptab imptab e))

  ;; --- Link configuration ---
  ;; A link config is two modules plus the binding table being built.
  (link-cfg ::= (link module-rec module-rec bindtab))

  ;; --- Evaluation configuration ---
  ;; An eval config is a module plus the compiled program and a completion value
  ;; (the result of running the entry handler).
  (eval-cfg ::= (eval module-rec prog comp-val))
  (comp-val ::= (normal v) (throw v) (return v) v)

  ;; --- Dynamic import ---
  ;; A request carries the exporting module's id and export table. The event
  ;; slot makes enqueue, dequeue, and idle disjoint scheduler actions.
  (di-req ::= (di mid exptab imp-slot))
  (di-queue ::= (di-req ...))
  (di-settled ::= (binding ...))
  (di-event ::= none di-req)
  (di-cfg ::= (di di-event di-queue di-settled)))

;;; ===========================================================================
;;; Helpers
;;; ===========================================================================

;; Project the export table of a module.
(define-metafunction ES-mod
  module-exports : module-rec -> exptab
  [(module-exports (module mid exptab imptab e)) exptab])

;; Project the import table of a module.
(define-metafunction ES-mod
  module-imports : module-rec -> imptab
  [(module-imports (module mid exptab imptab e)) imptab])

;; Project the entry expression of a module.
(define-metafunction ES-mod
  module-entry : module-rec -> e
  [(module-entry (module mid exptab imptab e)) e])

;; Look an export up by name; returns the exported value, or null if absent.
(define-metafunction ES-mod
  lookup-export : exptab nm -> v
  [(lookup-export () nm_0) null]
  [(lookup-export ((exp nm_0 v_0) (exp nm_1 v_1) ...) nm_0) v_0]
  [(lookup-export ((exp nm_0 v_0) (exp nm_1 v_1) ...) nm_2)
   (lookup-export ((exp nm_1 v_1) ...) nm_2)])

;; Bind a single import slot against an export table.
(define-metafunction ES-mod
  bind-one : exptab imp-slot -> binding
  [(bind-one exptab (imp nm_0))
   (bind nm_0 (lookup-export exptab nm_0))])

;; Bind every import slot in an import table against an export table.
(define-metafunction ES-mod
  bind-all : exptab imptab -> bindtab
  [(bind-all exptab ()) ()]
  [(bind-all exptab (imp-slot_0 imp-slot_1 ...))
   (binding_0 binding_1 ...)
   (where binding_0 (bind-one exptab imp-slot_0))
   (where (binding_1 ...) (bind-all exptab (imp-slot_1 ...)))])

;; Resolve an import table against an export table: the link step.
(define-metafunction ES-mod
  link-resolve : module-rec module-rec -> bindtab
  [(link-resolve module-rec_src module-rec_dst)
   (bind-all exptab_src imptab_dst)
   (where exptab_src (module-exports module-rec_src))
   (where imptab_dst (module-imports module-rec_dst))])

;; Enqueue a dynamic-import request at the tail of the FIFO queue.
(define-metafunction ES-mod
  enqueue-import : di-queue di-req -> di-queue
  [(enqueue-import (di-req ...) di-req_new)
   (di-req ... di-req_new)])

;; Settle a dynamic-import request: resolve the requested slot against the
;; export table carried in the request, producing a binding.
(define-metafunction ES-mod
  settle-import : di-req -> binding
  [(settle-import (di mid exptab (imp nm_0)))
   (bind nm_0 (lookup-export exptab nm_0))])


;;; ===========================================================================
;;; Relation: -->bind  (bind a module's imports against an export table)
;;; ===========================================================================
;;;
;;; -->bind takes (exptab imptab) and produces the fully resolved binding table.
;;; One named rule binds the head slot and recurses; a base rule closes the
;;; empty import table.

(define -->bind
  (reduction-relation ES-mod
    #:domain (exptab imptab)
    #:codomain bindtab
    (--> (exptab (imp-slot_0 imp-slot_1 ...))
         (binding_0 binding_1 ...)
         (where binding_0 (bind-one exptab imp-slot_0))
         (where (binding_1 ...) (bind-all exptab (imp-slot_1 ...)))
        "bind-cons")
    (--> (exptab ())
         ()
        "bind-nil")))

;;; ===========================================================================
;;; Relation: -->link  (link two modules: resolve imports against exports)
;;; ===========================================================================
;;;
;;; -->link takes a link-cfg (two modules plus a binding table accumulator) and
;;; produces the completed binding table. The named rule resolves the second
;;; module's imports against the first module's exports in one step.

(define -->link
  (reduction-relation ES-mod
    #:domain any
    (--> (link module-rec_src module-rec_dst bindtab_init)
         bindtab_resolved
         (where bindtab_resolved (link-resolve module-rec_src module-rec_dst))
        "module_link")))

;;; ===========================================================================
;;; Relation: -->eval  (evaluate a module's entry expression)
;;; ===========================================================================
;;;
;;; -->eval takes an eval-cfg (module, compiled program, completion placeholder)
;;; and produces a completion value by compiling the module's entry expression
;;; and reducing it to a normal completion with the result register's value.
;;; The named rule compiles the entry to a program and yields a normal
;;; completion carrying the entry's value (here modeled as the const value of
;;; the compiled entry's first instruction, or null if none).

(define-metafunction ES-mod
  entry-result-value : e -> v
  [(entry-result-value e)
   v_res
   (where (idx instr_0 instr_1 ...) (comp () 0 e))
   (where v_res (first-const instr_0))])

;; Extract the value from a leading (const v x) instruction; default null.
(define-metafunction ES-mod
  first-const : instr -> v
  [(first-const (const v x)) v]
  [(first-const instr) null])

(define -->eval
  (reduction-relation ES-mod
    #:domain eval-cfg
    #:codomain comp-val
    (--> (eval module-rec_0 prog_old comp-val_old)
         (normal v_res)
         (where e_entry (module-entry module-rec_0))
         (where v_res (entry-result-value e_entry))
        "module_evaluate")))

;;; ===========================================================================
;;; Relation: -->dimport  (dynamic import: enqueue + FIFO settle)
;;; ===========================================================================
;;;
;;; -->dimport carries one external event plus a FIFO request queue. `none`
;;; means no external enqueue is pending; a `di-req` event may enqueue only to
;;; an empty queue. This makes the named actions pairwise disjoint.

(define -->dimport
  (reduction-relation ES-mod
    #:domain di-cfg
    #:codomain di-cfg
    (--> (di none (di-req_0 di-req_1 ...) (binding_old ...))
         (di none (di-req_1 ...) (binding_old ... binding_new))
         (where binding_new (settle-import di-req_0))
        "dynamic_import")
    (--> (di di-req_new () (binding_old ...))
         (di none (di-req_new) (binding_old ...))
        "dynamic-import-enqueue")
    (--> (di none () (binding_old ...))
         (di none () (binding_old ...))
        "dynamic-import-idle")))

;;; The plan's public vocabulary uses these unabbreviated relation names.
(define -->binding -->bind)
(define -->evaluate -->eval)
(define -->dynamic-import -->dimport)

;;; ===========================================================================
;;; Tests
;;; ===========================================================================
(module+ test
  (require rackunit)

  ;; --- Binding: deterministic controls ---
  (check-equal?
   (term (bind-one ((exp a O1) (exp b O17)) (imp a)))
   (term (bind a O1)))
  ;; An absent export binds to null (total model).
  (check-equal?
   (term (bind-one ((exp a O1)) (imp b)))
   (term (bind b null)))
  (check-equal?
   (term (bind-all ((exp a O1) (exp b O17)) ((imp a) (imp b))))
   (term ((bind a O1) (bind b O17))))
  ;; -->bind resolves a non-empty import table.
  (check-equal?
   (apply-reduction-relation* -->bind
                               (term (((exp a O1) (exp b O17)) ((imp a) (imp b)))))
   (term (((bind a O1) (bind b O17)))))
  ;; -->bind on an empty import table yields the empty binding table.
  (check-equal?
   (apply-reduction-relation* -->bind (term (((exp a O1)) ())))
   (term (())))
  ;; -->bind where an import is missing falls back to null but still resolves.
  (check-not-false
   (member (term ((bind a O1) (bind b null)))
           (apply-reduction-relation*
            -->bind (term (((exp a O1)) ((imp a) (imp b)))))))

  ;; --- Linking: deterministic controls ---
  (check-equal?
   (term (module-exports (module m0 ((exp a O1)) ((imp b)) O1)))
   (term ((exp a O1))))
  (check-equal?
   (term (module-imports (module m0 ((exp a O1)) ((imp b)) O1)))
   (term ((imp b))))
  (check-equal?
   (term (link-resolve (module m0 ((exp a O1) (exp b O17)) () O1)
                       (module m1 () ((imp a) (imp b)) O17)))
   (term ((bind a O1) (bind b O17))))
  ;; -->link resolves the second module's imports against the first's exports.
  (check-equal?
   (apply-reduction-relation* -->link
       (term (link (module m0 ((exp a O1) (exp b O17)) () O1)
                   (module m1 () ((imp a) (imp b)) O17)
                   ())))
   (term (((bind a O1) (bind b O17)))))

  ;; --- Evaluation: deterministic controls ---
  (check-equal? (term (entry-result-value O1)) (term O1))
  (check-equal? (term (entry-result-value O17)) (term O17))
  ;; A non-const-leading entry still yields a value (null fallback).
  (check-equal? (term (first-const (mov r0 r1))) (term null))
  ;; -->eval reduces a module's entry to a normal completion carrying the value.
  (check-equal?
   (apply-reduction-relation* -->eval
       (term (eval (module m0 () () O1) (program (entry (ret))) (normal null))))
   (term ((normal O1))))

  ;; --- Dynamic import: deterministic controls ---
  (check-equal?
   (term (enqueue-import ((di m0 ((exp a O1)) (imp a)))
                         (di m1 ((exp b O17)) (imp b))))
   (term ((di m0 ((exp a O1)) (imp a)) (di m1 ((exp b O17)) (imp b)))))
  (check-equal?
   (term (settle-import (di m0 ((exp a O1)) (imp a))))
   (term (bind a O1)))
  ;; An absent export settles to null (total model).
  (check-equal?
   (term (settle-import (di m0 () (imp a))))
   (term (bind a null)))
  ;; -->dimport di-dequeue: FIFO drain settles the head request.
  (check-equal?
   (apply-reduction-relation -->dimport
       (term (di none ((di m0 ((exp a O1)) (imp a))) ())))
   (term ((di none () ((bind a O1))))))
  ;; -->dimport di-idle: no request event and an empty queue are stable.
  (check-equal?
   (apply-reduction-relation -->dimport (term (di none () ())))
   (term ((di none () ()))))
  ;; -->dimport di-enqueue: an external request becomes the queue head.
  (check-equal?
   (apply-reduction-relation -->dimport
       (term (di (di m1 ((exp b O17)) (imp b)) () ())))
   (term ((di none ((di m1 ((exp b O17)) (imp b))) ()))))

  (test-case
   "control::ecmascript/modules.rkt::-->link::deterministic-examples"
   (check-equal?
    (apply-reduction-relation
     -->link
     (term (link (module m0 ((exp a O1) (exp b O17)) () O1)
                 (module m1 () ((imp a) (imp b)) O17)
                 ())))
    (term (((bind a O1) (bind b O17))))))

  (test-case
   "control::ecmascript/modules.rkt::-->link::fixed-seed-redex-check"
   (parameterize ([current-pseudo-random-generator
                   (make-pseudo-random-generator)])
     (random-seed 4101)
     (redex-check
      ES-mod
      (link module-rec_src module-rec_dst bindtab_init)
      (match (apply-reduction-relation
              -->link
              (term (link module-rec_src module-rec_dst bindtab_init)))
        [(list bindings)
         (equal? bindings
                 (term (link-resolve module-rec_src module-rec_dst)))]
        [_ #f])
      #:attempts 100
      #:source -->link)))

  (test-case
   "control::ecmascript/modules.rkt::-->link::named-rule-coverage"
   (check-equal?
    (map car
         (apply-reduction-relation/tag-with-names
          -->link
          (term (link (module m0 ((exp a O1)) () O1)
                      (module m1 () ((imp a)) O17)
                      ()))))
    '("module_link")))

  (test-case
   "control::ecmascript/modules.rkt::-->link::malformed-input-negative"
   (check-equal? (apply-reduction-relation -->link (term O1)) '()))

  (test-case
   "control::ecmascript/modules.rkt::-->link::observable-mutation"
   (define actual
     (apply-reduction-relation
      -->link
      (term (link (module m0 ((exp a O1)) () O1)
                  (module m1 () ((imp a)) O17)
                  ()))))
   (check-equal? actual (term (((bind a O1)))))
   (check-not-equal? actual (term (((bind a null))))))

  (test-case
   "control::ecmascript/modules.rkt::-->evaluate::deterministic-examples"
   (check-equal?
    (apply-reduction-relation
     -->evaluate
     (term (eval (module m0 () () O17) (program (entry (ret))) (normal null))))
    (term ((normal O17)))))

  (test-case
   "control::ecmascript/modules.rkt::-->evaluate::fixed-seed-redex-check"
   (parameterize ([current-pseudo-random-generator
                   (make-pseudo-random-generator)])
     (random-seed 4102)
     (redex-check
      ES-mod
      v_0
      (equal?
       (apply-reduction-relation
        -->evaluate
        (term (eval (module m0 () () v_0)
                    (program (entry (ret)))
                    (normal null))))
       (list (term (normal v_0))))
      #:attempts 100)))

  (test-case
   "control::ecmascript/modules.rkt::-->evaluate::named-rule-coverage"
   (check-equal?
    (map car
         (apply-reduction-relation/tag-with-names
          -->evaluate
          (term (eval (module m0 () () O1)
                      (program (entry (ret)))
                      (normal null)))))
    '("module_evaluate")))

  (test-case
   "control::ecmascript/modules.rkt::-->evaluate::malformed-input-negative"
   (check-exn exn:fail? (lambda () (apply-reduction-relation -->evaluate (term O1)))))

  (test-case
   "control::ecmascript/modules.rkt::-->evaluate::observable-mutation"
   (define actual
     (apply-reduction-relation
      -->evaluate
      (term (eval (module m0 () () O17) (program (entry (ret))) (normal null)))))
   (check-equal? actual (term ((normal O17))))
   (check-not-equal? actual (term ((normal null)))))

  (test-case
   "control::ecmascript/modules.rkt::-->dynamic-import::deterministic-examples"
   (check-equal?
    (apply-reduction-relation
     -->dynamic-import
     (term (di (di m1 ((exp b O17)) (imp b)) () ())))
    (term ((di none ((di m1 ((exp b O17)) (imp b))) ()))))
   (check-equal?
    (apply-reduction-relation
     -->dynamic-import
     (term (di none ((di m1 ((exp b O17)) (imp b))) ())))
    (term ((di none () ((bind b O17))))))
   (check-equal?
    (apply-reduction-relation -->dynamic-import (term (di none () ())))
    (term ((di none () ()))))

  (test-case
   "control::ecmascript/modules.rkt::-->dynamic-import::fixed-seed-redex-check"
   (parameterize ([current-pseudo-random-generator
                   (make-pseudo-random-generator)])
     (random-seed 4103)
     (redex-check
      ES-mod
      di-cfg_0
      (= (length
          (apply-reduction-relation
           -->dynamic-import
           (term di-cfg_0)))
         1)
      #:attempts 100
      #:source -->dynamic-import)))

  (test-case
   "control::ecmascript/modules.rkt::-->dynamic-import::named-rule-coverage"
   (check-equal?
    (sort
     (append
      (map car
           (apply-reduction-relation/tag-with-names
            -->dynamic-import
            (term (di none ((di m0 ((exp a O1)) (imp a))) ()))))
      (map car
           (apply-reduction-relation/tag-with-names
            -->dynamic-import
            (term (di (di m1 ((exp b O17)) (imp b)) () ()))))
      (map car
           (apply-reduction-relation/tag-with-names
            -->dynamic-import
            (term (di none () ())))))
     string<?)
    '("dynamic-import-enqueue" "dynamic-import-idle" "dynamic_import")))

  (test-case
   "control::ecmascript/modules.rkt::-->dynamic-import::malformed-input-negative"
   (check-exn exn:fail? (lambda () (apply-reduction-relation -->dynamic-import (term O1)))))

  (test-case
   "control::ecmascript/modules.rkt::-->dynamic-import::observable-mutation"
   (define actual
     (apply-reduction-relation
      -->dynamic-import
      (term (di none ((di m0 ((exp a O1)) (imp a))) ()))))
   (check-equal? actual (term ((di none () ((bind a O1))))))
   (check-not-equal? actual (term ((di none () ((bind a null)))))))

  ;; --- Malformed negative: a bare value is not a link-cfg, so -->link yields
  ;; no result.
  (check-equal?
   (apply-reduction-relation -->link (term O1))
   (term ()))
  ;; --- Fixed-seed redex-check: -->bind emits one binding per import slot. ---
  (redex-check
   ES-mod
   (exptab_0 (imp-slot_0 ...))
   (match (apply-reduction-relation* -->bind
                                       (term (exptab_0 (imp-slot_0 ...))))
     [(list bindtab)
      (= (length bindtab) (length (term (imp-slot_0 ...))))]
     [_ #f])
   #:attempts 200
   #:source -->bind)

  ;; --- Coverage assertion: every named rule is present. ---
  (check-equal?
   (sort (reduction-relation->rule-names -->bind) symbol<?)
   '(bind-cons bind-nil))
  (check-equal?
   (sort (reduction-relation->rule-names -->link) symbol<?)
   '(module_link))
  (check-equal?
   (sort (reduction-relation->rule-names -->eval) symbol<?)
   '(module_evaluate))
  (check-equal?
   (sort (reduction-relation->rule-names -->dimport) symbol<?)
   '(dynamic-import-enqueue dynamic-import-idle dynamic_import))
  ;; --- Observable mutant guard: link-resolve must reflect exports. A mutant
  ;; that ignores the export table (always binds null) breaks this equality.
  (check-not-equal?
   (term (link-resolve (module m0 ((exp a O1)) () O1)
                       (module m1 () ((imp a)) O17)))
   (term ((bind a null)))))
)

