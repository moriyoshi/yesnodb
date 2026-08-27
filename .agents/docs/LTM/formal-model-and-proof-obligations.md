# Formal Model and Proof Obligations

## Summary

`docs/formal-model.md` formulates yesno as a persistent versioned map from keys to subsets of a finite ordinal universe, evaluated structurally over a fiber decomposition. Its review history established both durable propositions and a process rule: finite proof obligations should be transcribed and checked row by row rather than summarized in prose.

## Key Facts

- The domain is a persistent map `D : K -> P(U)` with prefix-closed atomic visibility and zero-copy reads.
- The ordinal universe excludes `u64::MAX`; the top fiber is shorter by one point.
- Fiber decomposition makes container operations representation-independent at the set level.
- Conforming streams provide the canonical lazy representation of a set.
- Reclamation is best stated as stable obligations, not as volatile implementation predicates.
- The planner rewrite system terminates under one weighted-size measure; it does not depend on the mutable cost heuristic.
- The rewrite system is terminating but not confluent. Match-arm order can affect plan cost while preserving semantics.
- Proposition 14 is a time-average statement; steady-state space amplification is a conditional corollary requiring a stationary age distribution.
- Section 14 records the restriction algebra. Restriction pushdown is unconditionally sound, but reassembling arbitrary ordinal segments cannot assume prefix-aligned `Concat`.
- The article has repeatedly benefited from independent review; self-review did not validate it.
- Numbered section identities are checker inputs: new article framing uses subsections or unnumbered headings rather than casually renumbering audited sections.
- Matrix, integer, and view lenses extend the model from one set carrier to affine injections and controlled non-injective folds.

## Details

### Core formulation

Ordinals decompose into a 48-bit prefix and 16-bit low value. Set algebra lifts fiberwise, and streams enumerate only non-empty fibers in ascending prefix order. The top fiber excludes low value `0xFFFF`, which affects fullness and the counting bound only at the extreme ceiling.

Container encodings are surjective but not injective representations of subsets. Correctness is defined on the quotient by decode equality. This formally justifies using the generic kernel as an oracle for every specialized representation pair.

### Rewrite termination

Several earlier termination measures failed on individual rules: range splitting, XOR-to-OR, double negation, and De Morgan factoring. The stable measure is a single weighted expression size with weights chosen so all 32 outcome schemas strictly decrease.

`scripts/check-plan-measure.py` transcribes the source outcomes, names their source lines, and pins a hash of the full `pass_b` region. The hash is conservative: any edit requires a manual re-audit. A green transcription proves the checked schemas, not automatic completeness beyond that audit.

The system is not confluent. For example, range difference can normalize either through bounded complement or through a union of two ranges. Both denote the same set and may have different costs.

### Reclamation model

The durable abstraction has three non-subsuming obligations:

- no pinned root reaches the extent;
- no recoverable superblock reaches it;
- no materialized zero-copy alias survives.

These are obligations over reachability and aliasing, not a claim that current predicates are sufficient by their spelling. This formulation survived changes from version-based proxies to checkpoint-sequence tracking.

### Space model

Threshold-cleaning formulas assume a continuum payload model. The time-average occupancy is scale-invariant under rescaling of the death curve. Turning it into ensemble space amplification requires a stationary age distribution, and write amplification additionally assumes reclaimed capacity services an equal volume of new writes.

Default operation keeps evacuation disabled, so the formulas guide a policy decision rather than describe a running default.

### Restriction algebra

Section 14 preserves the live algebra from the retired split/merge design. Restriction distributes through the expression operators without a correctness predicate; whether to perform that pushdown is purely a cost decision. Fullness is a different abstraction from occupancy, so refining an occupancy threshold cannot prove that a window is full.

The decomposition theorem separates three claims. Disjoint decomposition and ordered contributions within a fiber hold for arbitrary ordinal cuts, but prefix ordering holds only when cuts lie between chunks. An evaluator that cuts at range or run boundaries must combine adjacent pieces of the same chunk with one scratch container; feeding them separately to `Concat` silently violates stream order.

### Review history

Independent reviews repeatedly found:

- correct numbers with false rationales;
- correct claims supported by evidence narrower than the claim;
- duplicate claims corrected in one location only;
- prose made stale by source changes elsewhere;
- a theorem carrying an unnecessary dependency on a volatile cost function.

The durable editorial rule is to state each claim once beside its proposition and point to it elsewhere. A passage must also earn its space by changing what the reader understands or does; correctness alone does not make a paragraph useful.

### Article structure and packed carriers

The article now leads with an abstract, contributions ledger, combination claim, and conclusion. Its numerical claims remain tied to named numbered sections by `check-model-constants.py`, so renumbering is a compatibility change for the checker and for dozens of external references. A subsection added inside an audited section can also duplicate a role phrase and make an exact-once check fail.

Section 15 generalizes an ordinal set through affine layout maps. Matrix and integer layouts are injective reinterpretations. Packed views add the non-injective case: `Any`, `All`, and `Parity` are exact for union, intersection, and symmetric difference respectively, while inverse-image expansion preserves the whole Boolean signature. In categorical terms, existential fold, inverse image, and universal fold form an adjoint triple.

The ternary-lens survey supplied a useful negative result. Thirteen candidate consumers mostly already had stronger incumbent representations or required block-local density the proposed global encoding could not promise. Demand is part of an abstraction proof: a mathematically valid carrier does not belong in production without a consumer whose own units it improves.

## Files

- `docs/formal-model.md` - the maintained mathematical article, including the restriction algebra in §14.
- `scripts/check-plan-measure.py` - rewrite-measure checker.
- `scripts/check-model-constants.py` - source-derived numeric drift guard.
- `yesno-core/tests/stream_conformance.rs` - stream canonicity layer.

## Test Coverage

The article maps executable propositions to specific test layers in its testing section. Calculus and counting propositions are checked by derivation scripts and source-linked constants rather than by pretending an execution test can falsify their stated model. Propositions 18, 19, 21, and Proposition 20 clause 2 currently describe behavior the implementation does not perform, so they deliberately have no claimed execution-test guard.

## Pitfalls

- Do not describe an empirical constant as the solution of an equation unless the equation actually has that solution.
- Do not use a mutable, budget-dependent heuristic as a mathematical measure of an expression.
- When a named proposition changes, grep and inspect every reference to it.
- Do not renumber an audited section without updating every checker site and prose reference.
- Do not generalize restriction homomorphism laws to folds; their exactness is operator-specific.
- Do not use `Concat` across arbitrary ordinal cuts; only chunk-aligned cuts establish its prefix-order precondition.
- The article should not be called independently validated while each new review still produces upheld findings.
