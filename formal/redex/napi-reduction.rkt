#lang racket

;;; formal/redex/napi-reduction.rkt
;;; PLT Redex 9.2 model of the BamTiScript Node-API (N-API) lifecycle.
;;;
;;; The model pins down the explicit Node-API lifecycle reductions: handle
;;; (napi_ref) creation, strong/weak reference unref, retire (which bumps
;;; the slot generation — a retired handle becomes a stale handle), the
;;; stale-handle rejection on deref (resolves), the exactly-once finalizer
;;; transition, the epoch-safe reclaim, and the teardown that drops roots.
;;; Every rule is named; the rules are pairwise disjoint so the relation is
;;; deterministic (one successor per reachable configuration).
;;;
;;; Sibling anchors mirrored here:
;;;   * formal/lean/Bamti/NapiLifecycle.lean
;;;       NapiState { env owner roots completed finalized phase };
;;;       TeardownPhase = active | closing | closed;
;;;       napiWellFormed: owner = env, (closed -> roots = []);
;;;       finish/finalize are idempotent (at_most_once_*); closeNapi sets
;;;       phase := closed, roots := [].
;;;   * formal/lean/Bamti/Abi.lean
;;;       FatHandle { slot generation }; retire h = {h with generation := +1};
;;;       resolves table h = table = h; stale_after_retire: ¬ resolves
;;;       (retire h) h; generation_never_wraps.
;;;   * formal/quint/napi_lifecycle.qnt
;;;       actions openScope/closeScope/teardown; invariants InvOwnerGeneration
;;;       (callbackOwner==owner, generation==handleGeneration), InvScopeLifo,
;;;       InvRefRoot, InvAsyncWork, InvTsfn, InvTeardownOrder,
;;;       InvFinalizer (not(finalized and finalizerQueued)), InvAbiCap
;;;       (1 <= requestedAbi <= MaxAbi=9).
;;;   * formal/quint/gc/core.qnt + lifecycle.qnt + model.qnt
;;;       slot(live, generation, marked, remembered, address);
;;;       resolves(s, h) = s.live and s.generation == h.generation;
;;;       epochSafeReclaim(l) = l.retiredEpoch < l.epoch and l.readers == 0;
;;;       finalizerExactlyOnce(l) = not(l.finalized and l.finalizerPending);
;;;       retire bumps generation; seal: live:=false, generation:=+1,
;;;       retireEpoch:=markEpoch, finalized:=true, shutdown:=true.
;;;
;;; The configuration is
;;;   (napi env tab hgen scope roots async tsfn phase fq fz abi cl)
;;; where
;;;   env   : the napi_env identity (a Nat; mirrors NapiState.env)
;;;   tab   : the slot table, a list of (slot sid live gen marked) entries
;;;           (mirrors gc/core.qnt `slot` and Abi.lean FatHandle per slot)
;;;   hgen  : the handle generation carried by the live handle (Abi.lean
;;;           FatHandle.generation; InvOwnerGeneration: gen == hgen)
;;;   scope : scope depth (InvScopeLifo: 0 <= scope <= TestBound=3)
;;;   roots : strong ref-root count (InvRefRoot: 0 <= roots <= 3)
;;;   async : pending async-work count (InvAsyncWork: 0 <= async <= 3)
;;;   tsfn  : pending thread-safe-function count (InvTsfn: 0 <= tsfn <= 3)
;;;   phase : TeardownPhase = active | closing | closed (NapiLifecycle.lean)
;;;   fq    : finalizer-queued flag (gc finalizerPending)
;;;   fz    : finalized flag (gc finalized; at_most_once_finalizer)
;;;   abi   : requested ABI version (InvAbiCap: 1 <= abi <= MaxAbi=9)
;;;   cl    : a callback-after-teardown flag (InvTeardownOrder: not cl)
;;;
;;; Slot table entries: (slot sid live gen marked). `sid` is a finite slot
;;; id (s0..s3), `live` is the liveness bit (gc/core.qnt slot.live), `gen` is
;;; the slot generation (gc slot.generation / Abi.lean FatHandle.generation
;;; — retire bumps it), `marked` is the mark bit (gc slot.marked — reclaim
;;; requires marked).
;;;
;;; Non-cyclic import graph (the scheduler set):
;;;   napi-reduction.rkt (this file) -- standalone, requires only redex.
;;;   node-loop.rkt -- standalone, requires only redex.
;;; The two never require each other; a future harness may require both and
;;; compose them. No language definition is duplicated across the set: this
;;; module owns NAPI; node-loop.rkt owns NL.
;;;
;;; Public surface: NAPI, the relation napi-lifecycle, the metafunctions resolves?,
;;; retire-slot, epoch-safe?, finalizer-once?, napi-well-formed?, and the
;;; invariant predicates inv-owner-generation, inv-scope-lifo, inv-ref-root,
;;; inv-async-work, inv-tsfn, inv-teardown-order, inv-finalizer, inv-abi-cap.

(require redex/reduction-semantics
         racket/list)

(provide
 ;; Language
 NAPI
 ;; Relation
callback-capability
napi-lifecycle
 ;; Metafunctions / predicates
 resolves?
 retire-slot
 epoch-safe?
 finalizer-once?
 napi-well-formed?
 lookup-slot
 bump-gen
 marked-slot?
 ;; Invariants
 inv-owner-generation
 inv-scope-lifo
 inv-ref-root
 inv-async-work
 inv-tsfn
 inv-teardown-order
 inv-finalizer
 inv-abi-cap)

;;; ===========================================================================
;;; Language: NAPI  (the Node-API lifecycle)
;;; ===========================================================================

(define-language NAPI
  ;; --- Finite slot / handle alphabet (so redex-check is exhaustive) ---
  (sid ::= s0 s1 s2 s3)
  (b ::= true false)

  ;; --- Slot table and handles ---
  ;; `handle` must not share the constructor's name: a declared Redex
  ;; nonterminal in a production is a matcher, rather than a literal tag.
  (entry ::= (slot sid b gen b))
  (tab ::= (entry ...))
  (handle ::= (h sid gen))
  ;; --- Lifecycle state ---
  (phase ::= active closing closed)
  (gen ::= natural)
  (cnt ::= natural)
  (abi ::= 1 2 3 4 5 6 7 8 9)
  ;; (napi env tab hgen scope roots async tsfn phase fq fz abi cl)
  (cfg ::= (napi env tab gen cnt cnt cnt cnt phase b b abi b))
  ;; (cap tab handle phase enabled): a callback capability is valid only for
  ;; the live slot generation that granted it and until lifecycle revocation.
  (capcfg ::= (cap tab handle phase b))
  (env ::= natural))

;;; ===========================================================================
;;; Slot table helpers (metafunctions)
;;; ===========================================================================

;; Lookup is total. The recursive clause only skips a nonmatching head, so the
;; matching clause remains the unique answer for a slot id.
(define-metafunction NAPI
  lookup-slot : tab sid -> entry
  [(lookup-slot ((slot sid_0 b_live gen_0 b_marked) entry_1 ...) sid_0)
   (slot sid_0 b_live gen_0 b_marked)]
  [(lookup-slot ((slot sid_0 b_live gen_0 b_marked) entry_1 ...) sid_1)
   (lookup-slot (entry_1 ...) sid_1)
   (side-condition (not (equal? (term sid_0) (term sid_1))))]
  [(lookup-slot () sid_0) (slot sid_0 false 0 false)])

(define-metafunction NAPI
  resolves? : tab handle -> b
  [(resolves? tab_0 (h sid_0 gen_hgen))
   ,(if (and (equal? (term b_live) 'true)
             (= (term gen_0) (term gen_hgen)))
        'true
        'false)
   (where (slot sid_0 b_live gen_0 b_marked) (lookup-slot tab_0 sid_0))])

;; Retiring a slot preserves its liveness but advances its generation, making
;; every handle carrying the old generation stale.
(define-metafunction NAPI
  retire-slot : tab sid -> tab
  [(retire-slot ((slot sid_0 b_live gen_0 b_marked) entry_1 ...) sid_0)
   ((slot sid_0 b_live gen_1 b_marked) entry_1 ...)
   (where gen_1 ,(add1 (term gen_0)))]
  [(retire-slot ((slot sid_0 b_live gen_0 b_marked) entry_1 ...) sid_1)
   ((slot sid_0 b_live gen_0 b_marked) entry_2 ...)
   (side-condition (not (equal? (term sid_0) (term sid_1))))
   (where (entry_2 ...) (retire-slot (entry_1 ...) sid_1))]
  [(retire-slot () sid_0) ()])

(define-metafunction NAPI
  marked-slot? : tab sid -> b
  [(marked-slot? tab_0 sid_0)
   b_marked
   (where (slot sid_0 b_live gen_0 b_marked) (lookup-slot tab_0 sid_0))])

(define-metafunction NAPI
  bump-gen : gen -> gen
  [(bump-gen gen_0) ,(add1 (term gen_0))])

;; Epoch-safe reclamation requires a dead, marked slot from an earlier epoch
;; and no outstanding roots.
(define-metafunction NAPI
  epoch-safe? : tab handle cnt -> b
  [(epoch-safe? tab_0 (h sid_0 gen_hgen) cnt_roots)
   ,(if (and (equal? (term b_live) 'false)
             (equal? (term b_marked) 'true)
             (< (term gen_0) (term gen_hgen))
             (= (term cnt_roots) 0))
        'true
        'false)
   (where (slot sid_0 b_live gen_0 b_marked) (lookup-slot tab_0 sid_0))])

(define-metafunction NAPI
  finalizer-once? : b b -> b
  [(finalizer-once? true true) false]
  [(finalizer-once? true false) true]
  [(finalizer-once? false true) true]
  [(finalizer-once? false false) true])

(define (napi-well-formed? candidate)
  (redex-match? NAPI cfg candidate))

;;; ===========================================================================
;;; Invariants (mirrors napi_lifecycle.qnt Inv* and NapiLifecycle.lean)
;;; ===========================================================================

(define-metafunction NAPI
  inv-owner-generation : tab gen -> b
  [(inv-owner-generation () gen_hgen) true]
  [(inv-owner-generation ((slot sid_0 false gen_0 b_marked) entry_1 ...) gen_hgen)
   (inv-owner-generation (entry_1 ...) gen_hgen)]
  [(inv-owner-generation ((slot sid_0 true gen_hgen b_marked) entry_1 ...) gen_hgen)
   (inv-owner-generation (entry_1 ...) gen_hgen)]
  [(inv-owner-generation ((slot sid_0 true gen_0 b_marked) entry_1 ...) gen_hgen)
   false
   (side-condition (not (= (term gen_0) (term gen_hgen))))])

(define-metafunction NAPI
  inv-scope-lifo : cnt -> b
  [(inv-scope-lifo cnt_0)
   ,(if (<= (term cnt_0) 3) 'true 'false)])

(define-metafunction NAPI
  inv-ref-root : cnt -> b
  [(inv-ref-root cnt_0)
   ,(if (<= (term cnt_0) 3) 'true 'false)])

(define-metafunction NAPI
  inv-async-work : cnt -> b
  [(inv-async-work cnt_0)
   ,(if (<= (term cnt_0) 3) 'true 'false)])

(define-metafunction NAPI
  inv-tsfn : cnt -> b
  [(inv-tsfn cnt_0)
   ,(if (<= (term cnt_0) 3) 'true 'false)])

(define-metafunction NAPI
  inv-teardown-order : phase b -> b
  [(inv-teardown-order phase_0 false) true]
  [(inv-teardown-order phase_0 true) false])

(define-metafunction NAPI
  inv-finalizer : b b -> b
  [(inv-finalizer b_fz b_fq) (finalizer-once? b_fz b_fq)])

(define-metafunction NAPI
  inv-abi-cap : abi -> b
  [(inv-abi-cap abi_0)
   ,(if (and (<= 1 (term abi_0))
             (<= (term abi_0) 9))
        'true
        'false)])

;;; ===========================================================================
;;; Reduction: napi-lifecycle  (explicit Node-API lifecycle)
;;; ===========================================================================
;;;
;;; The lifecycle uses disjoint state regions rather than priority:
;;;   open-scope  : empty table, scope=0, roots=0, not finalized
;;;   make-ref    : empty table, scope=1, roots=0, not finalized
;;;   close-scope : empty table, scope>=2, roots=0, not finalized
;;;   unref       : roots>0
;;;   retire      : roots=0 with a live head slot
;;;   finalize    : roots=0 with a dead head slot and fz=false
;;;   reclaim     : roots=0 with a marked, epoch-safe dead head and fz=true
;;;   teardown    : finalized, empty table, zero work counters, scope>0
;;; Thus every well-formed configuration has at most one successor.

(define napi-lifecycle
  (reduction-relation
   NAPI
   #:domain cfg
   #:codomain cfg

   ;; open-scope: bootstrap the first active scope.
   (--> (napi env_0 () gen_hgen 0 0 cnt_async cnt_tsfn
              active b_fq false abi_0 false)
        (napi env_0 () gen_hgen 1 0 cnt_async cnt_tsfn
              active b_fq false abi_0 false)
        "open-scope")

   ;; make-ref: the first open scope owns one live reference.
   (--> (napi env_0 () gen_hgen 1 0 cnt_async cnt_tsfn
              active b_fq false abi_0 false)
        (napi env_0 ((slot s0 true gen_hgen false)) gen_hgen 1 1
              cnt_async cnt_tsfn active b_fq false abi_0 false)
        "make-ref")

   ;; close-scope: only scopes beyond the initial ownership scope can close
   ;; before finalization; this is disjoint from make-ref's scope=1 region.
   (--> (napi env_0 () gen_hgen cnt_scope 0 cnt_async cnt_tsfn
              active b_fq false abi_0 false)
        (napi env_0 () gen_hgen cnt_scope-next 0 cnt_async cnt_tsfn
              active b_fq false abi_0 false)
        (side-condition (>= (term cnt_scope) 2))
        (where cnt_scope-next ,(sub1 (term cnt_scope)))
        "close-scope")

   ;; unref: any outstanding strong root drops before a slot may retire.
   (--> (napi env_0 tab_0 gen_hgen cnt_scope cnt_roots cnt_async cnt_tsfn
              active b_fq b_fz abi_0 false)
        (napi env_0 tab_0 gen_hgen cnt_scope cnt_roots-next cnt_async cnt_tsfn
              active b_fq b_fz abi_0 false)
        (side-condition (> (term cnt_roots) 0))
        (where cnt_roots-next ,(sub1 (term cnt_roots)))
        "unref")

   ;; retire: roots are gone and a live head slot advances its generation.
   (--> (napi env_0 ((slot sid_0 true gen_0 b_marked) entry_1 ...)
              gen_hgen cnt_scope 0 cnt_async cnt_tsfn active b_fq b_fz abi_0 false)
        (napi env_0 tab_1 gen_hgen cnt_scope 0 cnt_async cnt_tsfn
              active b_fq b_fz abi_0 false)
        (where tab_1
               (retire-slot ((slot sid_0 true gen_0 b_marked) entry_1 ...) sid_0))
        "retire")

   ;; finalize: a dead slot can queue exactly one finalizer completion.
   (--> (napi env_0 ((slot sid_0 false gen_0 b_marked) entry_1 ...)
              gen_hgen cnt_scope 0 cnt_async cnt_tsfn active b_fq false abi_0 false)
        (napi env_0 ((slot sid_0 false gen_0 b_marked) entry_1 ...)
              gen_hgen cnt_scope 0 cnt_async cnt_tsfn active false true abi_0 false)
        "finalize")

   ;; reclaim: only finalized, marked dead slots from an earlier epoch leave
   ;; the table. Advancing hgen preserves generation monotonicity.
   (--> (napi env_0 ((slot sid_0 false gen_0 true) entry_1 ...)
              gen_hgen cnt_scope 0 cnt_async cnt_tsfn active b_fq true abi_0 false)
        (napi env_0 (entry_1 ...) gen_hgen-next cnt_scope 0 cnt_async cnt_tsfn
              active b_fq true abi_0 false)
        (side-condition (< (term gen_0) (term gen_hgen)))
        (where gen_hgen-next ,(add1 (term gen_hgen)))
        "reclaim")

   ;; teardown: no table, work, or roots remain after finalization.
   (--> (napi env_0 () gen_hgen cnt_scope 0 0 0 phase_0 b_fq true abi_0 false)
        (napi env_0 () gen_hgen-next cnt_scope 0 0 0 closed false true abi_0 false)
        (side-condition (and (> (term cnt_scope) 0)
                             (not (equal? (term phase_0) 'closed))))
        (where gen_hgen-next ,(add1 (term gen_hgen)))
        "teardown")))

;;; ===========================================================================
;;; Reduction: callback-capability  (handle-generation callback authority)
;;; ===========================================================================

(define callback-capability
  (reduction-relation
   NAPI
   #:domain capcfg
   #:codomain capcfg

   ;; A callback is granted only to the currently live slot generation.
   (--> (cap ((slot sid_0 true gen_0 b_marked) entry_0 ...)
             (h sid_0 gen_0) active false)
        (cap ((slot sid_0 true gen_0 b_marked) entry_0 ...)
             (h sid_0 gen_0) active true)
        "grant-callback")

   ;; Retiring the owning slot advances its generation and revokes the
   ;; capability carried by the now-stale callback handle.
   (--> (cap ((slot sid_0 true gen_0 b_marked) entry_0 ...)
             (h sid_0 gen_0) active true)
        (cap tab_1 (h sid_0 gen_0) active false)
        (where tab_1
               (retire-slot ((slot sid_0 true gen_0 b_marked) entry_0 ...) sid_0))
        "retire-callback")

   ;; Lifecycle teardown revokes an otherwise live callback before closure.
   (--> (cap tab_0 (h sid_0 gen_hgen) closing true)
        (cap tab_0 (h sid_0 gen_hgen) closed false)
        "teardown-callback")))

;;; ===========================================================================
;;; Tests
;;; ===========================================================================
(module+ test
  (require rackunit)

  ;; --- Deterministic examples: every named lifecycle rule has one result. ---
  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 () 0 0 0 0 0 active false false 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal? (car nexts)
                  (term (napi 0 () 0 1 0 0 0 active false false 1 false))))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 () 0 2 0 0 0 active false false 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal? (car nexts)
                  (term (napi 0 () 0 1 0 0 0 active false false 1 false))))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 () 0 1 0 0 0 active false false 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal?
     (car nexts)
     (term (napi 0 ((slot s0 true 0 false)) 0 1 1 0 0
                 active false false 1 false))))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 ((slot s0 true 0 false)) 0 1 1 0 0
                              active false false 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal?
     (car nexts)
     (term (napi 0 ((slot s0 true 0 false)) 0 1 0 0 0
                 active false false 1 false))))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 ((slot s0 true 0 false)) 0 1 0 0 0
                              active false false 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal?
     (car nexts)
     (term (napi 0 ((slot s0 true 1 false)) 0 1 0 0 0
                 active false false 1 false)))
    (check-equal?
     (term (resolves? ((slot s0 true 1 false)) (h s0 0)))
     (term false))
    (check-equal?
     (term (resolves? ((slot s0 true 1 false)) (h s0 1)))
     (term true)))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 ((slot s0 false 0 false)) 0 1 0 0 0
                              active false false 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal?
     (car nexts)
     (term (napi 0 ((slot s0 false 0 false)) 0 1 0 0 0
                 active false true 1 false))))

  (check-equal?
   (apply-reduction-relation
    napi-lifecycle
    (term (napi 0 ((slot s0 false 0 false)) 0 1 0 0 0
                active false true 1 false)))
   (term ()))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 ((slot s0 false 0 true)) 2 1 0 0 0
                              active false true 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal?
     (car nexts)
     (term (napi 0 () 3 1 0 0 0 active false true 1 false))))

  (let ([nexts (apply-reduction-relation napi-lifecycle
                  (term (napi 0 () 0 1 0 0 0 active false true 1 false)))])
    (check-equal? (length nexts) 1)
    (check-equal?
     (car nexts)
     (term (napi 0 () 1 1 0 0 0 closed false true 1 false))))

  ;; --- Malformed negatives and invariant controls. ---
  (check-equal?
   (term (resolves? ((slot s0 true 1 false)) (h s0 0)))
   (term false))
  (check-equal?
   (term (epoch-safe? ((slot s0 false 0 true)) (h s0 2) 1))
   (term false))
  (check-equal?
   (term (epoch-safe? ((slot s0 true 0 true)) (h s0 2) 0))
   (term false))
  (check-equal? (term (finalizer-once? true true)) (term false))
  (check-equal? (term (finalizer-once? true false)) (term true))

  ;; --- Fixed-seed redex-check: no configuration has two lifecycle steps. ---
  (parameterize ([current-pseudo-random-generator
                  (make-pseudo-random-generator)])
    (random-seed 63017)
    (redex-check
     NAPI
     cfg
     (let ([nexts (apply-reduction-relation napi-lifecycle (term cfg))])
       (<= (length nexts) 1))
     #:attempts 300
     #:source napi-lifecycle))

  ;; --- Fixed-seed redex-check: retire a generated live head and reject the
  ;; handle carrying that head's pre-retire generation.
  (parameterize ([current-pseudo-random-generator
                  (make-pseudo-random-generator)])
    (random-seed 63019)
    (redex-check
     NAPI
     (((slot sid_0 true gen_0 b_0) entry_0 ...) sid_0 gen_0)
     (let* ([source-table
             (term ((slot sid_0 true gen_0 b_0) entry_0 ...))]
            [retired (term (retire-slot ,source-table sid_0))]
            [old-h (term (h sid_0 gen_0))])
       (or (equal? retired source-table)
           (equal? (term (resolves? ,retired ,old-h)) (term false))))
     #:attempts 300))

  ;; --- Coverage assertion: every named napi-lifecycle rule is present and named. ---
  (check-equal?
   (sort (reduction-relation->rule-names napi-lifecycle) symbol<?)
   '(close-scope finalize make-ref open-scope reclaim retire teardown unref))

  ;; --- Observable mutant guard: a retire that does not bump generation
  ;; leaves the old handle resolving and violates this assertion.
  (check-not-equal?
   (term (resolves? (retire-slot ((slot s0 true 0 false)) s0) (h s0 0)))
   (term true))

  (test-case
   "control::napi-reduction.rkt::callback-capability::deterministic-examples"
   (check-equal?
    (apply-reduction-relation
     callback-capability
     (term (cap ((slot s0 true 0 false)) (h s0 0) active false)))
    (list (term (cap ((slot s0 true 0 false)) (h s0 0) active true)))))

  (test-case
   "control::napi-reduction.rkt::callback-capability::fixed-seed-redex-check"
   (parameterize ([current-pseudo-random-generator
                   (make-pseudo-random-generator)])
     (random-seed 63023)
     (redex-check
      NAPI
      (((slot sid_0 true gen_0 b_0) entry_0 ...) sid_0 gen_0)
      (let ([nexts
             (apply-reduction-relation
              callback-capability
              (term (cap ((slot sid_0 true gen_0 b_0) entry_0 ...)
                         (h sid_0 gen_0) active false)))])
        (= (length nexts) 1))
      #:attempts 300)))

  (test-case
   "control::napi-reduction.rkt::callback-capability::named-rule-coverage"
   (define fired
     (append*
      (for/list
          ([source
            (in-list
             (list
              (term (cap ((slot s0 true 0 false)) (h s0 0) active false))
              (term (cap ((slot s0 true 0 false)) (h s0 0) active true))
              (term (cap ((slot s0 true 0 false)) (h s0 0) closing true))))])
        (map (lambda (step) (format "~a" (car step)))
             (apply-reduction-relation/tag-with-names
              callback-capability source)))))
   (check-equal?
    (sort (remove-duplicates fired) string<?)
    '("grant-callback" "retire-callback" "teardown-callback")))

  (test-case
   "control::napi-reduction.rkt::callback-capability::malformed-input-negative"
   (check-equal?
    (term (resolves? ((slot s0 true 1 false)) (h s0 0)))
    (term false))
   (check-equal?
    (apply-reduction-relation
     callback-capability
     (term (cap ((slot s0 true 1 false)) (h s0 0) active false)))
    '()))

  (test-case
   "control::napi-reduction.rkt::callback-capability::observable-mutation"
   (define retired
     (car
      (apply-reduction-relation
       callback-capability
       (term (cap ((slot s0 true 0 false)) (h s0 0) active true)))))
   (check-equal? retired
                 (term (cap ((slot s0 true 1 false)) (h s0 0) active false)))
   (check-not-equal?
    (term (resolves? ((slot s0 true 1 false)) (h s0 0)))
    (term true)))

  (test-case
   "control::napi-reduction.rkt::napi-lifecycle::deterministic-examples"
   (check-equal?
    (apply-reduction-relation
     napi-lifecycle
     (term (napi 0 () 0 1 0 0 0 active false true 1 false)))
    (list (term (napi 0 () 1 1 0 0 0 closed false true 1 false)))))

  (test-case
   "control::napi-reduction.rkt::napi-lifecycle::fixed-seed-redex-check"
   (parameterize ([current-pseudo-random-generator
                   (make-pseudo-random-generator)])
     (random-seed 63029)
     (redex-check
      NAPI
      cfg
      (<= (length (apply-reduction-relation napi-lifecycle (term cfg))) 1)
      #:attempts 300
      #:source napi-lifecycle)))

  (test-case
   "control::napi-reduction.rkt::napi-lifecycle::named-rule-coverage"
   (define fired
     (append*
      (for/list
          ([source
            (in-list
             (list
              (term (napi 0 () 0 0 0 0 0 active false false 1 false))
              (term (napi 0 () 0 2 0 0 0 active false false 1 false))
              (term (napi 0 () 0 1 0 0 0 active false false 1 false))
              (term (napi 0 ((slot s0 true 0 false)) 0 1 1 0 0 active false false 1 false))
              (term (napi 0 ((slot s0 true 0 false)) 0 1 0 0 0 active false false 1 false))
              (term (napi 0 ((slot s0 false 0 false)) 0 1 0 0 0 active false false 1 false))
              (term (napi 0 ((slot s0 false 0 true)) 2 1 0 0 0 active false true 1 false))
              (term (napi 0 () 0 1 0 0 0 active false true 1 false))))])
        (map (lambda (step) (format "~a" (car step)))
             (apply-reduction-relation/tag-with-names napi-lifecycle source)))))
   (check-equal?
    (sort (remove-duplicates fired) string<?)
    '("close-scope" "finalize" "make-ref" "open-scope"
      "reclaim" "retire" "teardown" "unref")))

  (test-case
   "control::napi-reduction.rkt::napi-lifecycle::malformed-input-negative"
   (check-equal?
    (apply-reduction-relation
     napi-lifecycle
     (term (napi 0 () 0 1 0 0 0 closed false false 1 false)))
    '()))

  (test-case
   "control::napi-reduction.rkt::napi-lifecycle::observable-mutation"
   (define finalizer-result
     (apply-reduction-relation
      napi-lifecycle
      (term (napi 0 ((slot s0 false 0 false)) 0 1 0 0 0
                  active false false 1 false))))
   (define teardown-result
     (apply-reduction-relation
      napi-lifecycle
      (term (napi 0 () 0 1 0 0 0 active false true 1 false))))
   (check-equal?
    finalizer-result
    (list
     (term (napi 0 ((slot s0 false 0 false)) 0 1 0 0 0
                 active false true 1 false))))
   (check-equal?
    teardown-result
    (list (term (napi 0 () 1 1 0 0 0 closed false true 1 false))))
   (check-not-equal?
    (car teardown-result)
    (term (napi 0 () 0 1 0 0 0 active false true 1 false))))
)
