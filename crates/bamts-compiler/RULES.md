# BamTS strictness rules

This file is generated from `bamts_compiler::lint::RULES`; do not edit it manually.

## `BAMTS-W001`: `method-parameter-bivariance`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Method parameters are bivariant, so a narrower handler can receive an incompatible value.
- Sound alternative: Use a function-property callback with a contravariant parameter.
- Silence: `-A method-parameter-bivariance`

## `BAMTS-W002`: `mutable-array-covariance`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Mutable arrays are covariant, so a widened alias can write the wrong element type.
- Sound alternative: Expose readonly arrays across type boundaries.
- Silence: `-A mutable-array-covariance`

## `BAMTS-W003`: `non-fresh-excess-property`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A non-fresh object can bypass excess-property checks and hide misspelled fields.
- Sound alternative: Validate the object at its construction boundary.
- Silence: `-A non-fresh-excess-property`

## `BAMTS-W004`: `delete-required-property`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Deleting a required property breaks the declared object shape.
- Sound alternative: Model removability with an optional property or a separate value.
- Silence: `-A delete-required-property`

## `BAMTS-W005`: `unchecked-catch-member`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A catch binding is untrusted until it is narrowed before member access.
- Sound alternative: Narrow the caught value with a runtime guard.
- Silence: `-A unchecked-catch-member`

## `BAMTS-W006`: `generic-any-downcast`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: Casting any through a generic return loses the proof required by every caller.
- Sound alternative: Validate the input and return a concrete checked type.
- Silence: `-A generic-any-downcast`

## `BAMTS-W007`: `dynamic-tuple-index`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A dynamic tuple index can read beyond the tuple's known bounds.
- Sound alternative: Use a literal index or prove the index is in range.
- Silence: `-A dynamic-tuple-index`

## `BAMTS-W008`: `unchecked-index-signature-read`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: An index-signature read can be absent even when its value type excludes undefined.
- Sound alternative: Handle undefined after the lookup.
- Silence: `-A unchecked-index-signature-read`

## `BAMTS-W009`: `explicit-undefined-for-optional`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: An optional property without undefined distinguishes absence from an explicit undefined value.
- Sound alternative: Omit the property or include undefined in its declared type.
- Silence: `-A explicit-undefined-for-optional`

## `BAMTS-W010`: `detached-this-method`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Extracting a receiver-dependent method loses the this binding it requires.
- Sound alternative: Bind the method or call it through its receiver.
- Silence: `-A detached-this-method`

## `BAMTS-W011`: `divergent-accessor-types`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Different getter and setter types hide an unsafe property boundary.
- Sound alternative: Use one compatible property type or an explicit conversion method.
- Silence: `-A divergent-accessor-types`

## `BAMTS-W012`: `readonly-alias-mutation`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A writable alias can mutate data promised as readonly elsewhere.
- Sound alternative: Keep the mutable value private and expose a readonly view.
- Silence: `-A readonly-alias-mutation`

## `BAMTS-W013`: `fewer-callback-parameters`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A callback that accepts fewer parameters can silently discard required protocol data.
- Sound alternative: Declare the callback parameters you intentionally receive.
- Silence: `-A fewer-callback-parameters`

## `BAMTS-W014`: `value-returning-void-callback`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A value returned from a void callback is silently discarded.
- Sound alternative: Use a block body when the return value is intentionally ignored.
- Silence: `-A value-returning-void-callback`

## `BAMTS-W015`: `open-object-keys-assumption`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Object.keys does not prove that runtime keys are limited to keyof T.
- Sound alternative: Validate keys at runtime or work from a closed key list.
- Silence: `-A open-object-keys-assumption`

## `BAMTS-W016`: `index-signature-dot-access`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Dot access through an index signature hides that a property may be absent.
- Sound alternative: Use bracket access and handle the missing value.
- Silence: `-A index-signature-dot-access`

## `BAMTS-W017`: `explicit-any`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: Explicit any disables type checking at the annotated boundary.
- Sound alternative: Use unknown and narrow it before use.
- Silence: `-A explicit-any`

## `BAMTS-W018`: `implicit-any`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: An inferred any lets an untyped value flow without an explicit boundary.
- Sound alternative: Add an explicit checked type or unknown annotation.
- Silence: `-A implicit-any`

## `BAMTS-W019`: `unchecked-type-assertion`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: A type assertion claims a narrower type without runtime proof.
- Sound alternative: Narrow with a guard or validate with a decoder.
- Silence: `-A unchecked-type-assertion`

## `BAMTS-W020`: `double-assertion`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: A double assertion bypasses assignability through any or unknown.
- Sound alternative: Convert or validate the value at the boundary.
- Silence: `-A double-assertion`

## `BAMTS-W021`: `non-null-assertion`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: A non-null assertion erases a possible null or undefined value.
- Sound alternative: Narrow the value before accessing it.
- Silence: `-A non-null-assertion`

## `BAMTS-W022`: `definite-assignment-assertion`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: A definite-assignment assertion skips proof that a field is initialized.
- Sound alternative: Initialize the field or assign it in every constructor path.
- Silence: `-A definite-assignment-assertion`

## `BAMTS-W023`: `diagnostic-suppression-directive`

- Group: `escape-hatches`
- Default level: `warn`
- Rationale: A TypeScript diagnostic directive hides a compiler check instead of resolving it.
- Sound alternative: Fix the diagnostic or make the boundary explicit.
- Silence: `-A diagnostic-suppression-directive`

## `BAMTS-W024`: `runtime-namespace`

- Group: `non-erasable`
- Default level: `warn`
- Rationale: A value-bearing namespace requires runtime code instead of erasing as type syntax.
- Sound alternative: Use ES modules or an ambient namespace.
- Silence: `-A runtime-namespace`

## `BAMTS-W025`: `parameter-property`

- Group: `non-erasable`
- Default level: `warn`
- Rationale: A parameter property synthesizes a field assignment during compilation.
- Sound alternative: Declare the field and assign the constructor parameter explicitly.
- Silence: `-A parameter-property`

## `BAMTS-W026`: `legacy-decorator-semantics`

- Group: `legacy-syntax`
- Default level: `warn`
- Rationale: Legacy decorators have semantics that differ from standard ECMAScript decorators.
- Sound alternative: Use standard decorators or an explicit wrapper.
- Silence: `-A legacy-decorator-semantics`

## `BAMTS-W027`: `angle-bracket-assertion`

- Group: `legacy-syntax`
- Default level: `warn`
- Rationale: Angle-bracket assertions are ambiguous with JSX syntax.
- Sound alternative: Use the `as T` assertion spelling.
- Silence: `-A angle-bracket-assertion`

## `BAMTS-W028`: `declaration-inference-dependency`

- Group: `legacy-syntax`
- Default level: `warn`
- Rationale: Declaration output that depends on cross-file inference is fragile and non-local.
- Sound alternative: Write an explicit exported type annotation.
- Silence: `-A declaration-inference-dependency`

## `BAMTS-W029`: `jsx-transform-required`

- Group: `legacy-syntax`
- Default level: `warn`
- Rationale: JSX requires a configured runtime transform and cannot simply be erased.
- Sound alternative: Configure a JSX runtime or use ordinary function calls.
- Silence: `-A jsx-transform-required`

## `BAMTS-W030`: `import-export-equals`

- Group: `modules`
- Default level: `warn`
- Rationale: TypeScript import-equals and export-equals require target-specific module rewriting.
- Sound alternative: Use standard ESM import and export syntax.
- Silence: `-A import-export-equals`

## `BAMTS-W031`: `type-imported-as-value`

- Group: `modules`
- Default level: `warn`
- Rationale: A type-only import emitted as a value import creates a runtime dependency.
- Sound alternative: Use `import type` for type-only symbols.
- Silence: `-A type-imported-as-value`

## `BAMTS-W032`: `type-reexported-as-value`

- Group: `modules`
- Default level: `warn`
- Rationale: A type-only re-export emitted as a value re-export creates a runtime dependency.
- Sound alternative: Use `export type` for type-only symbols.
- Silence: `-A type-reexported-as-value`

## `BAMTS-W033`: `commonjs-in-esm`

- Group: `modules`
- Default level: `allow`
- Rationale: CommonJS globals inside an ESM module depend on host-specific interop.
- Sound alternative: Use ESM exports or isolate the CommonJS bridge.
- Silence: `-A commonjs-in-esm`

## `BAMTS-W034`: `implicit-script-file`

- Group: `modules`
- Default level: `allow`
- Rationale: A file without imports or exports silently becomes a global script.
- Sound alternative: Add an explicit export or force module detection.
- Silence: `-A implicit-script-file`

## `BAMTS-W035`: `unchecked-side-effect-import`

- Group: `modules`
- Default level: `warn`
- Rationale: An unresolved side-effect import can conceal a missing runtime dependency.
- Sound alternative: Resolve the module or declare the host-provided virtual module.
- Silence: `-A unchecked-side-effect-import`

## `BAMTS-W036`: `extensionless-relative-import`

- Group: `modules`
- Default level: `warn`
- Rationale: Relative ESM imports need a runtime file extension in Node-style resolution.
- Sound alternative: Write the explicit runtime extension.
- Silence: `-A extensionless-relative-import`

## `BAMTS-W037`: `interop-dependent-default-import`

- Group: `modules`
- Default level: `warn`
- Rationale: A default import from CommonJS can rely on synthetic interop semantics.
- Sound alternative: Use a namespace import or a real ESM default export.
- Silence: `-A interop-dependent-default-import`

## `BAMTS-W038`: `virtual-call-in-constructor`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: A constructor dispatching to an overridable method can observe uninitialized derived state.
- Sound alternative: Defer the hook until construction is complete.
- Silence: `-A virtual-call-in-constructor`

## `BAMTS-W039`: `uninitialized-field-emit-split`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: An uninitialized field has different runtime presence under competing emit modes.
- Sound alternative: Initialize it or use `declare` when no own field is intended.
- Silence: `-A uninitialized-field-emit-split`

## `BAMTS-W040`: `field-overrides-accessor`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: A defined field can shadow an inherited accessor instead of invoking it.
- Sound alternative: Use an accessor, `declare`, or a distinct field name.
- Silence: `-A field-overrides-accessor`

## `BAMTS-W041`: `implicit-override`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: An unmarked override can silently drift when its base member changes.
- Sound alternative: Mark the member with `override`.
- Silence: `-A implicit-override`

## `BAMTS-W042`: `typescript-private-field`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: A TypeScript private modifier erases and does not provide runtime privacy.
- Sound alternative: Use an ECMAScript `#private` field for runtime privacy.
- Silence: `-A typescript-private-field`

## `BAMTS-W043`: `runtime-enum`

- Group: `enum-semantics`
- Default level: `warn`
- Rationale: A non-const enum creates a runtime object with non-erasable behavior.
- Sound alternative: Use a union or a const object when a runtime object is intentional.
- Silence: `-A runtime-enum`

## `BAMTS-W044`: `const-enum`

- Group: `enum-semantics`
- Default level: `warn`
- Rationale: A const enum relies on compile-time inlining across compilation boundaries.
- Sound alternative: Use a union or a const object.
- Silence: `-A const-enum`

## `BAMTS-W045`: `numeric-enum-number-flow`

- Group: `enum-semantics`
- Default level: `warn`
- Rationale: Numeric enums accept arbitrary numbers, weakening the enum boundary.
- Sound alternative: Use a string enum or validate the numeric value.
- Silence: `-A numeric-enum-number-flow`

## `BAMTS-W046`: `heterogeneous-enum`

- Group: `enum-semantics`
- Default level: `warn`
- Rationale: A heterogeneous enum mixes unrelated representations and complicates consumers.
- Sound alternative: Use one representation or a discriminated union.
- Silence: `-A heterogeneous-enum`

## `BAMTS-W047`: `computed-enum-member`

- Group: `enum-semantics`
- Default level: `warn`
- Rationale: A computed enum member depends on runtime evaluation rather than a stable constant.
- Sound alternative: Use a constant initializer or a separate runtime value.
- Silence: `-A computed-enum-member`

## `BAMTS-W048`: `numeric-enum-reverse-lookup`

- Group: `enum-semantics`
- Default level: `warn`
- Rationale: Numeric enum reverse lookup depends on generated runtime mappings.
- Sound alternative: Store the display name explicitly.
- Silence: `-A numeric-enum-reverse-lookup`

## `BAMTS-W049`: `interface-declaration-merge`

- Group: `declaration-merging`
- Default level: `warn`
- Rationale: Same-scope interfaces merge implicitly, making a type's shape non-local.
- Sound alternative: Declare one complete interface or use a closed type alias.
- Silence: `-A interface-declaration-merge`

## `BAMTS-W050`: `namespace-value-merge`

- Group: `declaration-merging`
- Default level: `warn`
- Rationale: A namespace merged with a value creates an implicit hybrid declaration.
- Sound alternative: Use an explicit object or separate module export.
- Silence: `-A namespace-value-merge`

## `BAMTS-W051`: `global-augmentation`

- Group: `declaration-merging`
- Default level: `warn`
- Rationale: A global augmentation mutates ambient types for unrelated code.
- Sound alternative: Expose a local wrapper or explicit global installation boundary.
- Silence: `-A global-augmentation`

## `BAMTS-W052`: `module-augmentation`

- Group: `declaration-merging`
- Default level: `warn`
- Rationale: A module augmentation changes another module's contract outside that module.
- Sound alternative: Wrap or extend the module through an explicit local API.
- Silence: `-A module-augmentation`

## `BAMTS-W053`: `ambient-value-declaration`

- Group: `declaration-merging`
- Default level: `warn`
- Rationale: An ambient value declaration cannot prove that the runtime provides the value.
- Sound alternative: Pass the value explicitly or install it through a checked host API.
- Silence: `-A ambient-value-declaration`

## `BAMTS-W054`: `javascript-input`

- Group: `javascript-compatibility`
- Default level: `allow`
- Rationale: JavaScript source enters a typed program with weaker static guarantees.
- Sound alternative: Convert the source to TypeScript or isolate it behind typed declarations.
- Silence: `-A javascript-input`

## `BAMTS-W055`: `jsdoc-type-syntax`

- Group: `javascript-compatibility`
- Default level: `allow`
- Rationale: JSDoc types make JavaScript comments carry part of the type system.
- Sound alternative: Move the file to TypeScript with native type syntax.
- Silence: `-A jsdoc-type-syntax`

## `BAMTS-W056`: `prototype-class-pattern`

- Group: `javascript-compatibility`
- Default level: `allow`
- Rationale: Prototype assignment spreads class behavior across mutable runtime objects.
- Sound alternative: Use class syntax or an explicit factory object.
- Silence: `-A prototype-class-pattern`

## `BAMTS-W057`: `ts-check-directive`

- Group: `javascript-compatibility`
- Default level: `allow`
- Rationale: A per-file ts-check directive makes type-checking policy non-uniform.
- Sound alternative: Use project-wide checkJs or convert the file to TypeScript.
- Silence: `-A ts-check-directive`

## `BAMTS-W058`: `prefer-type-alias`

- Group: `opinionated`
- Default level: `allow`
- Rationale: An interface can merge later, leaving an API shape open unintentionally.
- Sound alternative: Use a type alias for a closed shape.
- Silence: `-A prefer-type-alias`

## `BAMTS-W059`: `prefer-readonly-array`

- Group: `opinionated`
- Default level: `allow`
- Rationale: A mutable array type advertises mutation where a read-only view may suffice.
- Sound alternative: Accept `readonly T[]` unless mutation is required.
- Silence: `-A prefer-readonly-array`

## `BAMTS-W060`: `prefer-function-property`

- Group: `opinionated`
- Default level: `allow`
- Rationale: A method signature keeps bivariant parameter checking.
- Sound alternative: Use a function-property signature for callback members.
- Silence: `-A prefer-function-property`

## `BAMTS-W061`: `no-barrel-star-export`

- Group: `opinionated`
- Default level: `allow`
- Rationale: A wildcard barrel export obscures the package's public dependency surface.
- Sound alternative: Re-export the intended names explicitly.
- Silence: `-A no-barrel-star-export`

## `BAMTS-W062`: `no-default-export`

- Group: `opinionated`
- Default level: `allow`
- Rationale: A default export lets importers rename one public binding arbitrarily.
- Sound alternative: Use a named export.
- Silence: `-A no-default-export`

## `BAMTS-W063`: `exhaustive-discriminated-switch`

- Group: `opinionated`
- Default level: `allow`
- Rationale: A discriminated-union switch omits a reachable variant.
- Sound alternative: Handle every variant and assert never in the default branch.
- Silence: `-A exhaustive-discriminated-switch`

## `BAMTS-W064`: `long-parameter-list`

- Group: `opinionated`
- Default level: `allow`
- Rationale: A long positional parameter list makes calls easy to misorder.
- Sound alternative: Use a parameter object or smaller cohesive functions.
- Silence: `-A long-parameter-list`

## `BAMTS-W065`: `implicit-return-path`

- Group: `control-flow`
- Default level: `warn`
- Rationale: A function can complete without returning the value its signature implies.
- Sound alternative: Return on every reachable path or include undefined in the return type.
- Silence: `-A implicit-return-path`

## `BAMTS-W066`: `switch-fallthrough`

- Group: `control-flow`
- Default level: `warn`
- Rationale: A non-empty switch case falls through without an explicit transfer.
- Sound alternative: Add break, return, throw, or an explicit fallthrough marker.
- Silence: `-A switch-fallthrough`

## `BAMTS-W067`: `unreachable-code`

- Group: `control-flow`
- Default level: `warn`
- Rationale: A statement is unreachable under the program's control flow.
- Sound alternative: Remove it or restructure the surrounding control flow.
- Silence: `-A unreachable-code`

## `BAMTS-W068`: `unused-label`

- Group: `control-flow`
- Default level: `warn`
- Rationale: A label is declared but never targeted, obscuring control flow.
- Sound alternative: Remove the label or add its intended labeled transfer.
- Silence: `-A unused-label`

## `BAMTS-W069`: `unused-local`

- Group: `control-flow`
- Default level: `warn`
- Rationale: A local binding is never read after declaration.
- Sound alternative: Remove it or use it deliberately.
- Silence: `-A unused-local`

## `BAMTS-W070`: `unused-parameter`

- Group: `control-flow`
- Default level: `warn`
- Rationale: A declared parameter is never read by its function.
- Sound alternative: Remove it or name an intentionally unused protocol parameter clearly.
- Silence: `-A unused-parameter`

## `BAMTS-W071`: `invalid-number-formatting-options`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Known number-formatting arguments lie outside the ECMAScript-supported range.
- Sound alternative: Validate or clamp the argument before calling the method.
- Silence: `-A invalid-number-formatting-options`

## `BAMTS-W072`: `unsound-numeric-key-order-assumption`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Integer-like object keys are ordered before other keys, not purely by insertion.
- Sound alternative: Avoid insertion-order dependence or sort the keys explicitly.
- Silence: `-A unsound-numeric-key-order-assumption`

## `BAMTS-W073`: `json-stringify-unserializable-type`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: JSON.stringify can throw for BigInt or return undefined for a top-level value.
- Sound alternative: Validate serializability and handle the undefined result.
- Silence: `-A json-stringify-unserializable-type`

## `BAMTS-W074`: `unchecked-json-parse-any`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: JSON.parse returns untrusted data that is consumed as a trusted type.
- Sound alternative: Parse to unknown and validate with a decoder.
- Silence: `-A unchecked-json-parse-any`

## `BAMTS-W075`: `numeric-array-default-sort`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Comparator-free sort coerces elements to strings rather than numeric order.
- Sound alternative: Pass an explicit numeric or domain comparator.
- Silence: `-A numeric-array-default-sort`

## `BAMTS-W076`: `loose-equality-coercion`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Loose equality can depend on implicit abstract coercion.
- Sound alternative: Use strict equality or an explicit conversion.
- Silence: `-A loose-equality-coercion`

## `BAMTS-W077`: `object-implicit-toprimitive-coercion`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Implicit object-to-primitive conversion can call surprising coercion hooks.
- Sound alternative: Call String, Number, or an explicit conversion method.
- Silence: `-A object-implicit-toprimitive-coercion`

## `BAMTS-W078`: `symbol-template-interpolation-throw`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: Interpolating a symbol directly into a template literal throws.
- Sound alternative: Wrap it with String or use its description.
- Silence: `-A symbol-template-interpolation-throw`

## `BAMTS-W079`: `nan-strict-comparison`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: NaN is never strictly equal to itself, so a direct comparison is ineffective.
- Sound alternative: Use Number.isNaN.
- Silence: `-A nan-strict-comparison`

## `BAMTS-W080`: `unsafe-tostringtag-override`

- Group: `unsoundness`
- Default level: `warn`
- Rationale: A toStringTag override is not a trustworthy runtime brand.
- Sound alternative: Use a string tag and validate the actual value shape.
- Silence: `-A unsafe-tostringtag-override`

## `BAMTS-W081`: `uninitialized-class-field-shadowing`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: An uninitialized derived field defines an own property that shadows an inherited accessor.
- Sound alternative: Use `declare`, initialize deliberately, or rename the field.
- Silence: `-A uninitialized-class-field-shadowing`

## `BAMTS-W082`: `preserve-const-enums-option`

- Group: `non-erasable`
- Default level: `warn`
- Rationale: Preserving const enums retains runtime enum objects while inlining their uses.
- Sound alternative: Disable preserveConstEnums or replace the enum.
- Silence: `-A preserve-const-enums-option`

## `BAMTS-W083`: `emit-decorator-metadata-option`

- Group: `legacy-syntax`
- Default level: `warn`
- Rationale: Emitted decorator metadata couples runtime reflection to compiler type information.
- Sound alternative: Disable metadata emit and provide explicit metadata.
- Silence: `-A emit-decorator-metadata-option`

## `BAMTS-W084`: `legacy-class-field-set-semantics`

- Group: `class-semantics`
- Default level: `allow`
- Rationale: Legacy class-field set semantics invoke inherited setters instead of defining fields.
- Sound alternative: Enable standard define semantics.
- Silence: `-A legacy-class-field-set-semantics`

## `BAMTS-W085`: `javascript-syntax-rejection`

- Group: `javascript-compatibility`
- Default level: `deny`
- Rationale: TypeScript-only syntax in a JavaScript file violates that file's source dialect.
- Sound alternative: Rename the file to TypeScript or remove the type syntax.
- Silence: `-A javascript-syntax-rejection`

## `BAMTS-W086`: `cjs-esm-named-export-mismatch`

- Group: `modules`
- Default level: `warn`
- Rationale: An ESM named import from CommonJS may not exist in its statically detected exports.
- Sound alternative: Use the CommonJS default export or a declared named export.
- Silence: `-A cjs-esm-named-export-mismatch`
