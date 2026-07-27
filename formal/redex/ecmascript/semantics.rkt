#lang racket

;;; formal/redex/ecmascript/semantics.rkt
;;; PLT Redex 9.2 model of the BamTiScript dynamic semantics: completion
;;; records, iterators, promises, and the async scheduler.
;;;
;;; Requires core.rkt (non-cyclic) and extends ES with the runtime configuration
;;; of the dynamic semantics. Defines four named reduction relations:
;;;   -->complete  completion-record transitions (normal/throw/return/abrupt)
;;;   -->iter      iterator protocol (next/return/throw/done)
;;;   -->promise   promise settlement (pending -> fulfilled | rejected)
;;;   -->async      async scheduler: FIFO microtask drain with N-API lifecycle
;;;
;;; Public surface: ES-sem, the four relations, and the helper metafunctions
;;; (make-completion, completion-type, iter-step, promise-settle, enqueue,
;;; drain-queue).

(require redex/reduction-semantics)
(require "core.rkt")

(provide (all-defined-out))

;;; ===========================================================================
;;; Extended language: ES-sem
;;; ===========================================================================
;;;
;;; Extends ES (from core.rkt) with completion records, iterators, promise
;;; records, and the async configuration (microtask queue + lifecycle state).
;;; This is the only place the dynamic-semantics configuration is defined; ES
;;; itself is not redefined (no duplication within the set).

(define-extended-language ES-sem ES
  ;; --- Completion records (ECMAScript Completion Record spec) ---
  (ctype ::= normal throw return break continue)
  (completion ::= (ctype v))
  (comp-val ::= completion v)

  ;; --- Iterators ---
  ;; An iterator is (iter done v): a done flag and the current value.
  (done? ::= true false)
  (iterator ::= (iter done? v))
  (iterator-result ::= (iter-result done? comp-val))
  (iter-op ::= next return throw)
  (iterator-cfg ::= (iter-cfg iter-op iterator e)) ;; requested iterator operation

  ;; --- Promises ---
  (pstate ::= pending fulfilled rejected)
  (settled ::= fulfilled rejected)
  (promise-record ::= (promise pstate v))
  ;; A promise reference is a heap pointer; modeled as a small pid.
  (pid ::= p0 p1 p2 p3)

  ;; Q is a FIFO queue of microtasks (each a handler name h plus an arg val).
  ;; S is the N-API lifecycle state: running, blocked, or exited. The explicit
  ;; operation selects the scheduler event, keeping the relation deterministic.
  (microtask ::= (task h v))
  (Q ::= (microtask ...))
  (S ::= running blocked exited)
  (async-op ::= drain suspend unblock exit)
  (async-cfg ::= (async async-op Q S))

  ;; --- Runtime value union (so a register/feedback cell may hold any) ---
  (rtv ::= v completion iterator promise-record)

  ;; --- Bounded source-semantics configurations and observations ---
  (source-form ::= (source-async microtask)
                   (source-binding y v)
                   (source-call h v)
                   (source-conditional bool v v)
                   (source-construct h v)
                   (source-iterator done? v)
                   (source-literal v)
                   (source-loop n v)
                   (source-promise v)
                   (source-property-get v y v)
                   (source-property-set v y v)
                   (source-sequence v v)
                   (source-throw v)
                   (source-try-catch-finally v v v)
                   (source-variable y v))
  (source-observation ::= v
                         completion
                         iterator-result
                         promise-record
                         async-cfg
                         (binding-state y v)
                         (call-state h v)
                         (construct-state h v)
                         (property-state v y v)
                         (try-state v v)))

;;; ===========================================================================
;;; Completion helpers & relation: -->complete
;;; ===========================================================================

(define-metafunction ES-sem
  make-completion : ctype v -> completion
  [(make-completion ctype_0 v_0) (ctype_0 v_0)])

(define-metafunction ES-sem
  completion-type : completion -> ctype
  [(completion-type (ctype_0 v_0)) ctype_0])

(define-metafunction ES-sem
  completion-value : completion -> v
  [(completion-value (ctype_0 v_0)) v_0])

;;; -->complete: transition a completion record. Mirrors the ECMAScript
;;; Completion Record semantics: a normal completion may be returned or thrown;
;;; a throw completion propagates; abrupt completions (break/continue/return)
;;; unwind.
(define -->complete
  (reduction-relation ES-sem
    #:domain any
    #:codomain completion
    (--> (normal v_0)
         (return v_0)
        "normal->return")
    (--> (throw v_0)
         (throw v_0)
        "throw-propagate")
    (--> (return v_0)
         (return v_0)
        "return-keep")
    (--> (break v_0)
         (break v_0)
        "break-keep")
    (--> (continue v_0)
         (continue v_0)
        "continue-keep")))

;;; ===========================================================================
;;; Iterator helpers & relation: -->iter
;;; ===========================================================================

(define-metafunction ES-sem
  iter-done : iterator -> done?
  [(iter-done (iter done?_0 v_0)) done?_0])

(define-metafunction ES-sem
  iter-value : iterator -> v
  [(iter-value (iter done?_0 v_0)) v_0])

;;; -->iter: the iterator protocol. `next` advances a not-done iterator to the
;;; next value; `return` closes it (marks done and returns the value); `throw`
;;; raises (turns the iterator into a throw completion).
(define -->iter
  (reduction-relation ES-sem
    #:domain iterator-cfg
    #:codomain iterator-result
    (--> (iter-cfg next (iter false v_0) e_0)
         (iter-result false (normal v_0))
        "iter-next")
    (--> (iter-cfg return (iter done?_0 v_0) e_0)
         (iter-result true (return v_0))
        "iter-return")
    (--> (iter-cfg throw (iter done?_0 v_0) e_0)
         (iter-result true (throw v_0))
        "iter-throw")
    (--> (iter-cfg next (iter true v_0) e_0)
         (iter-result true (normal v_0))
        "iter-done")))

;;; ===========================================================================
;;; Promise helpers & relation: -->promise
;;; ===========================================================================

(define-metafunction ES-sem
  promise-state : promise-record -> pstate
  [(promise-state (promise pstate_0 v_0)) pstate_0])

(define-metafunction ES-sem
  promise-value : promise-record -> v
  [(promise-value (promise pstate_0 v_0)) v_0])

;;; promise-settle settles a pending promise to fulfilled or rejected. A
;;; settled promise stays settled (idempotent).
(define-metafunction ES-sem
  promise-settle : promise-record settled v -> promise-record
  [(promise-settle (promise pending v_0) settled_0 v_1)
   (promise settled_0 v_1)]
  [(promise-settle (promise fulfilled v_0) settled_0 v_1)
   (promise fulfilled v_0)]
  [(promise-settle (promise rejected v_0) settled_0 v_1)
   (promise rejected v_0)])

;;; -->promise: pending promises may settle to fulfilled or rejected; settled
;;; promises are stable.
(define -->promise
  (reduction-relation ES-sem
    #:domain promise-record
    #:codomain promise-record
    (--> (promise pending v_0)
         (promise fulfilled v_0)
        "resolve")
    (--> (promise pending v_0)
         (promise rejected v_0)
        "reject")
    (--> (promise fulfilled v_0)
         (promise fulfilled v_0)
        "fulfilled-stable")
    (--> (promise rejected v_0)
         (promise rejected v_0)
        "rejected-stable")))

;;; ===========================================================================
;;; Async scheduler helpers & relation: -->async
;;; ===========================================================================
;;;
;;; The async scheduler is a FIFO microtask queue with explicit N-API lifecycle
;;; transitions. A running scheduler drains the head task (FIFO); when the queue
;;; is empty, it exits. A blocked scheduler stays blocked (awaiting an external
;;; N-API callback). Tasks are added by enqueue.

(define-metafunction ES-sem
  enqueue : Q microtask -> Q
  [(enqueue (microtask_0 ...) microtask_1)
   (microtask_0 ... microtask_1)])

(define-metafunction ES-sem
  enqueue* : Q (microtask ...) -> Q
  [(enqueue* Q_0 ()) Q_0]
  [(enqueue* Q_0 (microtask_0 microtask_1 ...))
   (enqueue* (enqueue Q_0 microtask_0) (microtask_1 ...))])

(define-metafunction ES-sem
  queue-head : Q -> microtask
  [(queue-head (microtask_0 microtask_1 ...)) microtask_0]
  [(queue-head ()) (task entry null)])

(define-metafunction ES-sem
  queue-rest : Q -> Q
  [(queue-rest (microtask_0 microtask_1 ...)) (microtask_1 ...)]
  [(queue-rest ()) ()])

;;; -->async: FIFO drain + N-API lifecycle. Named rules:
;;;   drain      : running + non-empty -> running, head task removed (executed)
;;;   block      : running -> blocked (N-API callback pending)
;;;   unblock    : blocked -> running (N-API callback delivered)
;;;   exit       : running + empty -> exited (no more microtasks)
;;;   noop       : exited stays exited
(define -->async
  (reduction-relation ES-sem
    #:domain async-cfg
    #:codomain async-cfg
    (--> (async drain (microtask_0 microtask_1 ...) running)
         (async drain (microtask_1 ...) running)
        "drain")
    (--> (async suspend (microtask_0 ...) running)
         (async suspend (microtask_0 ...) blocked)
        "block")
    (--> (async unblock (microtask_0 ...) blocked)
         (async unblock (microtask_0 ...) running)
        "unblock")
    (--> (async exit () running)
         (async exit () exited)
        "exit")
    (--> (async exit () exited)
         (async exit () exited)
        "noop")))

;;; ===========================================================================
;;; Bounded source semantics
;;; ===========================================================================

(define-metafunction ES-sem
  select-source-branch : bool v v -> v
  [(select-source-branch true v_then v_else) v_then]
  [(select-source-branch false v_then v_else) v_else])

(define -->source
  (reduction-relation ES-sem
    #:domain source-form
    #:codomain source-observation
    (--> (source-async microtask_0)
         (async drain (microtask_0) running)
         "async")
    (--> (source-binding y_0 v_0)
         (binding-state y_0 v_0)
         "binding")
    (--> (source-call h_0 v_0)
         (call-state h_0 v_0)
         "call")
    (--> (source-conditional bool_0 v_then v_else)
         v_selected
         (where v_selected
                (select-source-branch bool_0 v_then v_else))
         "conditional")
    (--> (source-construct h_0 v_0)
         (construct-state h_0 v_0)
         "construct")
    (--> (source-iterator done?_0 v_0)
         (iter-result done?_0 (normal v_0))
         "iterator")
    (--> (source-literal v_0)
         v_0
         "literal")
    (--> (source-loop n_0 v_0)
         (normal v_0)
         "loop")
    (--> (source-promise v_0)
         (promise fulfilled v_0)
         "promise")
    (--> (source-property-get v_object y_0 v_property)
         v_property
         "property_get")
    (--> (source-property-set v_object y_0 v_property)
         (property-state v_object y_0 v_property)
         "property_set")
    (--> (source-sequence v_first v_second)
         v_second
         "sequence")
    (--> (source-throw v_0)
         (throw v_0)
         "throw")
    (--> (source-try-catch-finally v_thrown v_catch v_finally)
         (try-state v_catch v_finally)
         "try_catch_finally")
    (--> (source-variable y_0 v_0)
         v_0
         "variable")))

;;; ===========================================================================
;;; Tests
;;; ===========================================================================
(module+ test
  (require rackunit racket/list racket/set "modules.rkt")

  (define (executed-rule-names relation sources)
    (sort
     (remove-duplicates
      (map (lambda (name) (if (string? name) (string->symbol name) name))
           (append-map
            (lambda (source)
              (map car (apply-reduction-relation/tag-with-names relation source)))
            sources)))
     symbol<?))

  (define source-coverage-cases
    (list (term (source-async (task f O1)))
          (term (source-binding a O1))
          (term (source-call f O1))
          (term (source-conditional true O1 O17))
          (term (source-construct f O17))
          (term (source-iterator false O1))
          (term (source-literal O17))
          (term (source-loop O1 O17))
          (term (source-promise O1))
          (term (source-property-get O1 a O17))
          (term (source-property-set O1 a O17))
          (term (source-sequence O1 O17))
          (term (source-throw O1))
          (term (source-try-catch-finally O1 O17 Z))
          (term (source-variable a O1))))

  (check-equal?
   (executed-rule-names -->source source-coverage-cases)
   '(async binding call conditional construct iterator literal loop promise
           property_get property_set sequence throw try_catch_finally variable))

  (check-exn
   #rx"not in domain"
   (lambda () (apply-reduction-relation -->source (term O1))))

  (define -->source-conditional-else-mutant
    (reduction-relation
     ES-sem
     #:domain source-form
     #:codomain source-observation
     (--> (source-conditional bool_0 v_then v_else)
          v_else
          "conditional-else-mutant")))
  (define conditional-source (term (source-conditional true O1 O17)))
  (check-not-equal?
   (apply-reduction-relation -->source conditional-source)
   (apply-reduction-relation -->source-conditional-else-mutant
                             conditional-source))

  ;; --- Completion: deterministic controls ---
  (check-equal? (term (make-completion normal O1)) (term (normal O1)))
  (check-equal? (term (completion-type (throw O1))) (term throw))
  (check-equal? (term (completion-value (return O17))) (term O17))
  ;; normal completion transitions to return.
  (check-equal?
   (apply-reduction-relation -->complete (term (normal O1)))
   (term ((return O1))))
  ;; throw propagates (stays a throw completion).
  (check-equal?
   (apply-reduction-relation -->complete (term (throw O1)))
   (term ((throw O1))))

  ;; --- Iterator: deterministic controls ---
  (check-equal? (term (iter-done (iter false O1))) (term false))
  (check-equal? (term (iter-value (iter true O17))) (term O17))
  ;; A not-done iterator's next yields a normal completion with the value.
  (check-equal?
   (apply-reduction-relation -->iter
                               (term (iter-cfg next (iter false O1) O1)))
   (term ((iter-result false (normal O1)))))
  ;; A done iterator's next yields a normal completion (iter-done).
  (check-equal?
   (apply-reduction-relation -->iter
                               (term (iter-cfg next (iter true O1) O1)))
   (term ((iter-result true (normal O1)))))
  ;; Explicit return and throw operations are also single, named transitions.
  (check-equal?
   (apply-reduction-relation -->iter
                               (term (iter-cfg return (iter false O17) O1)))
   (term ((iter-result true (return O17)))))
  (check-equal?
   (apply-reduction-relation -->iter
                               (term (iter-cfg throw (iter true O17) O1)))
   (term ((iter-result true (throw O17)))))

  ;; --- Promise: deterministic controls ---
  (check-equal? (term (promise-state (promise pending O1))) (term pending))
  (check-equal? (term (promise-value (promise fulfilled O17))) (term O17))
  ;; A pending promise settles to fulfilled (resolve) or rejected (reject).
  (check-not-false
   (member (term (promise fulfilled O1))
           (apply-reduction-relation -->promise (term (promise pending O1)))))
  (check-not-false
   (member (term (promise rejected O1))
           (apply-reduction-relation -->promise (term (promise pending O1)))))
  ;; A fulfilled promise is stable.
  (check-equal?
   (apply-reduction-relation -->promise (term (promise fulfilled O1)))
   (term ((promise fulfilled O1))))
  ;; promise-settle on a pending promise sets the new state and value.
  (check-equal?
   (term (promise-settle (promise pending O1) fulfilled O17))
   (term (promise fulfilled O17)))
  ;; promise-settle on a settled promise is idempotent.
  (check-equal?
   (term (promise-settle (promise fulfilled O1) rejected O17))
   (term (promise fulfilled O1)))

  ;; --- Async scheduler: deterministic controls ---
  (check-equal? (term (enqueue () (task f O1))) (term ((task f O1))))
  (check-equal? (term (queue-head ((task f O1) (task entry O17))))
                (term (task f O1)))
  (check-equal? (term (queue-rest ((task f O1) (task entry O17))))
                (term ((task entry O17))))
  ;; FIFO drain: head task removed, still running.
  (check-equal?
   (apply-reduction-relation -->async
                               (term (async drain ((task f O1) (task entry O17)) running)))
   (term ((async drain ((task entry O17)) running))))
  ;; Empty + running -> exited.
  (check-equal?
   (apply-reduction-relation -->async (term (async exit () running)))
   (term ((async exit () exited))))
  ;; Exited stays exited.
  (check-equal?
   (apply-reduction-relation -->async (term (async exit () exited)))
   (term ((async exit () exited))))
  ;; Block and unblock are explicit external scheduler events.
  (check-equal?
   (apply-reduction-relation -->async (term (async suspend () running)))
   (term ((async suspend () blocked))))
  (check-equal?
   (apply-reduction-relation -->async (term (async unblock () blocked)))
   (term ((async unblock () running))))

  ;; --- Malformed negative: a bare value is not a completion, so -->complete
  ;; yields no result.
  (check-equal?
   (apply-reduction-relation -->complete (term O1))
   (term ()))

  ;; --- Fixed-seed redex-check: every settled promise is stable. ---
  (parameterize ([current-pseudo-random-generator
                  (make-pseudo-random-generator)])
    (random-seed 4000)
    (redex-check
     ES-sem
     (promise fulfilled v_0)
     (equal? (apply-reduction-relation -->promise (term (promise fulfilled v_0)))
             (term ((promise fulfilled v_0))))
     #:attempts 200
     #:source -->promise))

  ;; --- Coverage assertion: every named rule is present. ---
  (check-equal?
   (sort (reduction-relation->rule-names -->complete) symbol<?)
   '(break-keep continue-keep normal->return return-keep throw-propagate))
  (check-equal?
   (sort (reduction-relation->rule-names -->iter) symbol<?)
   '(iter-done iter-next iter-return iter-throw))
  (check-equal?
   (sort (reduction-relation->rule-names -->promise) symbol<?)
   '(fulfilled-stable reject rejected-stable resolve))
  (check-equal?
   (sort (reduction-relation->rule-names -->async) symbol<?)
   '(block drain exit noop unblock))

  ;; --- Observable mutant guard: drain must remove exactly the head task. ---
  (check-equal?
   (apply-reduction-relation -->async
                               (term (async drain ((task f O1) (task entry O17)) running)))
   (term ((async drain ((task entry O17)) running))))

  (test-case "control::ecmascript/semantics.rkt::-->JS::deterministic-examples"
    (check-equal?
     (apply-reduction-relation -->JS (term (() 0 O17)))
     (term ((1 (const O17 r0))))))

  (test-case "control::ecmascript/semantics.rkt::-->JS::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 4101)
      (redex-check
       ES
       (env_0 idx_0 v_0)
       (match (apply-reduction-relation -->JS (term (env_0 idx_0 v_0)))
         [(list (list idx_1 (list 'const value register)))
          (and (equal? idx_1 (add1 (term idx_0)))
               (equal? value (term v_0))
               (equal? register (term (fresh-reg idx_0))))]
         [_ #f])
       #:attempts 200
       #:source -->JS)))

  (test-case "control::ecmascript/semantics.rkt::-->JS::named-rule-coverage"
    (check-equal?
     (executed-rule-names
      -->JS
      (list (term (() 0 O1))
            (term (((a r2)) 0 a))
            (term (() 0 (add O1 O17)))
            (term (() 0 (sub O17 O1)))
            (term (() 0 (is-null null Z)))
            (term (() 0 (not false)))
            (term (() 0 (let O1 O17)))
            (term (() 0 (if O1 O17 Z)))
            (term (() 0 (seq O1 O17)))
            (term (() 0 (fun f O1)))))
     '(bin-add bin-is-null bin-sub if-branch let-bind load-value load-var
               make-handler seq-eval un-not)))

  (test-case "control::ecmascript/semantics.rkt::-->JS::malformed-input-negative"
    (check-equal?
     (apply-reduction-relation -->JS (term (() 0 r0)))
     '()))

  (test-case "control::ecmascript/semantics.rkt::-->JS::observable-mutation"
    (define -->JS-no-index-bump-mutant
      (reduction-relation
       ES
       #:domain (env idx v)
       #:codomain out
       (--> (env idx v)
            (idx (const v x))
            (where x (fresh-reg idx))
            "load-value-no-index-bump-mutant")))
    (define source (term (() 0 O1)))
    (check-not-equal?
     (apply-reduction-relation -->JS source)
     (apply-reduction-relation -->JS-no-index-bump-mutant source)))

  (test-case "control::ecmascript/semantics.rkt::-->async::deterministic-examples"
    (check-equal?
     (apply-reduction-relation
      -->async
      (term (async drain ((task f O1) (task entry O17)) running)))
     (term ((async drain ((task entry O17)) running)))))

  (test-case "control::ecmascript/semantics.rkt::-->async::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 4201)
      (redex-check
       ES-sem
       (async drain (microtask_0 microtask_1 ...) running)
       (equal?
        (apply-reduction-relation
         -->async
         (term (async drain (microtask_0 microtask_1 ...) running)))
        (term ((async drain (microtask_1 ...) running))))
       #:attempts 200
       #:source -->async)))

  (test-case "control::ecmascript/semantics.rkt::-->async::named-rule-coverage"
    (check-equal?
     (executed-rule-names
      -->async
      (list (term (async drain ((task f O1)) running))
            (term (async suspend () running))
            (term (async unblock () blocked))
            (term (async exit () running))
            (term (async exit () exited))))
     '(block drain exit noop unblock)))

  (test-case "control::ecmascript/semantics.rkt::-->async::malformed-input-negative"
    (check-exn #rx"not in domain" (lambda () (apply-reduction-relation -->async (term O1)))))

  (test-case "control::ecmascript/semantics.rkt::-->async::observable-mutation"
    (define -->async-drain-tail-mutant
      (reduction-relation
       ES-sem
       #:domain async-cfg
       #:codomain async-cfg
       (--> (async drain (microtask_0 microtask_1) running)
            (async drain (microtask_0) running)
            "drain-tail-mutant")))
    (define source
      (term (async drain ((task f O1) (task entry O17)) running)))
    (check-not-equal?
     (apply-reduction-relation -->async source)
     (apply-reduction-relation -->async-drain-tail-mutant source)))

  (test-case "control::ecmascript/semantics.rkt::-->binding::deterministic-examples"
    (check-equal?
     (apply-reduction-relation
      -->binding
      (term (((exp a O1) (exp b O17)) ((imp a) (imp b)))))
     (term (((bind a O1) (bind b O17))))))

  (test-case "control::ecmascript/semantics.rkt::-->binding::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 4301)
      (redex-check
       ES-mod
       (exptab_0 (imp-slot_0 ...))
       (match
           (apply-reduction-relation
            -->binding
            (term (exptab_0 (imp-slot_0 ...))))
         [(list bindings)
          (= (length bindings) (length (term (imp-slot_0 ...))))]
         [_ #f])
       #:attempts 200
       #:source -->binding)))

  (test-case "control::ecmascript/semantics.rkt::-->binding::named-rule-coverage"
    (check-equal?
     (executed-rule-names
      -->binding
      (list (term (((exp a O1)) ((imp a))))
            (term (((exp a O1)) ()))))
     '(bind-cons bind-nil)))

  (test-case "control::ecmascript/semantics.rkt::-->binding::malformed-input-negative"
    (check-exn #rx"not in domain" (lambda () (apply-reduction-relation -->binding (term O1)))))

  (test-case "control::ecmascript/semantics.rkt::-->binding::observable-mutation"
    (define -->binding-null-mutant
      (reduction-relation
       ES-mod
       #:domain (exptab imptab)
       #:codomain bindtab
       (--> (exptab_0 ((imp nm_0)))
            ((bind nm_0 null))
            "bind-null-mutant")))
    (define source (term (((exp a O1)) ((imp a)))))
    (check-not-equal?
     (apply-reduction-relation -->binding source)
     (apply-reduction-relation -->binding-null-mutant source)))

  (test-case "control::ecmascript/semantics.rkt::-->completion::deterministic-examples"
    (check-equal?
     (apply-reduction-relation -->completion (term (normal O17)))
     (term ((return O17)))))

  (test-case "control::ecmascript/semantics.rkt::-->completion::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 4401)
      (redex-check
       ES-sem
       completion_0
       (= (length
           (apply-reduction-relation -->completion (term completion_0)))
          1)
       #:attempts 200
       #:source -->completion)))

  (test-case "control::ecmascript/semantics.rkt::-->completion::named-rule-coverage"
    (check-equal?
     (executed-rule-names
      -->completion
      (list (term (normal O1))
            (term (throw O1))
            (term (return O1))
            (term (break O1))
            (term (continue O1))))
     '(break-keep continue-keep normal->return return-keep throw-propagate)))

  (test-case "control::ecmascript/semantics.rkt::-->completion::malformed-input-negative"
    (check-equal? (apply-reduction-relation -->completion (term O1)) '()))

  (test-case "control::ecmascript/semantics.rkt::-->completion::observable-mutation"
    (define -->completion-normal-stable-mutant
      (reduction-relation
       ES-sem
       #:domain any
       #:codomain completion
       (--> (normal v_0)
            (normal v_0)
            "normal-stable-mutant")))
    (define source (term (normal O1)))
    (check-not-equal?
     (apply-reduction-relation -->completion source)
     (apply-reduction-relation -->completion-normal-stable-mutant source)))

  (test-case "control::ecmascript/semantics.rkt::-->iterator::deterministic-examples"
    (check-equal?
     (apply-reduction-relation
      -->iterator
      (term (iter-cfg next (iter false O17) O1)))
     (term ((iter-result false (normal O17))))))

  (test-case "control::ecmascript/semantics.rkt::-->iterator::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 4501)
      (redex-check
       ES-sem
       (iter-cfg iter-op_0 iterator_0 e_0)
       (= (length
           (apply-reduction-relation
            -->iterator
            (term (iter-cfg iter-op_0 iterator_0 e_0))))
          1)
       #:attempts 200
       #:source -->iterator)))

  (test-case "control::ecmascript/semantics.rkt::-->iterator::named-rule-coverage"
    (check-equal?
     (executed-rule-names
      -->iterator
      (list (term (iter-cfg next (iter false O1) O1))
            (term (iter-cfg return (iter false O1) O1))
            (term (iter-cfg throw (iter true O1) O1))
            (term (iter-cfg next (iter true O1) O1))))
     '(iter-done iter-next iter-return iter-throw)))

  (test-case "control::ecmascript/semantics.rkt::-->iterator::malformed-input-negative"
    (check-exn #rx"not in domain" (lambda () (apply-reduction-relation -->iterator (term O1)))))

  (test-case "control::ecmascript/semantics.rkt::-->iterator::observable-mutation"
    (define -->iterator-next-done-mutant
      (reduction-relation
       ES-sem
       #:domain iterator-cfg
       #:codomain iterator-result
       (--> (iter-cfg next (iter false v_0) e_0)
            (iter-result true (normal v_0))
            "iter-next-done-mutant")))
    (define source (term (iter-cfg next (iter false O1) O1)))
    (check-not-equal?
     (apply-reduction-relation -->iterator source)
     (apply-reduction-relation -->iterator-next-done-mutant source)))

  (test-case "control::ecmascript/semantics.rkt::-->promise::deterministic-examples"
    (check-equal?
     (list->set
      (apply-reduction-relation -->promise (term (promise pending O1))))
     (set (term (promise fulfilled O1))
          (term (promise rejected O1)))))

  (test-case "control::ecmascript/semantics.rkt::-->promise::fixed-seed-redex-check"
    (parameterize ([current-pseudo-random-generator
                    (make-pseudo-random-generator)])
      (random-seed 4601)
      (redex-check
       ES-sem
       (promise fulfilled v_0)
       (equal?
        (apply-reduction-relation -->promise (term (promise fulfilled v_0)))
        (term ((promise fulfilled v_0))))
       #:attempts 200
       #:source -->promise)))

  (test-case "control::ecmascript/semantics.rkt::-->promise::named-rule-coverage"
    (check-equal?
     (executed-rule-names
      -->promise
      (list (term (promise pending O1))
            (term (promise fulfilled O1))
            (term (promise rejected O1))))
     '(fulfilled-stable reject rejected-stable resolve)))

  (test-case "control::ecmascript/semantics.rkt::-->promise::malformed-input-negative"
    (check-exn #rx"not in domain" (lambda () (apply-reduction-relation -->promise (term O1)))))

  (test-case "control::ecmascript/semantics.rkt::-->promise::observable-mutation"
    (define -->promise-null-resolve-mutant
      (reduction-relation
       ES-sem
       #:domain promise-record
       #:codomain promise-record
       (--> (promise pending v_0)
            (promise fulfilled null)
            "resolve-null-mutant")
       (--> (promise pending v_0)
            (promise rejected v_0)
            "reject-mutant")))
    (define source (term (promise pending O1)))
    (check-not-equal?
     (list->set (apply-reduction-relation -->promise source))
     (list->set
      (apply-reduction-relation -->promise-null-resolve-mutant source)))))
;;; The plan's public vocabulary uses these unabbreviated relation names.
(define -->completion -->complete)
(define -->iterator -->iter)
