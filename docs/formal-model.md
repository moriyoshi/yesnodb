---
title: "An Algebraic and Operational Model of a Persistent Roaring-Style Set Store"
subtitle: "Representation, Lazy Boolean Evaluation, and Zero-Copy Persistence"
author: "The yesno project"
date: 2026-09-13
lang: en
keywords:
  - compressed bitmaps
  - set algebra
  - query optimization
  - multi-version concurrency control
  - persistent storage
abstract: |
  This article develops an algebraic and operational model for `yesno`, a
  persistent store that maps 64-bit keys to subsets of a finite ordinal
  universe. Queries are Boolean expressions over stored sets and may request
  either a materialised result or its cardinality. The model separates three
  concerns: a fiberwise decomposition compatible with portable Roaring
  serialization, lazy ordered evaluation with cost-directed rewriting, and
  versioned persistence in which materialised containers may alias a mapped
  file.

  The principal results are exact set-algebra identities, conditional
  complexity bounds, a termination argument for the rewrite system, and three
  independent safety obligations for reclaiming storage visible through
  zero-copy reads. Additional analyses derive representation crossovers and
  quantify space amplification under explicit workload assumptions. The
  mathematical arguments are accompanied by artifact evidence: differential,
  property, concurrency, recovery, allocation, and model-consistency checks.
  These checks validate an implementation of the model but do not constitute a
  mechanized proof. A serialization defect discovered while making the format
  boundary explicit has since been corrected and is retained as a case study in
  history-sensitive validation.
header-includes: |
  \providecommand{\llbracket}{[\![}
  \providecommand{\rrbracket}{]\!]}
---

# 1. Introduction

`yesno` is a persistent, versioned, embedded store for the map

$$
D \;:\; \mathbb{K} \longrightarrow \mathcal{P}(\mathbb{U}),
\qquad \mathbb{K} = \{0,\dots,2^{64}-1\},
$$

where $\mathbb{U}$ is a finite universe of *ordinals* ( defined in §2 ) and
$\mathcal{P}(\mathbb{U})$ is its power set. A key $k$ is a term; its image
$D(k)$ is a posting list; the intended query is not "give me $D(k)$" but

$$
\Big|\; \varphi\big(D(k_1),\dots,D(k_m)\big) \;\Big|
\qquad\text{or}\qquad
\varphi\big(D(k_1),\dots,D(k_m)\big),
$$

for $\varphi$ an arbitrary term in the Boolean algebra
$\big(\mathcal{P}(\mathbb{U}),\cap,\cup,\triangle,\setminus,\complement\big)$.

Three requirements motivate the model:

**(R1) Structural, not extensional, cost.** The cost of evaluating $\varphi$ must
scale with the *structure* of the operands — how many chunks they occupy, how
much they overlap — and not with $\sum_i |D(k_i)|$. A cardinality query must not
pay for materialising a result it then discards.

**(R2) Durability and atomic multi-key visibility.** Updates form a sequence of
transactions; a reader must observe a *prefix-closed* set of them, all-or-nothing
per transaction, and must keep observing exactly that set for its lifetime, even
across crashes, checkpoints, and reclamation of the space its data occupied.

**(R3) Zero-copy reads.** A materialised container must be a *slice* of a
memory-mapped file rather than a copy of it — which turns "do not overwrite live
data" from a preference into a memory-safety obligation.

The following sections show how the design addresses (R1)–(R3). Some conclusions
are mathematical consequences of the stated model; others are engineering
choices evaluated under explicit assumptions. The distinction is maintained
throughout.

| requirement | discharged by |
|---|---|
| **(R1)** structural cost | §3 ( the decomposition that makes cost structural ), §4.2 ( cardinality without materialising ), §4.3 ( intersection at $O(m\log(n'/m))$ ), §5 ( streams that yield without collecting ), §6 ( rewriting on structure ), §10 |
| **(R2)** durability, atomic visibility | §8.1 ( commit versions and the visible watermark $W$ ), §8.2 ( redo-only recovery as a corollary of the buffer policy ), §8.3 ( retention across checkpoint and reclamation ) |
| **(R3)** zero-copy reads | §8.3 ( aliasing turns immutability into a memory-safety obligation ), §4.4 ( byte identity, which is what makes $O(\sigma)$ import legitimate ) |

Table: Where each of the three requirements is discharged.

What this article does **not** formulate, deliberately: sharding ( the virtual-shard
layer is a scaling strategy, not something (R1)–(R3) force ), replication, and the
Arrow and DataFusion boundaries, which consume the model of §4 and §5 rather than
shaping it. §12.8 places the system against comparable ones; the scope here is the
single-node map $D$ and the three requirements above.

Three later layers — an ordinal set read as a stack of boolean matrices, the
same set read as a series of arbitrary-precision integers, and one set read as
several sets sharing an ordinal space — are consumers in the same sense and would
belong in that list, except that they turned out to instantiate four of the
statements below at carriers those statements were not written for, and, in the
third case, to contain two of them as special cases of a single statement. They
are treated in §15 for that reason rather than for their own sake, and §15.7 is
explicit about how narrow the resulting claim is.

The engineering constants are not tuning knobs, though they are not all of one kind either, and
the difference is worth stating at the outset:

| constant | what it is |
|---|---|
| $4096$ | the serialized-size crossover $2m = 8192$ and the portable-format boundary between array and bitmap containers |
| $3584$ | a demotion point whose gap satisfies $8192/(g+2) < 16$ bytes per mutation. Not the largest: $g > 510$ admits $3585$ too, and $512$ is chosen as the power of two |
| $7/8$ | a threshold chosen so that re-encoding at rest cannot two-cycle and is bounded at $\approx 62$ switches |
| $2032$ | a capacity choice with deliberate slack — see §4, where the tight bound is $2047$ |
| $0.40$ | a **deliberate deviation** from the computed optimum $0.285$, justified in §9 by the shape of the cost curve |
| $256$ | a bounded sketch capacity that trades planning cost against precision ( §7 ) |

Table: The engineering constants, and what kind of thing each one is.

Each constant therefore has either a derivation, an inequality, or an explicit
engineering rationale. Only the first is an exact crossover equation.

## 1.1 Contributions and scope

The representation is inherited from Roaring; the article's contribution is a
system-specific synthesis of representation, evaluation, and persistence rather
than a new bitmap encoding. The claims are grouped by evidentiary status below.

| contribution | where | status |
|---|---|---|
| Fiberwise Boolean algebra and representation-independent kernels | §3–§5 | exact results over the stated set model |
| Separate yield and cardinality costs, plus a terminating semantics-preserving rewrite system | §5–§6 | exact identities and a source-audited termination argument; cost conclusions are model-dependent |
| Sound occupancy abstraction and bounded sketch acquisition | §7 | exact soundness result; estimator quality depends on a probabilistic hashing model |
| Prefix-closed visibility, redo-only recovery, and three reclamation obligations | §8 | conditional safety results under the stated publication, checkpoint, and aliasing premises |
| Space amplification and density-gap calculations | §9, §13 | analytic results under explicit workload and encoding assumptions |
| Restriction, layout, and reduction algebras | §14–§15 | set-theoretic results; only part of this algebra is exercised by the implementation |
| Validation of the implementation artifact | §11 | differential, property, concurrency, recovery, resource, and consistency checks |

Table: Contributions classified by the evidence that supports them.

Three negative results are particularly useful. Proposition 3 rules out an
eager unbounded complement under a representation-independent lower bound.
Section 12.3 identifies circumstances in which planning metadata can improve on
adaptive execution, while explicitly limiting the claim to its access model.
Section 11 explains why extensionally equivalent implementations sometimes
require resource or interaction observables rather than output-only oracles.

## 1.2 Method and evidentiary status

The article uses three forms of evidence. First, definitions, propositions, and
proofs establish claims inside an abstract set and storage model. Second,
closed-form calculations and enumerations evaluate conditional models whose
hypotheses are stated with the result. Third, an executable artifact is checked
with independent semantic oracles, byte-level interoperability tests,
history-sensitive concurrency and recovery tests, and scripts that compare
documented constants with the implementation.

These forms are complementary and not interchangeable. A passing artifact test
does not prove a proposition for all inputs; a set-theoretic proof does not show
that the implementation satisfies its premises; and a model calculation does
not establish that its workload distribution holds in deployment. The rewrite
termination audit additionally depends on a manually reviewed transcription,
although source drift invalidates the audit automatically. No result in this
article has been checked by a proof assistant, and the related-work review is not
an exhaustive basis for priority or novelty claims.

Implementation observations are reported as observations about the evaluated
artifact, not as timeless properties of the abstract model. Historical defects
are stated in the past tense and paired with their present validation obligation.

## 1.3 Relation to existing systems

The constituent mechanisms have established precedents. Roaring libraries
provide the serialized representation and in-memory kernels [1–3]. Lucene and
Tantivy provide lazy Boolean evaluation over ordered posting iterators. LMDB
combines a shadow-paged copy-on-write tree, memory mapping, and multi-version
readers. Pilosa demonstrated direct references into mapped bitmap storage, and
FeatureBase's RBF combines Roaring containers with a transactional tree and a
write-ahead log. Section 12 gives the detailed comparisons.

The artifact studied here combines portable Roaring byte identity, mapped
zero-copy reads, MVCC, and algebraic rewriting of lazy Boolean expressions.
Section 12.8 compares this combination with the closest systems considered; the
review is not exhaustive and makes no uniqueness claim.

The term *cost-planned* refers specifically to algebraic rewriting before
operands are opened. Iterator systems such as Lucene also expose costs, but use
them primarily for local execution decisions after the query structure is
established. Likewise, immutable-segment analytical engines avoid much of the
reclamation problem in §8.3 by construction rather than by applying the mutable
storage protocol analysed here.

## Notation

| Symbol | Meaning |
|---|---|
| $\mathbb{U}$ | the ordinal universe, $\{0,\dots,2^{64}-2\}$ |
| $\mathbb{P}$ | the chunk-address space, $\{0,\dots,2^{48}-1\}$ |
| $\mathbb{L}$ | the within-chunk slot space, $\{0,\dots,2^{16}-1\}$ |
| $S_p$ | the **fiber** of $S$ at prefix $p$, a subset of $\mathbb{L}$; possibly empty |
| | a **chunk** is the stored $(p, S_p)$ pair, which exists only for $p \in \operatorname{supp}(S)$; a **container** is an encoding of a fiber. The three are distinct on purpose — Prop 6' forbids emitting an empty fiber, §13.1 has chunks with no extent, and Prop 4' varies the container while the fiber is fixed |
| $\operatorname{supp}(S)$ | $\{p \in \mathbb{P} : S_p \neq \emptyset\}$ |
| $\sigma(S)$ | $\lvert\operatorname{supp}(S)\rvert$, the chunk count |
| $r(A)$ | the number of maximal intervals of $A \subseteq \mathbb{L}$ |
| $W$ | the visible commit-version watermark |

Table: Notation used throughout.

---

# 2. Finite ordinal universe

The universe is one ordinal smaller than the machine word can address, and the
reason is not fastidiousness. A universe of exactly $2^{64}$ points has a subset
whose cardinality no `u64` can hold, so either the count or the complement stops
being representable. Giving up one point buys both back. This section proves
that, and then pays for it: the decomposition of §3 inherits a single fiber that
is one ordinal short, which silently disables every rule keyed on fullness for
that one chunk.

$$
\mathbb{U} \;=\; \{\,0,1,\dots,2^{64}-2\,\},
\qquad |\mathbb{U}| \;=\; 2^{64}-1 .
$$

The value $2^{64}-1$ is deliberately excluded ( invariant **I8** ). The reason is
a counting argument, not a taste:

**Proposition 1 ( representability of cardinality and complement ).**
Let $\mathbb{U}_{\text{full}} = \{0,\dots,2^{64}-1\}$. Then
$\mathcal{P}(\mathbb{U}_{\text{full}})$ contains an element of cardinality
$2^{64}$, which is not representable in a machine word of $64$ bits. Removing one
point gives $|\mathbb{U}| = 2^{64}-1 = \texttt{u64::MAX}$, so

$$
\forall S \subseteq \mathbb{U}:\quad
|S| \le 2^{64}-1
\quad\text{and}\quad
|\mathbb{U} \setminus S| \;=\; (2^{64}-1) - |S|
$$

are both machine-representable, and the half-open interval
$[0,\;2^{64}-1)$ names the whole universe without ever needing the
unrepresentable bound $2^{64}$. $\square$

The cost of Proposition 1 is stated honestly in the code: a foreign CRoaring
64-bit file containing $2^{64}-1$ is *rejected*, not silently truncated, because
truncation would break the round-trip identity of §4.4.

**A second cost, which §3 must inherit: the decomposition is ragged at the top.**
Removing one point from a universe of $2^{64}$ makes exactly one of the $2^{48}$
fibers short by exactly one ordinal. The top fiber
$\mathbb{L}_{2^{48}-1}$ spans $[2^{64}-2^{16},\,2^{64}-2]$ and holds $65\,535$
elements where every other fiber holds $65\,536$. Theorem 2 is unaffected — the
product decomposition needs a partition, not a uniform one — but three things
downstream are:

- the top fiber **can never be full**, since fullness is a comparison against
  $2^{16}$ and only $2^{16}-1$ ordinals exist there. Every rule keyed on `Full`
  ( §7's classification, the absorption of §6.2 ) is therefore silently
  inapplicable to that one chunk;
- a bitmap encoding of the top fiber carries one permanently unsettable bit, so
  $\lvert A\rvert = 2^{16}$ is unreachable and the promotion rule of §4.1 is
  never exercised at its own boundary there;
- §13's counting bound is stated for $n = 2^{16}$ and is off by the corresponding
  amount in that fiber.

§3 records the asymmetry where it arises — $\phi$'s image omits exactly the point
$(2^{48}-1,\,2^{16}-1)$, and Theorem 2's $\mathbb{L}_p$ is fiber-dependent for
that reason — and the implementation records it too, `ChunkProfile::of_range`
saturating at the top prefix with the note that *"that chunk cannot be full
anyway"*. What neither draws are the three consequences above, which is why they
are drawn here: none of it is reachable without data within $2^{16}$ of the top
of the address space, and a decomposition that is uniform *almost* everywhere is
exactly the premise later reasoning helps itself to.

---

# 3. Fiberwise chunk decomposition

Requirement (R1) asks that the cost of a Boolean operation follow the structure
of its operands rather than their size. Nothing about sets guarantees that; it
has to be bought, and this section is where it is bought. Splitting an ordinal at
its sixteenth bit factors the whole Boolean algebra into a product of small
independent ones — so an operation becomes a merge join over two sorted supports,
a prefix present in only one operand costs nothing at all for $\cap$, and each
fiber may be represented, logged and paged on its own. Everything downstream is
a consequence of that one factorisation, including, in §3.1, the one operation it
cannot make cheap.

Define $\phi(x) = (\lfloor x/2^{16}\rfloor,\; x \bmod 2^{16})$, an injection
$\mathbb{U} \hookrightarrow \mathbb{P} \times \mathbb{L}$ whose image is
$(\mathbb{P}\times\mathbb{L}) \setminus \{(2^{48}-1,\,2^{16}-1)\}$. For
$S \subseteq \mathbb{U}$ write $S_p = \{\,\ell : \phi^{-1}(p,\ell) \in S\,\}$.

**Theorem 2 ( fiberwise homomorphism ).** The map
$S \mapsto (S_p)_{p\in\mathbb{P}}$ is an isomorphism of Boolean algebras

$$
\mathcal{P}(\mathbb{U}) \;\cong\; \prod_{p \in \mathbb{P}} \mathcal{P}(\mathbb{L}_p),
\qquad \mathbb{L}_p = \{\ell : \phi^{-1}(p,\ell) \in \mathbb{U}\},
$$

and in particular, for every $\otimes \in \{\cap,\cup,\triangle,\setminus\}$,

$$
(S \otimes T)_p \;=\; S_p \otimes T_p \qquad \text{for all } p .
$$

*Proof.* Membership is decided pointwise and $\phi$ is a bijection onto its
image; each operation is defined pointwise on membership. $\square$

Theorem 2 is the entire architecture in one line. It says:

1. **Set algebra is a merge join** over the two sorted supports, because
   $\operatorname{supp}(S \cap T) \subseteq \operatorname{supp}(S) \cap
   \operatorname{supp}(T)$ and
   $\operatorname{supp}(S \cup T) = \operatorname{supp}(S) \cup
   \operatorname{supp}(T)$. A prefix present in only one operand needs no
   payload work for $\cap$ at all.
2. **Each fiber may be represented independently.** Nothing in Theorem 2
   constrains *how* $S_p \subseteq \mathbb{L}$ is encoded, which is what licenses
   the three encodings of §4.
3. **The unit of persistence can be the fiber.** A chunk is separately
   addressable, separately loggable, and separately pageable, because the algebra
   never needs two fibers at once.

$|\mathbb{L}| = 2^{16}$ is fixed by two independent constraints meeting: a slot
index must fit a `u16`, and the dense encoding of a fiber is then
$2^{16}/8 = 8192$ bytes, which is L1-resident. It is not a parameter to sweep.

**That last sentence was an assertion when it was first written, and is now a
measurement.** The width was swept — payload plus index floor, in bytes per
ordinal, for scattered corpora at chunk widths of $16$, $20$ and $24$ bits — and
the shape of the answer is that a wider chunk wins **only** in the sparse regime
and only by amortising the index. At the canonical sparse case it is about $15\%$
better, entirely because the per-chunk index cost falls from $43\%$ of stored
bytes to nothing; an order of magnitude denser it is about $18\%$ *worse*; and
beyond that the three widths agree to within a percent. So the one thing width
buys is index amortisation in the sparse regime — which a per-key blob format,
using a bit the on-disk chunk reference already reserves, attacks at more than
twice the effect while changing no constant, no kernel and no size-class ladder.

Four independent constraints then close it, and it is worth listing them because
each binds on its own. The on-disk chunk reference spends sixteen bits on a
cardinality field with four bits held in reserve, so a $20$-bit width consumes
exactly the reserve and a $24$-bit width does not fit the reference word at all.
The $8192$-byte bitmap is sized to L1: at $20$ bits a bitmap-against-bitmap
intersection touches a few hundred KiB per chunk pair and at $24$ bits several
MiB, which converts an L1-bound kernel into a DRAM-bound one. A $16$-bit slot
index is the Roaring specification, so any wider chunk forfeits the byte identity
of §4.4 and with it the $O(\sigma)$ import that identity licenses. And
`ARRAY_MAX`, the bitmap size and the run-interval cap are all derived from the
chunk cardinality, so moving it moves the whole ladder of §4 at once.

## 3.1 Why there is no eager complement

Sparsity is an assumption about $\sigma(S) = |\operatorname{supp}(S)|$, and
complement destroys it.

**Proposition 3 ( complement is dense, always ).** For any $S \subseteq
\mathbb{U}$,

$$
\sigma(\mathbb{U}\setminus S) \;\ge\; 2^{48} - \sigma(S),
$$

with equality iff every fiber of $S$ is full.

The set-theoretic statement above is unconditional. The algorithmic corollary is
not, and needs its access model stated: assume an algorithm that **produces the
complement fiber by fiber** and has no oracle for aggregate facts about $S$.
Under that model it performs at least

$$
\max\big(\sigma(S),\; 2^{48}-\sigma(S)\big) \;\ge\; 2^{47}
$$

chunk steps, *independently of $|S|$ and of $|\mathbb{U}\setminus S|$*.

The model is doing real work and dropping it makes the corollary false. An
implementation holding $|S|$ as a cached field decides $\mathbb{U}\setminus S =
\emptyset$ in $O(1)$ by comparing against $|\mathbb{U}|$, and answers
$|\mathbb{U}\setminus S|$ in $O(1)$ always — which is exactly what §4.2's
identities exploit and why §6 rewrites $\neg$ rather than executing it. What is
ruled out is *materialisation*, not every question about the complement.

*Proof.* If $p \notin \operatorname{supp}(S)$ then $(\mathbb{U}\setminus S)_p =
\mathbb{L}_p \neq \emptyset$, giving the bound on the output support. The
algorithm must at minimum emit its output ( $\ge 2^{48}-\sigma(S)$ steps ) and
read its input ( $\ge \sigma(S)$ steps ); the max of the two is minimised at
$\sigma(S)=2^{47}$. $\square$

So an eager `not()` is quadratically wrong in the useful direction: it is
expensive *even when its answer is tiny*. The crate therefore offers complement
only in two forms whose cost is honest:

- **bounded**: $\complement_{[lo,hi)} S = [lo,hi)\setminus S$, whose cost is
  visible in its arguments;
- **lazy**: a stream that produces the $n$-th output chunk on demand and, for
  cardinality, does not produce them at all — see Proposition 6.

---

# 4. Fiber encodings and representation policy

Fix $A \subseteq \mathbb{L}$, $|\mathbb{L}| = 2^{16}$. Let

$$
r(A) \;=\; \big|\{\, x \in A : x-1 \notin A \,\}\big|
$$

be the number of maximal intervals of $A$. Three encodings are used, with exact
serialized sizes in bytes:

$$
b_{\mathrm{arr}}(A) = 2|A|,
\qquad
b_{\mathrm{bmp}}(A) = 8192,
\qquad
b_{\mathrm{run}}(A) = 2 + 4\,r(A).
$$

These are not approximations: they are the portable Roaring format's payload
sizes [47], and byte-identity with that format is a load-bearing property ( §4.4 ).

**Proposition 4 ( payload-size crossovers ).**

$$
b_{\mathrm{arr}}(A) \le b_{\mathrm{bmp}}(A) \iff |A| \le 4096,
\qquad
b_{\mathrm{run}}(A) < b_{\mathrm{bmp}}(A) \iff r(A) \le 2047 .
$$

*Proof.* $2|A| \le 8192 \iff |A| \le 4096$; $2+4r < 8192 \iff r < 2047.5$.
$\square$

The value $\texttt{ARRAY\_MAX} = 4096$ is both the portable-format boundary and
the unique cardinality at which these two payload formulas are equal. The
equation does not prove that every implementation must switch at that point: a
system optimizing execution time could choose an earlier transition.
Byte-compatible serialization, however, must emit the kind prescribed by the
portable format. At the crossover, promotion
$\mathrm{Array}(4096) \to \mathrm{Bitmap}$ is a same-size rewrite, so it needs no
migration between on-disk size classes.

$\texttt{RUN\_MAX\_INTERVALS} = 2032$ keeps an emitted run container inside a
bitmap's footprint: $2 + 4\cdot 2032 = 8130 \le 8192$. It is a **capacity choice
with deliberate slack, not a tight bound** — the largest $r$ satisfying
$2 + 4r \le 8192$ is $2047$, and the largest that still fits the top size class
( 8256 bytes, payload $\le 8248$ ) is larger still. Decoding accepts $r \le 32768$ — the
information-theoretic maximum, since $r(A) \le \lceil |\mathbb{L}|/2 \rceil$ —
so that foreign files remain readable. The asymmetry between the write bound and
the read bound is deliberate and is the standard robustness principle.

## 4.1 The encoding rule is a hysteresis operator, not an argmin

The naive rule $\mathrm{enc}(A) = \arg\min_e b_e(A)$ is *stateless* and therefore
unstable: a sequence of updates that oscillates across a crossover point pays a
full re-encoding on every step. Formally, let $A^{(0)}, A^{(1)}, \dots$ be the
states of a fiber under single-element mutations. The argmin rule can incur
$\Theta(8192)$ bytes of conversion work per mutation — take $|A^{(t)}|$
alternating between $4096$ and $4097$.

The implemented rule makes the encoding a function of the *pair* (state, current
encoding), i.e. an automaton with a deadband:

$$
\text{promote array} \to \text{bitmap when } |A| > 4096,
\qquad
\text{demote bitmap} \to \text{array when } |A| < 3584 .
$$

**Proposition 5 ( amortised conversion cost ).** Between any promotion and the
next demotion, at least $4097-3583 = 514$ mutations occur, and likewise between
a demotion and the next promotion. Hence conversion work is at most
$8192/514 < 16$ bytes per mutation, amortised. $\square$

**What Proposition 5 proves, and what it does not.** It is an *amortised* bound:
conversion work per mutation, averaged over any run between conversions. The
sharper statement available for a switching problem of this shape is a
**competitive ratio** against the offline optimum that knows the whole mutation
sequence [42]. The two are different claims — cost along the realised trajectory
versus worst case against the best possible encoder — and the first does not
imply the second. No competitive ratio is established here.

The gap is asymmetric — $512$ below the promotion point, $0$ above it — because
the two errors are not symmetric. Holding a bitmap that could be an array wastes
bounded space; holding an array that should be a bitmap violates
$|A| \le \texttt{ARRAY\_MAX}$ and is not representable at all.

A second hysteresis governs *re-encoding at rest* ( `optimize` ): switch only on
a $\ge 12.5\%$ saving,

$$
8\,b_{\mathrm{new}} \;\le\; 7\,b_{\mathrm{old}} .
$$

**Proposition 5' ( no two-cycle at fixed content ).** With content unchanged,
$e_1 \to e_2 \to e_1$ would require $8b_2 \le 7b_1$ and $8b_1 \le 7b_2$, hence
$64 b_1 \le 49 b_1$, impossible for $b_1 > 0$. Moreover every accepted switch
multiplies the size by at most $7/8$, so at most
$\log_{8/7}(8192/2) \approx 62$ switches can occur without an intervening
enlargement. $\square$

## 4.2 Cardinality is a valuation, and that is worth four kernels

Cardinality is a *modular valuation* on the lattice $\mathcal{P}(\mathbb{U})$:

$$
|A \cup B| + |A \cap B| \;=\; |A| + |B| .
$$

**Proposition 6 ( three numbers determine the Venn vector ).** The four disjoint
regions $|A\setminus B|,\,|A\cap B|,\,|B\setminus A|,\,|\complement(A\cup B)|$
have three degrees of freedom given $|\mathbb{U}|$, and the triple
$\big(|A|,|B|,|A\cap B|\big)$ fixes them:

$$
\begin{aligned}
|A \cap B|      &= c, &
|A \cup B|      &= |A| + |B| - c, \\
|A \triangle B| &= |A| + |B| - 2c, &
|A \setminus B| &= |A| - c .
\end{aligned}
$$

$\square$

Two engineering consequences, both non-obvious until the identity is written
down:

- Only **one** non-materialising kernel family is needed —
  $\texttt{and\_cardinality}$ — to answer all four cardinality queries with zero
  allocation.
- $|A|$ must be $O(1)$ on **every** encoding, or the identities buy nothing.
  This is why a bitmap fiber maintains its population count incrementally rather
  than recomputing it, and why the on-disk chunk reference stores
  $\texttt{card\_m1} = |A| - 1$ so that $|D(k)| = \sum_p (\texttt{card\_m1}_p+1)$
  is answerable from an index scan **without decoding a single payload**.

For the complement the analogous identity is what makes the lazy operator cheap:

$$
\big|\,[lo,hi) \setminus S\,\big| \;=\; (hi-lo) \;-\; \big|\,S \cap [lo,hi)\,\big| ,
$$

which is driven by $\operatorname{supp}(S)$ — cost $O(\sigma(S))$ — rather than
by the $2^{48}$ chunks of the range. Compare Proposition 3: the same quantity,
computed by a different recurrence, drops from $2^{47}$ steps to $\sigma(S)$.

## 4.3 Intersection cost: the smaller operand should win

Let $S,T$ have supports of sizes $m \le n$, both sorted. A galloping ( exponential
+ binary ) merge computes $\operatorname{supp}(S)\cap\operatorname{supp}(T)$ in

$$
O\!\left(m \log\!\Big(1 + \frac{n}{m}\Big)\right)
\qquad\text{equivalently}\qquad
O\!\left(m + m\log \frac{n}{m}\right)
$$

comparisons. The additive $m$ is not cosmetic: at $m = n$ the second factor
vanishes while the work does not, and dropping it makes the bound read as $0$.

**On optimality, stated more carefully than an earlier draft did.** The
counting argument that distinguishes the $\binom{m+n}{m}$ interleavings bounds
*merging*, and merging is a harder problem than intersection — an intersection
algorithm need not determine the relative order of elements it will not output.
That bound therefore does not establish optimality here. What is true is the
weaker and sufficient statement: the galloping cost matches the known lower
bound for **searching** $m$ items into a sorted sequence of $n$, so no
comparison-based method improves on it by more than a constant, and the $1$-chunk
against $15\,000$-chunk case costs $O(\log n)$ rather than $O(n)$. That is the
formal content of requirement (R1). The measured $165\,\mathrm{ns}$ for a $1{:}100{,}000$ sparse intersection
is this bound, not a constant-factor win.

Within a fiber, the same ratio test governs the choice of algorithm: array
against array gallops when $n/m \ge \texttt{GALLOP\_RATIO} = 32$ and merges
otherwise. **That threshold is not derivable from the two asymptotic costs, and
this article should not imply it is.** A linear merge takes $m+n$ steps and
galloping $m\log_2(1+n/m)$; setting them equal gives $1 + r = \log_2(1+r)$ for
$r = n/m$, which has no solution for $r \ge 0$ — on *comparison count* galloping
is never worse at any ratio, once both are written with their additive terms. What differs is the cost of a step: a merge step is
sequential, branch-predictable and cache-resident, where a gallop step is a
random probe with a mispredicted branch. $32$ is a chosen operating point in that
neighbourhood, not a crossover derived here: no local ratio sweep in this article
establishes where the measured constants cross on any particular machine.

## 4.4 Format identity as an algebraic property

Let $\mathrm{enc}$ be `yesno`'s payload encoder and $\mathrm{enc}^{\mathrm{R}}$
the portable Roaring one. The crate maintains

$$
\mathrm{enc} \;=\; \mathrm{enc}^{\mathrm{R}}
\qquad\text{( as functions on fibers )},
$$

not merely $\mathrm{dec}\circ\mathrm{enc} = \mathrm{id}$. This equality is
stronger than semantic compatibility and buys two things that semantic
compatibility does not:

1. **Import and export are $O(\sigma)$, not $O(|S|)$** — a `.roaring` file's
   payload bytes are copied, never re-encoded.
2. **A byte-level differential test exists.** Semantic oracle testing compares
   $\mathrm{dec}(\mathrm{enc}(A))$ with $A$ and is blind to any encoder bug that
   its own decoder mirrors. Byte identity against an independent implementation
   is not.

Decoding is additionally required to be **total**: for every byte string $w$,
$\mathrm{dec}(w)$ either returns an error or returns a fiber satisfying the
encoding invariants. Never a panic. This is a contract on a partial function made
total, and it is what makes fuzzing meaningful rather than decorative.

## 4.5 Representation independence, and the one place it fails

§4.1 chooses an encoding and §4.3 prices an operation, but neither says what an
operation on two *encoded* fibers is required to compute. That obligation is what
licenses the implementation's central testing policy, so it is worth stating.
There are two ways to discharge it — make a generic implementation the oracle
every specialised arm is checked against, which is what this section does, or
make a canonical form the only domain a kernel is defined on, which is what
§15.5's reinterpretation layers do. The property bought is the same.

Write $\mathbb{K} = \{\textsf{Array}, \textsf{Bitmap}, \textsf{Run}\}$ and let
$E_k$ be the encoded forms of kind $k$. Decoding
$\mathrm{dec} : \bigsqcup_k E_k \to \mathcal{P}(\mathbb{L})$ is **surjective and
not injective**: every fiber has at least one encoding — $\textsf{Bitmap}$ is
always legal, at a fixed $8192$ bytes — and a fiber small enough for an array and
regular enough for a run has three. So the encoded forms are not the objects; the
objects are the **quotient** by $\mathrm{dec}$-equality, which is
$\mathcal{P}(\mathbb{L})$ itself.

An operator $\otimes$ is specified on that quotient. Its implementation is a
family of arms $\otimes_{k,l}$, one per ordered pair in
$\mathbb{K} \times \mathbb{K}$, and each arm's obligation is

$$
\mathrm{dec}\big(\otimes_{k,l}(x, y)\big) \;=\; \mathrm{dec}(x) \otimes \mathrm{dec}(y)
\qquad \text{for all } x \in E_k,\ y \in E_l .
$$

**Proposition 4' ( representation independence ).** Every arm of an operator
computes the same function on $\mathcal{P}(\mathbb{L})$. $\square$

That is immediate from the obligation, and three consequences follow which the
system depends on and which are not otherwise stated.

- **The output kind is unconstrained by correctness.** An arm may return any
  encoding of the correct set. A kernel that answers a three-element intersection
  with a bitmap is *wasteful*, not *wrong*, and §4.1's re-encoding rule is what
  repairs it — which is why choosing a kind and computing a set are separate
  mechanisms rather than one.
- **Any single totally-correct arm is a complete semantic oracle for all the
  others.** This is the formal content of the generic iterator kernel: it is
  written once over two decoded iterators, so it is correct for every pair by
  construction, and every specialised arm is checked against it differentially.
  The policy is sound rather than merely prudent, and it is why §13.2 can say
  that adding a fourth kind obliges no specialised kernel at all.
- **Specialisation carries no proof obligation beyond agreement.** A new arm
  cannot introduce a semantic question, only a performance one — so the decision
  to specialise is economic, taken pair by pair.

**The proposition fails at exactly one boundary, and §4.4 is that boundary.**
Representation independence is a statement about *sets*. Byte identity is an
equality of *encoders*, so the kind a fiber is stored in is observable through
`.roaring` export — two runs of the same query that chose different output kinds
would export different bytes while denoting the same set. The freedom the
proposition grants inside the algebra is therefore withdrawn at the format edge,
which is what `export_kind` exists to enforce. **The choice of encoding is free
for semantics and constrained for serialisation**, and conflating those two is
how an operator that is entirely correct produces a file that is not.

---

# 5. Lazy evaluation with ordered chunk streams

Theorem 2 says the algebra factors over fibers. A **stream** is the operational
form of that statement: an object that denotes a set and produces its fibers in
order, so that an expression can be evaluated without any subterm ever existing
as a set.

A stream $s$ denotes $[\![s]\!] \subseteq \mathbb{U}$ and produces

$$
\big(p_1, [\![s]\!]_{p_1}\big),\ \big(p_2, [\![s]\!]_{p_2}\big),\ \dots
\qquad p_1 < p_2 < \cdots,\quad [\![s]\!]_{p_i} \neq \emptyset,
$$

the graph of the fiber map restricted to its support, ascending, with no empty
fibers. Call a stream satisfying this the **conforming** stream for its
denotation.

## 5.1 Canonicity, and why the lift is well defined

Operators are described as homomorphisms lifted along Theorem 2,

$$
[\![\, s \mathbin{\otimes} t \,]\!] \;=\; [\![s]\!] \otimes [\![t]\!] ,
$$

and that is a *definition* only because the object being defined is unique.

**Proposition 6' ( canonicity ).** For each $S \subseteq \mathbb{U}$ there is
exactly one conforming production sequence, namely $(p, S_p)$ for
$p \in \operatorname{supp}(S)$ in ascending order; and every conforming sequence
determines $S = \bigcup_p \phi^{-1}(\{p\}\times S_p)$. Hence
$[\![\cdot]\!]$ is a bijection between conforming **production sequences** and
subsets of $\mathbb{U}$. $\square$

The bijection is on sequences, not on stream objects, and the distinction is not
pedantry: many implementations emit the same canonical sequence while differing
in what their advisory methods report, as the next bullet describes. What
Proposition 6' gives on streams is therefore a bijection on the quotient by
observational equivalence *restricted to the producing interface* — which is
exactly the equivalence the rest of the article reasons up to.

Three things follow that the rest of the article uses without argument.

- **The lift is total and unambiguous.** Defining $s \otimes t$ by its denotation
  picks out one production sequence, so an operator has no freedom in *what* to
  emit — only in how cheaply it computes it.
- **Denotational equality is observational equality — but only on the producing
  interface.** Two conforming streams for the same set produce the same sequence,
  so `next_chunk`, `next_cardinality` and `cardinality_dyn` cannot tell them
  apart, and equational reasoning over those is reasoning about sets. This is
  what lets §6.3 call splitting and concatenation inverses without qualifying the
  sense.

  It does **not** extend to the whole interface, and §5.2's table is where the
  difference lives. `peek_prefix` is a lower bound, so two conforming streams for
  one set may legitimately return different values; `stats` reports chunk count
  and *backing*, which is an implementation fact the denotation does not
  determine; `cardinality_hint` is a bound with a permissive default. Those three
  are **advisory**, and the split is not incidental — a method that the
  denotation determines is one an operator must implement correctly, and a method
  that it does not is one a planner may consult and a proof may not. §6 uses the
  advisory methods for ordering and §4.2's identities use only the producing
  ones.
- **Conformance is an obligation, not a property.** Nothing in the type system
  enforces ascending prefixes or non-empty fibers. An operator that emits an
  empty fiber is not *slightly* wrong; it has left the set of objects
  Proposition 6' is about, and every argument above it silently ceases to apply.
  The $\triangle$ and $\setminus$ operators are where this bites, because they
  are the ones that can discover emptiness only after computing a fiber, and must
  therefore filter before emitting.

Canonicity of this shape is not peculiar to streams. Wherever equality is defined
on a representation rather than on a denotation, the bits the denotation does not
determine have to be pinned by an invariant or everything computed from the
representation goes quietly wrong. §15.4 states that in general and gives two
further instances, and the reason the defect is so hard to see is the same one
§11 records here: the denotation stays right, so nothing that compares denotations
can notice.

## 5.2 One position, five questions

The interface is larger than "produce the next fiber", and the reason is that
callers need different *answers* about the same position, at costs that differ by
orders of magnitude. Advancing is not one operation:

The third column is **the information the answer is required to carry**, not what
any particular implementation touches to produce it:

| operation | answers | answer contains |
|---|---|---|
| `next_chunk` | the next fiber and its payload | the payload |
| `next_cardinality` | the next fiber's prefix and $\lvert\cdot\rvert$ | a count |
| `peek_prefix` | a lower bound on the next prefix | a prefix |
| `seek(p)` | positions at the first prefix $\ge p$ | nothing |
| `cardinality_dyn` | $\lvert [\![s]\!] \rvert$, consuming | a count |

Table: The five questions a stream position can be asked, and what each answer is required to carry.

**Read as actual behaviour the column would be false, and the distinction is
load-bearing rather than pedantic.** The trait defaults for `next_cardinality`
and `cardinality_dyn` both route through `next_chunk` and materialise every
payload — the source calls its own default "correct and materializing". And
`And::peek_prefix` materialises: it calls `fill()`, which computes the
intersection into a pending slot, because an intersection cannot know its next
surviving prefix without doing the work. That is not a defect and not a missing
override; it is intrinsic to the operator.

So the table states what a caller may *ask for* cheaply, and the gap between the
column and the implementation is precisely where the optimisation opportunity
lives. The organising principle is that **asking a more informative question than
the caller needs is the dominant avoidable cost in the system**. An operator that
holds a chunk in order to read its length pays for a payload it discards; §12.2
records three such sites, and removing one of them accounted for most of a factor
that had been attributed to a query rewrite. The interface is wide so that the
question can be narrow.

## 5.3 `seek`, and where §4.3's bound is actually realised

`seek(p)` positions the stream at the first fiber with prefix $\ge p$. This is
not an optimisation of the interface but the place where §4.3's
$O(m\log(1+n/m))$ becomes *reachable*: an intersection that could only scan
would be $\Theta(m+n)$ however good its kernels are, because the bound is a
statement about probing, and probing is what `seek` exposes.

Reachable, not guaranteed. The contract fixes only where the stream ends up, not
how it gets there — a correct implementation may scan, and the trait says so
explicitly, asking implementations to gallop without requiring it. So §4.3's
bound is a property of the leaf implementations, not of the algebra: a
conforming stream that scans is still a conforming stream, and every argument in
this article about *meaning* survives it while every claim about *cost* does
not.

So the leapfrog of §4.3 is: peek both sides, advance the *behind* side to the
other's prefix by `seek`, repeat. The smaller operand drives the number of
probes, which is the operational content of "the smaller operand should win", and
it is why $\cap$ visits $O(\min)$ fibers while $\cup$ must visit their sum.

## 5.4 Two contract points that carry mathematical content

**Lower-bound peeking.** `peek_prefix` satisfies

$$
\mathrm{peek}(s) \;\le\; \min\{\,p : p \text{ not yet produced},\ [\![s]\!]_p \neq \emptyset\,\}.
$$

It is an *admissible heuristic*: sound as a lower bound, not exact. For
$\triangle$ and $\setminus$ a candidate prefix may cancel to $\emptyset$ and be
skipped. Ordering decisions may use $\mathrm{peek}$; emptiness and cardinality
may not, and must go through the exact path. Confusing the two is the classic
error, and the type system cannot catch it — which is why it is stated as an
invariant.

**Non-materialising cardinality.** Every operator must override the cardinality
computation with a walk that applies Proposition 6 fiberwise:

$$
\big|[\![s\otimes t]\!]\big| \;=\; \sum_{p} f_\otimes\big(|[\![s]\!]_p|,\ |[\![t]\!]_p|,\ c_p\big),
\qquad c_p = \big|[\![s]\!]_p \cap [\![t]\!]_p\big| .
$$

The default — materialise each result fiber, then count it — is *extensionally
equal* to the override. That is exactly why it is dangerous: no correctness test
can distinguish them, and the separating observable is a **resource** one. Which
resource is not fixed, and choosing wrongly is how the property goes unguarded:
once containers are reference-counted, handing one back where a length would do
allocates nothing, so an allocation counter cannot see the difference at all.
§11 states what does separate them.

## 5.5 Closure, and what it buys

Every operator is itself a stream. The leaves are a materialised set, a range
synthesised without storage, and the empty stream; the combinators are
$\cap, \cup, \triangle, \setminus$, complement within a range, restriction to a
prefix window, ordered concatenation, and an $n$-ary union. All are `ChunkStream`,
so a composite is interchangeable with a leaf.

That closure is what makes §6's lowering total: an expression tree of any depth
becomes a single stream by structural recursion, with no case in which an
intermediate must be collected. It is also what makes the cardinality obligation
above load-bearing rather than local — a single operator that inherits the
materialising default silently converts every expression containing it, however
deep, back into the thing streams exist to avoid.

**What is not claimed: productivity is not free.** A stream may denote a set
whose support is $2^{48}$ fibers — a complement is the standard case — and
nothing in the contract bounds how many productions separate one non-empty fiber
from the next. Operators that would otherwise walk such a domain do not answer by
producing: §6.1's cost model exists precisely because $\neg$ answers cardinality
by walking its *input* rather than its denotation. The obligation is that no
operator be *required* to enumerate its denotation in order to answer a question
about it, and it is discharged operator by operator rather than by the contract.

# 6. Cost-directed expression rewriting

§5 makes evaluation lazy; it does not make it well chosen. Two terms denoting the
same set can differ in execution cost by orders of magnitude — §6.1 exhibits a
gap of up to $2^{48}$ between two forms of the same query — so there is something
for a rewriter to do before anything is opened. This section says what a rewriter
is allowed to do ( preserve the *set*, never merely its cardinality ), what it
should aim at ( two cost functions, because counting and enumerating are
different problems ), and why the process stops ( Theorem 7, on a measure that
deliberately excludes the cost model, so that revising the cost model cannot
break termination ). §12.3 then bounds what any planner of this kind can win.

Let $\mathcal{E}$ be the term algebra

$$
e \;::=\; \mathbf{0} \;\mid\; \mathrm{Set}(S) \;\mid\; \mathrm{Range}(lo,hi)
   \;\mid\; e \cap e \;\mid\; e \cup e \;\mid\; e \triangle e
   \;\mid\; e \setminus e \;\mid\; \neg_{[lo,hi)} e
$$

with the evident denotation $[\![\cdot]\!] : \mathcal{E} \to
\mathcal{P}(\mathbb{U})$. Planning is a rewriting relation
${\Rightarrow} \subseteq \mathcal{E}\times\mathcal{E}$ required to satisfy

$$
e \Rightarrow e' \;\implies\; [\![e]\!] = [\![e']\!]
\qquad\text{( soundness )}
$$

while decreasing a cost. Note that soundness is stated on the *set*, not on its
cardinality: a rule preserving only $|[\![e]\!]|$ would be catastrophically wrong
for the materialising path.

## 6.1 Two cost functions, because they genuinely differ

Two functions $\mathcal{E}\to\mathbb{N}$ are needed, because a term's *output*
size and the work to *count* it are different numbers. Write
$\chi(lo,hi) = \lceil (hi-lo)/2^{16} \rceil$ for the chunk width of a range.

$\mathrm{Y}$ estimates chunks produced when the stream is drained:

$$
\begin{aligned}
\mathrm{Y}(\mathbf{0}) &= 0, &
\mathrm{Y}(S) &= \sigma(S), &
\mathrm{Y}(\mathrm{Range}(lo,hi)) &= \chi(lo,hi), \\
\mathrm{Y}(\neg_{[lo,hi)} e) &= \chi(lo,hi), &
\mathrm{Y}(a \cap b) &= \min(\mathrm{Y}a, \mathrm{Y}b), &
\mathrm{Y}(a \setminus b) &= \mathrm{Y}a,
\end{aligned}
$$

with $\mathrm{Y}(a \cup b) = \mathrm{Y}(a \triangle b) = \mathrm{Y}a + \mathrm{Y}b$
less the shared prefixes when a sketch can bound them, and $\cap$ likewise
tightened by that bound when available.

$\mathrm{C}$ is the planner's cost, and it is **not** a function
$\mathcal{E}\to\mathbb{N}$. It is a heuristic evaluated against mutable state,
which is stated first because everything below is only meaningful with that
caveat attached. Its arms are, up to that state:

$$
\begin{aligned}
\mathrm{C}(\mathbf{0}) &= 0, &
\mathrm{C}(\mathrm{Range}) &= 1, &
\mathrm{C}(S) &= \sigma(S), \\
\mathrm{C}(a \cap b) &= \min(\mathrm{Y}a, \mathrm{Y}b), &
\mathrm{C}(a \setminus b) &= \mathrm{Y}a, &
\mathrm{C}(\neg_{[lo,hi)} e) &= \max\big(\min(\mathrm{Y}e,\ \chi(lo,hi)),\ 1\big),
\end{aligned}
$$

with $\mathrm{C}(a\cap b) = 0$ when the operands are provably disjoint, and

$$
\mathrm{C}(a \cup b) = \mathrm{C}(a \triangle b) =
\begin{cases}
\max(\mathrm{Y}a + \mathrm{Y}b,\ 1) & \text{prefix spans strictly separated,}\\
\max(\texttt{M}\cdot(\mathrm{Y}a + \mathrm{Y}b),\ 1) & \text{otherwise,}
\end{cases}
$$

$\texttt{M} = \texttt{MERGE\_STEP} = 2$. The split matters: a union whose parts
are disjoint in prefix order is lowered to a concatenation and never merged, so
charging a merge would overstate the cheapest union shape in the language by a
factor of two.

**Three caveats, and the first is disqualifying for any use in a proof.**

- **$\mathrm{C}$ depends on traversal history, not only on $e$.** The
  disjointness and shared-prefix tests it consults are gated on a *cumulative*
  statistics allowance held in a mutable thread-local cell. Once that allowance
  runs short, later operands fall back to coarser bounds, so the same expression
  can be priced differently depending on what was priced before it — and
  `cheaper` evaluates candidate and original sequentially under that changing
  state. This is sound for the planner ( every gated statistic is conservative in
  the safe direction, so it changes which rewrites fire and never an answer ) and
  it is fatal for a termination measure, which is why Theorem 7 does not use it.
- The arithmetic above is idealised: the implementation uses saturating add,
  subtract and multiply throughout, so the recursion is exact only below the
  saturation point.
- $\mathrm{Y}$ is itself an estimate, tightened by sketches when the budget
  allows and falling back to bounds when it does not.

The two exceptions are still the point of the planner. $\mathrm{C}(\mathrm{Range})$
is $1$ against a $\mathrm{Y}$ of up to $2^{48}$: a range's cardinality is
arithmetic. And a complement is counted through the identity of §4.2 by walking
its *input* — clipped to the window, which is the $\min$ above. So a term whose
output is $2^{48}$ chunks can be counted in a few thousand steps, and a planner
knowing only one of the two numbers would misrewrite in one direction or the
other.

The clip is recent. `cardinality_cost` previously priced a complement at
$\mathrm{Y}e$ outright, so a one-chunk window over a million-chunk input was
charged a million; the correction landed on 2026-08-27, and it made the model
more faithful without moving a single plan — both forms it distinguishes already
priced at $1$. An earlier draft of §6.2 used that mispricing as an example, which
is a reminder that a cost model is source that moves, not a definition that stays
put.

**$\mathrm{C}$ has a known blind spot, and it is a large one.** Every arm above is
counted in *chunks visited*; none carries a term for **materialisation**. A
complement counts by subtraction and builds nothing, while an intersection must
build both operands before it can count them — and $\mathrm{C}(a \cap b)$ is
$\min(\mathrm{Y}a, \mathrm{Y}b)$ either way. The consequence is measurable: for
$\neg p \cap \neg q$ against $\neg(p \cup q)$ over two $65\,535$-element
operands, both sides price at $1$, the planner therefore declines the rewrite for
want of a *strict* decrease, and the form it keeps is **$214\times$ slower** in a
release build. Recorded as `cost-model-cannot-see-materialization`.

This is a limitation of the cost model, and Theorem 7 is untouched by it — the
termination measure does not mention $\mathrm{C}$ at all. What it bounds is plan
*quality*: §6 currently chooses among semantically equal forms using a model that
cannot see one of the two dominant costs. That is the honest statement of what
the planner achieves, and it is a separate question from whether it halts.

## 6.2 The rules

The rules divide into two kinds, and the division is the important part. Most are
unconditional identities of Boolean algebra and are sound whatever the operands
contain. The rest fire only under a *side condition* — disjointness, containment,
a range relation — and every one of those conditions must be established from a
source that never answers "yes" when the truth is "no", because a false positive
here does not slow a query down, it returns the wrong set. §7 is the machinery
for supplying such answers, and §7.1's one-sidedness is what makes the third
column below safe to read.

| Class | Rule | Condition |
|---|---|---|
| Identity | $e \cap \mathbf{0} \Rightarrow \mathbf{0}$, $e \cup \mathbf{0} \Rightarrow e$, … | none |
| Definition | $R \setminus e \Rightarrow \neg_R\, e$ | $R$ a range |
| Absorption | $e \cap R \Rightarrow e$, $e \cup R \Rightarrow R$, $e \triangle R \Rightarrow \neg_R e$, $e\setminus R \Rightarrow \mathbf{0}$ | $[\![e]\!] \subseteq [\![R]\!]$ |
| Disjointness | $a \cap b \Rightarrow \mathbf{0}$, $a \setminus b \Rightarrow a$, $a \triangle b \Rightarrow a \cup b$ | $[\![a]\!] \cap [\![b]\!] = \emptyset$ |
| Range algebra | $R_1 \cap R_2$, $R_1 \cup R_2$ fuse; $R_1 \setminus R_2$ splits into $\le 2$ ranges | intervals |
| De Morgan | $\neg p \cap \neg q \Rightarrow \neg(p \cup q)$ and $\neg p \cup \neg q \Rightarrow \neg(p \cap q)$ — **factoring only** | $\mathrm{C}$ strictly drops |

Table: The rewrite rules, with the side condition each one requires.

The conditional rules require *proofs*, not guesses, and the two directions of
error are not symmetric:

- Claiming containment that does not hold **deletes rows**.
- Failing to claim containment that does hold **loses an optimisation**.

So every predicate here is one-sided: it may answer "unknown", never a false
"yes". §7 is about how those predicates are computed.

**Theorem 7 ( termination ).** Weight the node shapes

$$
w(\mathbf{0}) = w(R) = w(S) = 1, \quad
w(\cup) = 2, \quad
w(\neg) = 3, \quad
w(\cap) = 4, \quad
w(\setminus) = w(\triangle) = 6,
$$

and let $\mu(e) = \lVert e \rVert = \sum_{\text{nodes}} w(\cdot) \in \mathbb{N}$.
Then **every rewrite outcome strictly decreases $\mu$**, and $\mathbb{N}$ is well
founded, so $\Rightarrow$ terminates. $\square$

A `cheaper`-rejected candidate is not a counterexample: it leaves the term
identical, so it is not a step of $\Rightarrow$ at all.

**The single component is the point of this version.** $\mu$ mentions no cost
function, so termination is independent of `cheaper`, of the statistics budget,
of saturating arithmetic, and of every future correction to the cost model. That
independence is not aesthetic. Earlier versions of this theorem used
$\mathrm{C}$ as a second lexicographic component, and $\mathrm{C}$ is
**not eligible**: §6.1 records that it consults statistics gated on a mutable
thread-local budget, so its value depends on traversal history rather than on the
expression, and a lexicographic component must be a function of the term. The
cost model also moved three times while the proof quoted it.

The weights admit slack, but three rules constrain them, and each defeats the
obvious measure — plain node count — on its own:

| rule | plain size | weighted |
|---|---|---|
| $a \triangle b \Rightarrow a \cup b$ | unchanged | $6 \to 2$ |
| $\neg\neg x \Rightarrow x \cap R$ | unchanged, $\lvert x\rvert + 2$ either side | $6 \to 5$ |
| $\neg p \cup \neg q \Rightarrow \neg(p \cap q)$ | unchanged | $8 \to 7$ |

Table: Three rules that a plain node count cannot order, which is why the termination measure is weighted.

The first needs $w(\triangle) > w(\cup)$, the second $w(\neg) > \tfrac12(w(\cap) + 1)$,
and the third $w(\cup) + 2w(\neg) > w(\neg) + w(\cap)$, i.e.
$w(\cup) + w(\neg) > w(\cap)$. **That last inequality is where the previous
version failed.** With $w(\cup) = 1$ it holds with equality, the arm is neutral,
and a second component becomes necessary; raising $w(\cup)$ to $2$ makes it
strict and the second component disappears. The change is one integer.

**De Morgan applies in the factoring direction only** — two complements
collapsing into one — which is what makes the third row available. Were the
expanding direction $\neg(p \cap q) \Rightarrow \neg p \cup \neg q$ also a rule,
it would run that row backwards from $7$ to $8$ and no weighting could satisfy
both. So the theorem depends on a property of `pass_b`, and it is worth naming
rather than leaving as luck.

**The proof obligation is a loop over the rewrite outcomes, so it is written as
one.** A checker in the project's gate transcribes the $32$ outcome schemas of
`pass_b` — $30$ syntactic outcome sites, one of which returns three shapes — and
checks each against the weights symbolically:
metavariables carry coefficient vectors, so each inequality is verified for all
subterm weights at once rather than for one instance. All $32$ must strictly
decrease, and all $32$ do.

**What that check establishes is bounded, and the bound is worth stating.** It
does not parse Rust, so it cannot prove the transcription is *complete*; that is
a manual audit. What it does instead is pin a hash of `pass_b` — conservatively,
the whole function including its traversal prologue and comments — and fail on
any drift, so a rule added or changed without re-auditing turns the gate red
rather than leaving a stale green. Four earlier versions of this theorem were
wrong, and each would have failed the row-by-row check on a named line; none of
them failed the prose argument printed beside it.

Termination does **not** rest on the implementation's iteration cap; the cap is a
backstop against a future rule pair that violates the hypothesis.

**And the rewrite system is terminating but not confluent.** The Definition rule
$R \setminus e \Rightarrow \neg_R e$ and the Range-algebra rule for
$R_1 \setminus R_2$ overlap whenever $e$ is itself a range, giving a critical
pair. On the example above the two paths reach $\neg_{[0,100)}[40,60)$ and
$[0,40) \cup [60,100)$; both are normal forms, since no rule reduces a complement
of a range and no rule fuses two disjoint ranges. Two distinct normal forms for
one term is a failure of confluence. Newman's lemma runs
$\text{terminating} + \text{locally confluent} \Rightarrow \text{confluent}$, and
Theorem 7 supplies termination independently, so the contrapositive applies: the
system is **not locally confluent** either. The order matters — termination is a
hypothesis of the lemma, never a conclusion of it.

Three consequences worth stating, because none of them is a defect and all three
are easy to mistake for one. Meaning is preserved on every path, so the ambiguity
is in *which* cheaper form is produced, never in what it denotes. The normal form
is **not canonical**: two expressions denoting the same set can normalise
differently. And the implementation's rule order is therefore load-bearing for
cost while irrelevant to correctness — which is why the arms are ordered
deliberately rather than alphabetically.

## 6.3 Segmentation: partitioning the prefix domain

Consider a Boolean expression $e = \psi(e_1,\ldots,e_m)$ whose leaves have
operand spans $[\ell_i,h_i] \subseteq \mathbb{P}$. Let

$$
X = \{\ell_i\} \cup \{h_i + 1\}
$$

be the sorted cut points, inducing a partition
$\mathbb{P} = \biguplus_j \Pi_j$ into intervals.

**Proposition 8 ( restriction and reassembly over a partition ).** For every
segment $\Pi_j$, restrict every operand while retaining its position in the
expression tree. Then

$$
[\![e]\!]
\;=\;
\biguplus_j
[\![\psi(e_1|_{\Pi_j},\ldots,e_m|_{\Pi_j})]\!].
$$

*Proof.* Boolean set operations are pointwise. Restricting every operand to the
segment therefore restricts the result to that segment. The result restrictions
are disjoint and reassemble the original result because the $\Pi_j$ partition
the prefix domain. $\square$

Let $\mathrm{contrib}(j)=\{i:[\ell_i,h_i]\cap\Pi_j\ne\emptyset\}$.
Contributor sets are an optimisation derived from this identity, not part of the
identity itself. For union and symmetric difference, an absent operand may be
omitted because the empty set is an identity; a segment with one contributor
is consequently a pass-through. For intersection, any absent operand annihilates
the segment. For ordered difference, operand position must be retained:
$\emptyset \setminus B = \emptyset$, whereas $A \setminus \emptyset = A$.
For an arbitrary nested expression, the sound procedure is to substitute the
empty set for absent leaves and simplify the resulting expression. The current
segmentation optimisation specialises this result to union.

Specializing $\psi$ to union, the parts are disjoint and ordered, so reassembly
is concatenation — no comparisons at all. *Ordered* is doing work there and is
not free: it holds because every cut in $X$ is a prefix boundary, so no fiber
straddles a segment. §14 proves this, separates the alignment-free half of the
decomposition from the half that needs it, and gives the $O(1)$ reassembly that
replaces concatenation for a segmentation cutting anywhere else. Where
$|\mathrm{contrib}(j)| = 1$ the
segment is a pass-through, no merge, and its cardinality is the operand's own,
which for a range is $O(1)$. The rewrite pays exactly when the contributor sets are
non-uniform, and it is declined when spans are unknown, identical, or when
re-opening an operand would duplicate a whole evaluation.

---

# 7. Planner statistics and sound abstractions

The planner needs two different things about operand supports, and they are not
the same question:

- **"How much do $X$ and $Y$ share?"** — a *magnitude*, used to size work.
- **"Where are $X$ and $Y$?"** — a *position*, used to cut segments.

## 7.1 Positions: occupancy as an abstract interpretation

Fix a base $\beta$ and a shift $\tau$ with $2^{\tau}$ chosen so that at most
$B = 256$ buckets cover the domain of interest. Define

$$
\alpha(X) \;=\; \Big\{\ \big\lfloor (p-\beta)/2^{\tau} \big\rfloor \ :\ p \in X \ \Big\}
\ \subseteq\ \{0,\dots,B-1\}.
$$

$(\alpha,\gamma)$ with $\gamma(\hat{X}) = \{p : \lfloor (p-\beta)/2^\tau\rfloor
\in \hat{X}\}$ is a Galois connection: both maps are monotone,
$X \subseteq \gamma(\alpha(X))$, and $\alpha(\gamma(\hat X)) \subseteq \hat X$.
The last of these holds with **equality**, since every bucket is a non-empty set
of prefixes, so $\alpha\gamma = \mathrm{id}$ and the connection is a
coreflection. That is the formal content of "$\alpha$ is exact at its
resolution": abstracting a concretisation loses nothing, and all the imprecision
lives in $\gamma\alpha$, the other composite.

**Proposition 9 ( sound omission ).**
$\alpha(X) \cap \alpha(Y) = \emptyset \implies X \cap Y = \emptyset$.
The converse fails. $\square$

Hence dropping an operand from a segment when no overlapping bucket is marked is
sound; including one that contributes nothing merely costs an unnecessary merge.
The resolution loss is one-directional in the safe way: it costs optimisation,
never correctness.

$\alpha$ is not the only map of this shape, and §15.6 says which shape it is: the
existential half of an adjoint triple whose parameter is the size of a bucket,
with Proposition 9 one entry of a table of one-sided laws. Nothing below depends
on that, and it was not visible until a second instance existed.

$B = 256$ is a **cost bound, not a precision knob**. Refining $\tau$ to
single-prefix resolution would separate two operands occupying alternating chunks
perfectly — into $2n$ segments, each re-opening its operands, which is strictly
worse than the merge it replaces. Capping $B$ bounds the segment count by
construction, so genuinely interleaved operands are reported as "both, everywhere"
and left alone. Occupancy finds **gaps**, not interleaving.

## 7.2 Magnitudes: bottom-$K$ sketches, and why not a Bloom filter

Let $h$ be SplitMix64, a **bijection** on $2^{64}$. The sketch of a support $X$
is the $K = 256$ smallest values of $h(X)$ together with $n = |X|$.

**Proposition 10 ( exactness below saturation ).** If $|X| \le K$ and $|Y| \le
K$, the sketches retain $h(X)$ and $h(Y)$ in full; since $h$ is injective,
$h(X)\cap h(Y) = h(X\cap Y)$, so $|X \cap Y|$ is computed *exactly*, with no
false positives and no false negatives. $\square$

Above saturation, the standard $k$-minimum-values estimator applies. Let $U =
X\cup Y$, take the $k$ smallest hashes of $h(X)\cup h(Y)$, and let $b$ of them
lie in both. Then $\hat{J} = b/k$ estimates the Jaccard index $J = |X\cap
Y|/|X\cup Y|$, and inverting

$$
J = \frac{|X\cap Y|}{n_X + n_Y - |X\cap Y|}
\qquad\Longrightarrow\qquad
\widehat{|X \cap Y|} \;=\; \frac{\hat{J}\,(n_X+n_Y)}{1+\hat{J}} .
$$

This is the **simple, biased** form: $\hat J$ is a ratio estimator and the
inversion is non-linear, so $\mathbb{E}[\widehat{|X\cap Y|}] \neq |X\cap Y|$ in
general, and no error bound is claimed for it here. An unbiased estimator for the
same synopsis exists — see §12.5 — and the reason the biased one is tolerable is
structural rather than statistical: the estimate only ever *sizes* work.

This is used to **size** work — to cost $\cap$ by shared prefixes and
$\cup,\triangle$ by the estimated union — and never to prove a set empty, because
a false zero would be rewritten to $\mathbf{0}$ and would silently delete rows.
Disjointness is claimed only from an exact source ( Proposition 10, or the exact
galloping merge of two materialised prefix arrays, which is *cheaper* than the
intersection it is costing ).

**Why a Bloom filter is the wrong instrument here.** Equal prefixes hash equal,
so a zero AND of two filters does imply disjointness — the implication is valid
and the design is still useless. With $m$ bits and $a, b$ bits set by the two
operands, under the usual independence approximation

$$
\Pr[\text{AND is empty}] \;\approx\; \Big(1-\frac{a}{m}\Big)^{b}
\;\approx\; \exp\!\Big(-\frac{ab}{m}\Big),
$$

so a constant probability of proving disjointness requires $m = \Omega(ab)$, i.e.
$m = \Omega(n^2)$ for $n$-element supports. At $n = 500$ and $m = 4096$ the
expected overlap is $\approx 61$ bits and the AND is essentially never empty:
nothing is ever proved. The birthday bound, not an implementation detail, is what
kills it.

## 7.3 Neither statistic is always computed, and the abstraction is what absorbs it

Both instruments above are **rationed**. An individual operand is eligible only
when its chunk count does not exceed
$\texttt{STATS\_MAX\_CHUNKS}=4096$. Eligible work is then charged against a
cumulative allowance of $\texttt{STATS\_BUDGET}=16\,384$ chunks, further capped
per query at
$\texttt{cheap\_yield}(e)\cdot\texttt{STATS\_HEADROOM}$. The allowance is held
for the duration of one planning invocation. An operand that fails either limit
gets no statistic; the planner falls back to its prefix span, an interval
available from index metadata in $O(1)$.

The per-operand ceiling prevents a single scan from dominating planning, whereas
the cumulative budget bounds the total across a many-operand expression. The
cumulative allowance is spent in traversal order rather than divided in advance,
so the operands receiving precise statistics can depend on traversal order once
the allowance is exhausted.

That sounds like it should damage §7.1, and it does not, for a reason the
abstract-interpretation framing states cleanly. There is not one abstraction here
but a small chain of them, ordered by precision:

$$
\alpha_{\mathrm{occ}} \ \sqsubseteq\ \alpha_{\mathrm{span}},
\qquad
\alpha_{\mathrm{span}}(X) = [\min X, \max X],
$$

the interval domain being the coarsening that keeps one bucket instead of $B$.
**Proposition 9 holds pointwise in this chain** — a disjointness claim from any
member is sound, because each is an over-approximation of $X$ — so the budget
chooses *which* sound abstraction is used and can never produce an unsound one.
What it costs is resolution, and resolution costs only optimisation.

This is worth separating from the superficially similar situation in §6.1. There,
the same rationing makes $\mathrm{C}$ fail to be a function of its argument, and
that was disqualifying, because a termination measure must be a function of the
term. Here nothing is claimed to be a function of the expression alone: §7's
obligation is *soundness of an implication*, and an implication that holds for
every member of the chain holds whichever member the budget selects. The same
mechanism is fatal to one use and harmless to the other, and the difference is
what the two arguments quantify over.

---

# 8. Versioned persistence and zero-copy reclamation

Requirement (R2) asks for two things that sound like one: a transaction is
all-or-nothing, and a reader sees a *prefix-closed* set of transactions for its
entire lifetime. The second is the harder one, because "its entire lifetime"
outlives checkpoints and outlives the reuse of the space the reader's data sits
in. §8.1 gives the frontier that makes both true at once, §8.2 shows that
redo-only recovery is then not an achievement but a corollary of where dirty
pages are allowed to go, and §8.3 is where requirement (R3) turns all of this
from a correctness question into a memory-safety one — because a container that
aliases the file cannot tolerate the file changing under it, and the resulting
use-after-free is of a *file slot*, which no memory sanitiser can see.

## 8.1 The versioned state

A transaction $t$ is a finite set of updates and is assigned a **commit version**
$v(t) \in \mathbb{N}$, monotone, dense, never reused. Define the state after
version $v$ as the fold

$$
D_v \;=\; \big(\,\mathrm{apply}(t_v) \circ \cdots \circ \mathrm{apply}(t_1)\,\big)(D_0).
$$

Each transaction slot carries a state in $\{\textsf{Empty}, \textsf{Pending},
\textsf{Durable}, \textsf{Aborted}\}$. Define the **visible watermark**

$$
W \;=\; \max\ \{\, v \;:\; \forall u \le v,\ \mathrm{state}(u) \in \{\textsf{Durable}, \textsf{Aborted}\} \,\}.
$$

The visibility result requires two publication premises in addition to the
definition of $W$:

1. every shard update of transaction $t$ is installed before
   $\mathrm{state}(v(t))$ becomes $\textsf{Durable}$; and
2. a snapshot captures roots and applies version filtering so that each shard is
   observed at the same captured watermark.

**Proposition 11 ( atomic multi-shard visibility ).** Under these publication
premises, a reader that snapshots $W$ observes exactly $D_W$.

*Proof.* Prefix closure determines which transaction versions are eligible.
The first premise makes all shard effects of each eligible durable transaction
available before publication, and the second prevents a snapshot from combining
shard states from different watermarks. Because each transaction has one version,
all of its shard effects are therefore either included or excluded. $\square$

**Corollary 11.1 ( the abort path is not optional ).** If a version is assigned and
never resolved, the supremum in the definition of $W$ is capped at $v-1$
*forever*. Hence a failed transaction must be recorded as `Aborted` with the same
durability weight as a commit; "do nothing on failure" is not a valid
implementation of $W$.

Late assignment — taking $v$ while holding the participating shard locks, in
ascending shard order — gives per-shard monotonicity ( **I5** ): each shard's log
is strictly increasing in $v$, which is what makes a crash-truncated log a clean
*suffix* removal rather than a scatter of holes. It also bounds the
assign-to-durable window by one fsync, and deadlock-freedom follows from the
ordered acquisition.

## 8.2 Recovery is redo-only, and that is a theorem

Invariant **I4** ( checkpoint barrier ): the on-disk image persists only state at
or below the global visible watermark, i.e. the checkpointed image equals
$D_{W_{\mathrm{ckpt}}}$ for some $W_{\mathrm{ckpt}} \le W$.

**Proposition 12 ( recovery is redo-only ).** Under I4, no effect of an unresolved transaction ever reaches
the data file. Hence "undo" is vacuous, and recovery is exactly

$$
D_{W'} \;=\; \big(\mathrm{replay\ of\ log\ records\ with\ } v \le W'\big)\big(D_{W_{\mathrm{ckpt}}}\big),
$$

where $W'$ is the largest prefix of versions found fully durable in the log.
No acknowledged transaction is lost, because acknowledgement happens only after
$W \ge v$, which requires every version at or below $v$ to have been resolved and
fsynced. $\square$

The same statement is what makes physical replication correct by construction:
the follower applies raw log frames through *the same decoder as recovery*, so
there is one framing and one scanner rather than two that can drift. Determinism
is required at the level of set **contents**, not container **encoding** — a
follower may legitimately hold a bitmap where the leader holds an array for the
same fiber, since Theorem 2 is about sets and §4 is about their representation.

## 8.3 The keystone: immutability is a memory-safety obligation

Requirement (R3) says a container aliases the mapping. Therefore writing to a
page a live reader can reach is *undefined behaviour*, not a torn read. This
upgrades shadow paging from a preference to a requirement, and it means an extent
$e$ may be reused only when **nobody can still reach it**. That is one sentence
with three unrelated *failure modes* hiding in it, and separating them is the
content of this subsection. ( Not three populations: as the table below records,
one live snapshot can be the subject of two of them at once. )

Let $e$ be superseded by checkpoint $k = \mathrm{ckpt}(e)$. Reuse requires all
three of:

- **(A) Reachability.** No live reader holds an index root from which $e$ is
  reachable. An extent superseded by checkpoint $k$ is reachable from the roots of
  checkpoints $< k$, so this is: no live reader pins a root from a checkpoint
  below $k$.
- **(B) Recovery visibility.** No *future* recovery can reach $e$. After the
  checkpoint that superseded it flipped the superblock, the **other** A/B slot
  still names the previous root, which still references $e$; the obligation is
  discharged by the double-buffer delay of two checkpoints.
- **(C) No live alias.** No materialised zero-copy buffer over $e$ is still alive.

**No one of these implies another, which is why none can be dropped.** The claim
is non-subsumption, not sufficiency — and not disjointness either: a single live
snapshot can simultaneously hold a root reaching $e$ *and* a materialised buffer
over it, so (A) and (C) can have the same subject in one execution. What is
independent is the *failure mode* each rules out, and the observable each is
answerable from.

| obligation | rules out | answerable at the moment of decision from |
|---|---|---|
| (A) reachability | a reader following a pinned root into recycled bytes | per-reader pinned-root state |
| (C) no live alias | an escaped zero-copy buffer outliving its snapshot | the alias count |
| (B) recovery visibility | **a process that does not yet exist** — recovery, reading the other superblock slot | the checkpoint sequence, via the A/B slot age |

Table: The three reclamation obligations, by what each one rules out.

(A) and (C) are independent in both directions, each with a witness the other
misses. A snapshot may be *entitled to an extent it has never materialised*, where
the alias count is zero at that instant — (C) is silent, (A) is not. And a
zero-copy buffer is `'static`, so it may *outlive the snapshot that produced it* —
(A) is silent, (C) is not. Neither witness requires the other's subject to be a
different reader; they show non-subsumption, which is all the argument needs.

**(B) needs its own witness, and it is of a different kind.** Consider a database
with **no live readers at all**: (A) holds vacuously and (C) holds, since nothing
is pinned. Reuse is still unsafe, because a crash before the next flip leaves
recovery free to read the stale slot and follow a root into an extent now holding
other data. (B) is the only obligation constraining an agent absent when the
decision is taken: it holds no pin, so no refcount sees it, and it has no reader
state at all, so nothing about the live readers bears on it. What answers it is
*persisted* — how many checkpoints have flipped since $e$ was superseded — which
is why it is checkable at all despite its subject not existing. A scheme with only the two runtime obligations is
correct for every execution that does not crash — exactly the class of bug that
testing finds last.

### A version-only reachability proxy: counterexample and repair

The recovery-age and live-alias obligations, (B) and (C), have direct observables:
a checkpoint sequence and an alias count. Reachability obligation (A) cannot in
general be reduced to the reader's transaction version, because transaction
visibility and root reachability are different orders.

A former implementation used such a version proxy. The following interleaving is
a counterexample:

1. a checkpoint samples visible version $w$;
2. a commit advances visibility to $w+1$;
3. a snapshot records $w+1$ but captures a shard root before that shard is
   replaced by the checkpoint;
4. the checkpoint supersedes an extent reachable from the captured root and
   records retirement metadata derived from $w$; and
5. after the recovery delay and with no materialised alias, a version comparison
   permits reuse even though the snapshot root still reaches the extent.

No strict or non-strict comparison of transaction versions repairs this
counterexample, because the order being approximated is root publication order.
The appropriate observable is the checkpoint sequence associated atomically
with the captured root. If an extent is superseded by checkpoint $k$, obligation
(A) is discharged only when no live reader retains a root from a checkpoint
sequence below $k$.

The evaluated implementation now publishes this checkpoint sequence with each
reader's captured roots and combines it with the two-checkpoint recovery delay
and per-extent alias tracking. A deterministic concurrency regression constructs
the interleaving above and verifies elementwise snapshot stability through
reclamation. This is history-sensitive evidence: a generator that samples only
isolated states cannot reach the failure.

Cached cardinality does not provide an adequate oracle for this regression.
Cardinality may be answered from index metadata without decoding the recycled
payload, so an equal-cardinality substitution can remain invisible. The
regression must materialize and compare elements.

### The same three obligations partition a second way, and the two partitions differ

The table above sorts them by *what observes them*. There is a second sort, by
whether the obligation is **monotone along the queue of deferred extents**, and it
cuts the set differently — which is why the conjunction cannot be evaluated the
way its symmetry invites.

Superseded extents are queued in non-decreasing order of the checkpoint at which
they were superseded, and the A/B deadline is that checkpoint plus a **constant**
delay, so the two keys are non-decreasing together and differ by a fixed offset.
So (A) and (B) are **monotone in queue position** — the first entry that fails
either guarantees every entry behind it fails too, and the scan may stop there.
Pins are not ordered at all: a pin is a live buffer held by an arbitrary reader,
indexed by neither checkpoint nor position, so an entry failing (C) says nothing
whatever about its successors. It must be carried aside and retried, never used to
terminate the scan.

| obligation | monotone in the queue | observable by |
|---|---|---|
| (A) reachability | yes | per-reader pinned-root state |
| (B) recovery visibility | yes | neither |
| (C) no live alias | **no** | alias count |

Table: The same three obligations partitioned a second way. The two partitions do not agree, which is the point.

Neither partition refines the other: (B) is alone in the first table and shares a
class with (A) in the second. A conjunction is not, in general, evaluable by the
discipline any one of its conjuncts admits.

The cost of getting this wrong is recorded in the allocator and is worth
restating, because it is not the linear slowdown one would guess. Scanning the
whole queue rather than stopping at its front is quadratic, and the reclamation
pass runs **inside the shard's write lock**: a queue that cannot drain makes each
checkpoint slower, which starves the writers, which stops the watermark
advancing, which is what stopped the queue draining. The loop closes on itself.
Breaking at the front removes the feedback — but only because (A) and (B) are the
two it is legal to break on.

## 8.4 Index key algebra

Chunks are indexed by

$$
\kappa(k,p) \;=\; k \cdot 2^{48} + p, \qquad k \in \mathbb{K},\ p \in \mathbb{P}.
$$

**Proposition 13 ( the key encoding is order-preserving ).** $\kappa$ is an order isomorphism from the lexicographic order
on $\mathbb{K}\times\mathbb{P}$ to the natural order on
$\{0,\dots,2^{112}-1\}$. $\square$

Hence all chunks of one key form the *contiguous* interval $[k\cdot 2^{48},\,
(k{+}1)\cdot 2^{48})$, and an ordered scan of a key's chunks is a range scan over
one contiguous key interval rather than a lookup per chunk. ( The comparison is
not literally $128$-bit at every probe: within a compressed leaf the search
compares the shared high portion once, then binary-searches the truncated
suffixes as `u64`, reconstructing whole keys only at the uncompressed width. The
order isomorphism is what makes any of those searches legitimate; it does not
dictate the word size. ) This is why a **single global** tree suffices
and a per-key chunk directory is unnecessary: a huge key is just a long cursor
walk, so the "one key's chunk list exceeds memory" problem never arises.

### How those keys are stored, which §13.1 depends on

$\kappa$ occupies $\texttt{CHUNKKEY\_BYTES} = 14$ bytes, and a leaf does not
store $14$ per entry. It stores its first key in full as a header field
$\mathrm{common}$, then each key's low $s$ bytes, for a width
$s \in \{2,4,\dots,14\}$ chosen per leaf; a read reconstructs a key by
overwriting the low $s$ bytes of $\mathrm{common}$. So an entry costs $s + 8$
bytes, the $8$ being the `ChunkRef`, and §13.1's claim that the per-entry key term
falls below the cost of naming which chunks are occupied rests on this and on
nothing else.

The scheme is sound exactly when every key in a leaf agrees with $\mathrm{common}$
above byte $14 - s$. **Two distinct obligations discharge that, and collapsing
them misplaces the guarantee.**

- **Soundness** is enforced per key, not per leaf. `push` tests
  $\mathrm{first} \gg 8s = \mathrm{key} \gg 8s$ for every entry and refuses the
  entry — sealing the leaf and opening a new one — when it fails. This does not
  rely on the keys being sorted, and it is what makes the encoding correct.
- **The width choice** is what uses the order. `choose_ksuf` inspects only the
  *first and last* key of a sorted batch. That one comparison certifies the whole
  batch because of Proposition 13: the keys are ordered as $112$-bit big-endian
  integers, so if first and last share a byte prefix $P$ then every key between
  them lies in $[P\,\texttt{00}\dots,\ P\,\texttt{FF}\dots]$ and shares $P$ too.

The second is an optimisation of the first, not a substitute for it, and the two
fail differently. A sorted key outside the chosen width returns `Ok(false)`, which
the caller reads as "seal this leaf and start another" — a wasted leaf, never a
corrupt one. An *unsorted* key is rejected earlier and harder: `push` tests
ascent before it tests width and returns `Err("leaf keys must strictly ascend")`,
aborting the build. So sortedness is a precondition of the builder, not something
the width check quietly absorbs.

---

# 9. Conditional space-amplification model

A store that never overwrites in place accumulates dead bytes, and reclaiming
them costs writes. That is a single trade with one knob — the live fraction at
which a slab is worth evacuating — and this section prices it. The result is a
closed form for both amplifications, an optimum for their product, and a
deliberate refusal to sit at that optimum: the configured threshold is well away
from the computed minimum, which §9 justifies from the flatness of the cost curve
rather than from taste. Two hypotheses are doing work throughout and are named
rather than assumed — a stationary age distribution, without which the
time-average and the ensemble average are different numbers, and a uniform death
law, which §12.9 reports the source literature abandoned.

Let $C \in (0,1)$ be the live-fraction threshold at which a slab is evacuated.
Model a slab's contents as dying independently at a constant rate, so its live
fraction at normalised age $t$ is $u(t) = e^{-t}$. Evacuation occurs at age
$T = \ln(1/C)$, when $u(T) = C$.

**Proposition 14 ( time-averaged occupancy ).** The time-averaged occupancy of a
slab over its lifetime is

$$
\bar{u} \;=\; \frac{1}{T}\int_{0}^{T} e^{-t}\,dt
\;=\; \frac{1 - C}{\ln(1/C)},
$$

$\square$

**Corollary 14.1 ( space amplification ), conditional.** *If* the population of
live slabs is in a stationary regime — slabs created at a steady rate, so that the
age of one sampled at a random instant is uniform on $[0,T]$ — then the ensemble
occupancy equals $\bar u$ and the **space amplification** is

$$
\mathrm{SA}(C) \;=\; \frac{1}{\bar{u}} \;=\; \frac{\ln(1/C)}{1-C}.
$$

$\square$

The hypothesis is not decoration and is discharged nowhere in the implementation;
the next subsection measures what happens without it.

**Proposition 14.2 ( cleaning write amplification ), death-law independent.**
Evacuating a slab whose realised live fraction is $C$ rewrites $C$ units to free
$1-C$ units, so **the local cleaning term** — extra payload written per unit of
capacity reclaimed — is $C/(1-C)$. Charging that against new logical writing under
the convention that *reclaimed capacity services an equal volume of new writes*
gives

$$
\mathrm{WA}(C) \;=\; 1 + \frac{C}{1-C} \;=\; \frac{1}{1-C}. \qquad \square
$$

**What it does and does not assume.** The local term $C/(1-C)$ is arithmetic at
the victim and carries no hypothesis about ages, deaths or stationarity — which is
why this is stated apart from Corollary 14.1 rather than under it. The step to
$\mathrm{WA}$ is not assumption-free: it is a **capacity-flow accounting
convention**, and a system that reclaims faster or slower than it consumes does
not satisfy it.

**Nor is this the system's total physical write amplification.** It prices
evacuation payload only. Copy-on-write rewriting of index nodes is documented in
`store/mod.rs` as *the dominant write cost in the system*, and superblock and
metadata writes are unpriced here as well. $\mathrm{WA}$ is the cleaning
policy's own amplification, which is the quantity Proposition 15 needs; it is not
a figure to quote for the store.

All three formulas are independent of slab and page size **within the continuum
payload model**. The realised system is coarser: a slab is finite and evacuated at
a discrete used-slot fraction crossing a strict threshold, `SLAB_META` is
reserved, and the index node size that sets the dominant write cost appears
nowhere above.

**They are also independent of the absolute death rate, which is what makes the
model have one input rather than two.** Rescale time by $\alpha > 0$, writing
$u_\alpha(t) = u(t/\alpha)$. The evacuation age moves to $T_\alpha = \alpha T$,
and

$$
\bar u_\alpha
= \frac{1}{\alpha T}\int_0^{\alpha T} u(t/\alpha)\,\mathrm{d}t
= \frac{1}{\alpha T}\cdot \alpha\int_0^{T} u(s)\,\mathrm{d}s
= \bar u .
$$

So $\mathrm{SA}$ depends only on the *shape* of $u$, never on its time scale, and
$\mathrm{WA}$ does not involve time at all. Consequently $C^*$ is a functional of
the shape alone. This is why the two-rate rows of the sensitivity table below are
well posed despite naming only a *ratio* of rates: no normalisation convention is
needed, and choosing one changes nothing. It is also why the model can be
calibrated from a decay curve's form without ever measuring how fast a database
actually ages.

**What the hypothesis costs when it fails.** $\bar u$ is a *time* average over one
slab's lifetime; the ensemble quantity is occupancy across all live slabs at one
instant, and renewal theory identifies the two only in the stationary regime.
Simulated at $C = 0.40$, where the corollary predicts
$\bar u = 0.655$ and $\mathrm{SA} = 1.53$:

| age distribution of live slabs | mean occupancy | $\mathrm{SA}$ |
|---|---:|---:|
| uniform on $[0,T]$ | 0.655 | **1.53** |
| clustered young ( after a burst of allocation ) | 0.872 | 1.15 |
| clustered old ( a quiescent database near eviction ) | 0.460 | 2.18 |

Table: Space amplification under three age distributions of live slabs. Only the first is stationary.

So the prediction is exact in the stationary regime and departs from it
otherwise. The three rows are illustrative distributions, not extrema, but the
extrema are immediate: $u$ is confined to $[C, 1]$ on $[0,T]$ by the definition of
the evacuation threshold, so **whatever** the age distribution,

$$
C \;\le\; \mathbb{E}[u] \;\le\; 1
\qquad\Longrightarrow\qquad
1 \;\le\; \mathrm{SA} \;\le\; \tfrac{1}{C},
$$

and at $C = 0.40$ that upper bound is exactly $2.50$ — attained by the degenerate
population whose slabs all sit at age $T$, some $64\%$ above the stationary
$1.53$. The model is therefore *not* unbounded over age distributions; what is
unbounded is retention driven by §8.3's reachability obligation (A), which is a different
mechanism and outside this model entirely. What the bound loses is uniformity in
$C$: $1/C$ diverges as $C \to 0$, so no constant bounds amplification across all
thresholds.

The direction is the one that matters: a database that has stopped allocating
drifts toward the old end, where amplification is worst and the stationary figure
is most optimistic. None of this weakens Proposition 14, which is about a time
average and is correct as such; it bounds what Corollary 14.1's number means.

**Proposition 15 ( the product optimum is $C \approx 0.285$ ).** Under the
hypotheses of Corollary 14.1 ( a stationary age distribution ) and Proposition
14.2 ( the capacity-flow convention ), both of which this inherits by multiplying
their conclusions, minimising
$f(C) = \mathrm{SA}\cdot\mathrm{WA} = -\ln C \,/\, (1-C)^2$:

$$
f'(C) = \frac{1}{(1-C)^3}\left[-\frac{1-C}{C} - 2\ln C\right] = 0
\iff
\frac{1}{C} - 1 = 2\ln\frac{1}{C}.
$$

With $x = 1/C$ this is $x - 1 = 2\ln x$, whose non-trivial root is $x \approx
3.513$, i.e. $C \approx 0.285$. $\square$

The implementation defines $C = 0.40$, giving $\mathrm{SA} = 1.53$ for
$\mathrm{WA} = 1.67$ — **but evacuation is disabled by default**
( $\texttt{EVACUATE\_PER\_CHECKPOINT} = 0$ ), because measured evacuation did not
pay for itself. The threshold is consulted only when a `DbOptions` field turns
cleaning on. So this section models a policy that exists and is currently
switched off, not the amplification law of a default deployment: with no
threshold cleaning, nothing evacuates at $C$ and the steady state this section
describes is never entered. The model is what the constant *would* buy, and is
the right thing to consult before enabling it or changing it. The deviation from the optimum is deliberate and follows
from the *shape* of $\mathrm{SA}$: it is flat on the low side and sharp on the
high side ( at $C=0.8$, one buys a $27\%$ space reduction for $3\times$ the
writes ), so erring low is cheap and erring high is not.

**What is not proved.** Proposition 14 is a time average over one slab's
lifetime and Corollary 14.1 an ensemble statement conditional on stationarity;
neither bounds file size, and neither says anything about the case a long-lived
reader creates. §8.3's reachability condition can hold back an unbounded number of
superseded extents, and the model has no term for it.

The implementation supplies a pressure policy that may abort the oldest reader
and invokes it from the production maintenance path. This turns the mechanism
into an operational control, but not an unconditional mathematical bound:
enforcement depends on maintenance being scheduled, on the configured policy,
and on the threshold being observed before available storage is exhausted. The
model therefore reports reachability-driven retention separately from the
stationary cleaning calculation.

**How much does the uniform-death hypothesis decide?** §12.9 records that this is
the model's weakest hypothesis and that the source literature abandoned it —
Rosenblum and Ousterhout found hot and cold data segregate, so old segments decay
slower than $e^{-t}$. That is a correct identification and it stops short of
saying what it costs. It is cheap to bound.

Note first that $\mathrm{WA} = 1/(1-C)$ is **death-law independent**: by
Proposition 14.2 it counts bytes rewritten per byte reclaimed at a threshold, and
no decay curve enters. It is not assumption-free — it keeps that proposition's
capacity-flow convention, and prices neither index nor metadata writes — but those
assumptions do not vary with $u(t)$. So the *death-law* dependence sits entirely
in $\mathrm{SA}$, and therefore in $C^*$, which is what the rest of this
subsection varies.

Two families, both containing $e^{-t}$ as a special case — a stretched
exponential $u(t) = e^{-t^k}$, and a bimodal $u(t) = f e^{-\lambda_h t} +
(1-f) e^{-\lambda_c t}$ which is LFS's finding directly:

| model | $C^*$ | penalty at $C = 0.40$ |
|---|---:|---:|
| $k = 2.0$ ( sharper than exponential ) | 0.170 | 13.5% |
| $k = 1.0$ — Proposition 15 | 0.285 | 3.7% |
| $k = 0.7$ | 0.337 | 1.2% |
| $k = 0.5$ ( heavy tail ) | 0.380 | 0.1% |
| bimodal, 90/10 at $100\times$ ( LFS-like ) | 0.32 | 2.2% |
| bimodal, 95/5 at $1000\times$ | 0.30 | 2.9% |

Table: How far the optimum moves when the death law is not exponential.

$C^*$ genuinely moves — from $0.17$ to $0.38$ across the first family — so the
optimum is a property of the death law and not of the system. But it moves
**toward** the configured $C = 0.40$ under exactly the failure LFS reports, and
the penalty that value *would* carry — were cleaning enabled — *falls* as the
model degrades in that direction,
to $2.2\%$ under the bimodal case and $0.1\%$ under a heavy tail. The worst
penalty, $13.5\%$, is at $k = 2$ — decay *sharper* than exponential, which is the
direction the literature does not report.

So §9's justification for $C = 0.40$ ( the asymmetry of $\mathrm{SA}$'s shape at
a fixed model ) is joined by a second and independent one: it is where the
optimum goes when the model fails in the way it is known to fail.

Two one-parameter families are a sensitivity analysis, not a robustness proof.
And this bounds the *threshold's* exposure only — LFS's actual remedy was
cost-benefit cleaning, which weights a segment's age and so changes **which**
slab is cleaned rather than at what fraction. Whether an age term would pay here
is untouched by the above and remains open.

---

# 10. Complexity bounds

Let $\sigma = \sigma(S)$ be chunk count, $n = |S|$ cardinality, $m \le n'$ the
supports of two operands, $B$ the total bytes.

| Operation | Cost | Source |
|---|---|---|
| $S \cap T$, materialised | $O(m\log(1+n'/m))$ prefix probes $+$ payload work on shared fibers | Thm 2, §4.3 |
| $S \setminus T$, materialised | $\Omega(\sigma(S))$ — bounded by the left operand, not by $\min$ | §12.3 |
| $S \cup T$, materialised | $O(\sigma(S)+\sigma(T))$ prefix merge, with output lower bound $\Omega(\sigma(S\cup T))$ | Thm 2 |
| $S \triangle T$, materialised | $O(\sigma(S)+\sigma(T))$ prefix merge; cancellation means no per-operand output lower bound | Thm 2 |
| $\lvert S \cap T\rvert$ | as the first row, with zero result allocation | Prop 6 |
| $\lvert S \otimes T\rvert$, other $\otimes$ | zero result allocation; traversal depends on the carrier and cached cardinalities | Prop 6, §4.2 |
| $\lvert D(k)\rvert$ | $O(\sigma)$ index entries, **no payload decode** | §4.2, Prop 13 |
| $\lvert [lo,hi) \setminus S \rvert$ | $O(\sigma)$ | §4.2 |
| $\mathbb{U}\setminus S$, eager | $\ge 2^{47}$ — *not offered* | Prop 3 |
| `.roaring` import / export | $O(\sigma)$ | §4.4 |
| Container conversion, amortised | $< 16$ bytes per mutation | Prop 5 |
| Checkpoint | $O(\text{delta})$; untouched chunks carry by reference | §8.2 |
| Space amplification, *if* threshold cleaning is enabled | $\ln(1/C)/(1-C) = 1.53$ at $C=0.4$ | Cor 14.1 |

Table: Complexity summary, with the statement each bound comes from.

---

# 11. Validation methodology and limitations

A mathematical statement is evaluated by its proof or by a counterexample,
whereas an artifact test evaluates only the implementation and inputs it reaches.
The table records the strongest available evidence for each group of claims. A
test is listed only when its failure would bear on the claim as phrased; this is
an evidence map, not a claim that testing proves the mathematics.

| Statement | Stated in | Evidence or falsification route |
|---|---|---|
| Prop 1 ( I8, the reserved ordinal ) | §2 | `i8_violations_flags_only_the_illegal_ordinal` |
| Thm 2 ( fiberwise algebra ) | §3 | `proptest_oracle` against `BTreeSet<u64>` |
| Prop 3 ( complement is dense ) | §3.1 | `unbounded_complement_counts_without_walking_the_universe` |
| Prop 4, §4.4 ( sizes, byte identity ) | §4, §4.4 | `differential` — byte-level, both directions |
| Prop 4' ( representation independence ) | §4.5 | `every_arm_agrees_with_the_generic_kernel` in `ops::{array, bitmap, run}` and `..._in_both_orders` in `ops::mixed` — every specialised arm against `generic::apply` |
| Prop 5 ( hysteresis, amortised conversion ) | §4.1 | `hysteresis_prevents_thrash_at_the_boundary`, `array_promotes_to_bitmap_at_capacity` |
| Prop 5' ( no two-cycle at fixed content ) | §4.1 | `optimize_is_content_preserving` — the content half only; see the note below |
| §4.4 ( decode is total ) | §4.4 | `fuzz/decode_container`, `deserialize_never_panics` |
| Prop 6 ( cardinality identities ) | §4.2 | `expr_equivalence`: $\lvert\cdot\rvert = \lvert\mathrm{collect}\rvert$ |
| §5 ( non-materialising walk ) | §5 | `allocation`, plus a spy leaf recording *which question* was asked |
| Prop 6' ( canonicity, conformance ) | §5.1 | `stream_conformance` — ascending, non-empty, on both production paths and both lowerings, over the `Expr` operators and the three combinators `Expr` cannot reach |
| Thm 7, §6.2 ( sound rewriting ) | §6.2 | `planning_preserves_meaning`, planned vs unplanned vs oracle |
| Prop 8 ( restriction/reassembly ) | §6.3 | set-algebra proof; the union specialization has a segmentation regression |
| Prop 9, 10 ( one-sided statistics ) | §7.1, §7.2 | `disjointness_is_claimed_only_from_a_complete_sketch` |
| Prop 11 and Cor 11.1 ( atomic visibility ) | §8.1 | concurrent snapshots during multi-shard publication |
| Prop 12 ( recovery ) | §8.2 | crash-matrix truncation at every byte offset |
| §8.3 ( immutability, reclamation ) | §8.3 | `invariants` ( I2 by reading the file back ), `zero_copy_mvcc` |
| Prop 13 ( key ordering ) | §8.4 | `chunkkey_roundtrip_and_ordering` |
| Props 14, 14.2, 15 and Cor 14.1 ( stated models ) | §9 | calculus over a hypothesised $u(t)$ — see the note below |
| Props 16, 17 ( the density gap ) | §13.2 | counting against the bound of §13.1 — see the note below |
| Props 18–21 ( the restriction algebra ) | §14 | Prop 20 clause 3 by Prop 8's test; the rest describes a construction the planner does not perform — see the note below |
| Props 22–26 ( the reinterpretation layers ) | §15 | the matrix and integer property suites, against their own oracles; Prop 26 needs a generator biased to straddling indices, which is §11's third failure mode below |
| Props 27–29 ( reductions and the adjoint triple ) | §15.6 | the shared-space property suite: each reduction against combining the constituents separately, Prop 28's one-sided law asserted **strict** as well as sound, Prop 29's unit and counit as a round trip, and the inverse image against all four binary operators |

Table: Numbered statements and the strongest evidence presently associated with them.

**Limits of the evidence map.** Some claims have no executable structural layer
because they describe conditional models or operations the implementation does
not perform. Their support is analytic rather than experimental.

**Partly guarded.** Proposition 5' — `optimize_is_content_preserving` pins the
content half, while the no-two-cycle half is a two-line arithmetic argument with
nothing to falsify it.

**True of the system, but about something it does not do.** Propositions 18, 19
and 21 and clause 2 of Proposition 20. Segmentation is applied at lowering, so a
restriction reaches the operands as a stream and never as an `Expr`, and no
rewrite ever pushes one through a $\neg$ or collapses a leaf against a window.
The identities are set algebra and hold regardless — they are as sound as
Proposition 8, which shares their proof — but there is nothing to falsify because
there is nothing that executes them. Listing them as guarded because their
*subject matter* is tested would be the mistake §11 exists to prevent, so they
are listed here instead. They are stated because §14's clause-2 hazard is a trap
laid for whoever builds a segmentation that cuts anywhere but a prefix boundary,
and because Proposition 21 case 3 identifies a statistic §7 does not supply.

**Calculus and counting over a stated model.** Propositions 14, 14.2, 15, 16 and
17 and Corollary 14.1. There is no execution that could contradict them, only a
workload that could make them irrelevant, which is what the offline
container-shape instrument measured — an instrument that has since been removed,
for reasons §13.9 records. An earlier
version of this table listed Proposition 14 as *falsified by*
`aged_space_amplification_stays_bounded`, which is structurally impossible twice
over: the proposition is an integral over a hypothesised $u(t)$, and that test runs
default options, under which threshold cleaning is **disabled** and no evacuation
occurs at all. It guards the allocator's generation behaviour, which is what its
own comment says it guards, and it can neither confirm nor refute the threshold
formula.

Proposition 6' belonged to a third class until recently and no longer does, which
is worth recording because of *why* it was invisible. It had no layer at all
until `stream_conformance` was added: nothing checked that an operator's
productions are ascending and non-empty, and nothing would have. That gap was
invisible to every other layer for a structural reason worth stating — a stream
that repeats a prefix, walks backwards or emits an empty fiber still *collects to
the right set*, because `OrdSet::from_chunks` and the operators above it absorb
the malformation. So the denotational tests pass while the object every
proposition quantifies over has been left behind.

Two failure modes recur and both are invisible to ordinary testing, which is why
the propositions above are stated as propositions rather than as comments:

- **Extensionally equal, operationally different.** The materialising and
  non-materialising cardinality walks compute the same function; only a resource
  observable separates them. The same is true of a pairwise fold versus an
  $n$-ary union, and of decoding every payload versus reading $\texttt{card\_m1}$
  from the index.

  **An allocation counter measures allocations, not waste, and the difference
  became load-bearing once containers were frozen.** A frozen container is
  reference-counted, so handing one back where a length would do costs two
  atomics and a moved enum and allocates *nothing* — invisible to a counting
  allocator. A test written in `allocation` for exactly that defect **passed with
  the fix removed**, and was deleted rather than kept. Three separate operator wastes measured
  in this system — a difference operator taking a whole chunk to read a cached
  length, and two $k$-way folds retaining chunks they had already discarded —
  were invisible to allocation counting for the same reason.
  The property is pinned behaviourally instead: a spy leaf records **which
  question** an operator asked, and the test is verified by reverting one call
  site. Copy-on-write made the fast path allocation-free and thereby blinded the
  instrument that had been watching it.

  **And sometimes no observable is left but time.** A whole-set cardinality
  summed chunk by chunk inside the leaf allocates nothing, is observed by no
  operator — the walk is over `OrdSet`'s own arrays — and returns the same
  number as the $O(1)$ form. Cost was the only thing that differed. The
  defensible way to assert on cost is **a ratio between two input sizes, never a
  threshold**: 50 000 chunks against 500, where a correct implementation is flat
  at ~$1\times$ and the walking one is $89\times$ in release and $101\times$ in
  debug, against a bound set at $10\times$. An order of magnitude clear on both
  sides is what makes a timed assertion survive a machine whose timings moved
  $2\times$ within a single run — and the ratio form is what removes the machine
  from the assertion altogether.
- **Tests that pass a sabotage check and still measure nothing.** This is the
  sharpest form and it defeats the usual remedy. The standard check on a test is
  to break the subject and confirm the test goes red. Two scenario files in this
  project passed exactly that — breaking `ops::and` did fail them — and were
  still not tests of this system, because the assertion they made was about a
  reimplementation of an operator that does not exist in `src/`, and the coupling
  to `ops::and` was incidental to it. The instrument moved when the subject was
  broken and was not measuring the subject.

  The admission test that catches it is one question, and it is stricter than
  sabotage: **does a change to the system fail this test *in the way its
  assertion is phrased*?** Both files were reclassified as prototypes rather than
  repaired.

- **Tests that cannot fail.** A property whose generator cannot reach the
  boundary — an ordinal generator topping out below $2^{25}$ for a defect at
  $2^{64}-2$ — reads as coverage and provides none. Generators here are
  boundary-biased on three axes ( cardinality, prefix pattern, and the top of the
  address space ) precisely because uniform random $u64$ values would place one
  ordinal per chunk, so arrays would never fill, bitmaps would never appear, and
  run containers would never be produced.

---

# 12. Related work and implications

This section positions the model against prior work on compressed bitmaps, set
expression evaluation, abstract statistics, and persistent storage. The review
is organized by technical claim because the system combines mechanisms developed
in several research communities. It is a contextual comparison, not a systematic
literature review, and it does not establish priority.

| topic in this article | closest context | conclusion |
|---|---|---|
| Fiberwise algebra and portable serialization | Roaring [1–3] and its format specification [47] | representation and cardinality/type convention inherited; algebra specialized here to the system |
| Array/bitmap crossover at $4096$ | Roaring's fixed array threshold [1] | exact equality under the portable payload formulas; not a universal time optimum |
| Stateful encoding hysteresis | eager conversion in CRoaring and Java Roaring | implementation-specific policy with a derived amortized bound |
| Non-materializing cardinality | cardinality kernels in Roaring implementations | established implementation technique expressed here as a valuation |
| Lazy ordered chunk streams | Volcano, Lucene iterators, factorized representations [18] | established iterator discipline specialized to chunked bitmap containers |
| Yield and cardinality cost functions | proof-versus-answer distinction [28] | specialization selected by the caller's requested result |
| Planner metadata and adaptive certificates | adaptive set algorithms [28–32] | design diagnostic valid under the stated comparison and metadata-access model |
| Occupancy abstraction | small materialized aggregates [9], zone maps, and BRIN | established synopsis interpreted through a sound abstraction |
| Bottom-$K$ sketching | distinct-value and multiset synopses [10] | established estimator family; statistical guarantees require an explicit randomized-hash model |
| Visible watermark and recovery | durable epochs [19], ARIES taxonomy [11], shadow paging [12] | known mechanisms combined under the article's publication premises |
| Reclamation of mapped zero-copy data | epochs, hazard pointers [13], and the ERA theorem [48] | the reachability, recovery, and alias obligations are necessary for this API; the particular mechanism is not uniquely forced |
| Space amplification | log-structured cleaning [14] and the RUM conjecture [15] | conditional adaptation of an established model |
| Resource-sensitive validation | empirical complexity measurement [17] | established measurement principle applied to extensionally equal implementations |
| Density gaps | succinct and adaptive set representations [23, 35, 36, 38] | quantitative case analysis; priority is not claimed |

Table: Relationship between the article's claims and the closest prior work considered.

## 12.1 The representation ( §3, §4 )

The chunk decomposition of Theorem 2 is the Roaring format, introduced by Chambi,
Lemire, Kaser and Godin [1] as an alternative to run-length schemes — WAH,
Concise, EWAH — which compress a bitmap as a sequence of fill and literal words.
The essential difference is Theorem 2 itself: RLE schemes make the *whole bitmap*
one compressed stream, so a Boolean operation must decode both streams in
lockstep and cannot skip; Roaring's product decomposition makes each fiber
independently addressable, which is what admits seeking. Run containers, the
third encoding, were added by Lemire, Ssi-Yan-Kai and Kaser [2], and the
optimized C implementation is described by Lemire et al. [3].

The value $4096$ is inherited from the portable format's non-run container
convention [47]. Within the two payload formulas used here, it is also the unique
array/bitmap equality $2|A|=8192$. Thus the same-size promotion property follows
from the format and the payload model. The calculation does not establish that
$4096$ is a universal execution-time optimum.

The hysteresis policy of §4.1 differs from the eager conversion used by CRoaring
and Java Roaring. Those implementations demote a bitmap to an array when
cardinality falls to the array threshold; the resulting in-memory kind is
canonical with respect to cardinality. Proposition 5 bounds the conversion work
saved by retaining history in the in-memory representation.

Portable serialization imposes a separate condition. For a non-run container,
the reader infers array or bitmap representation from cardinality, as specified
by the format. An implementation may therefore use a noncanonical in-memory kind
only if export restores the serialized kind expected at that cardinality. The
current exporter performs this normalisation.

An earlier exporter omitted the normalisation. A promoted-then-shrunk bitmap
could consequently produce an invalid or misinterpreted file. The defect was
reached only through an update history and was missed by state-only differential
generation; after correction, a history-sensitive byte-level regression checks
both 32-bit and 64-bit round trips. This case study supports the validation
method of §11 without treating the repaired defect as a current property of the
system.
**Proposition 6 is folklore in implementation form.** CRoaring exposes
`roaring_bitmap_and_cardinality`, `_or_cardinality`, `_xor_cardinality` and
`_andnot_cardinality`, all of which avoid materialising a result. Note that
CRoaring's separate `roaring_bitmap_lazy_or` family means something quite
different from §5 — it defers *cardinality bookkeeping* within an eager
operation, not the operation itself. What §4.2 contributes is the framing:
stating the four identities as one valuation identity shows that a single kernel
family suffices, and it turns "$|A|$ must be $O(1)$" from an implementation
preference into a proof obligation.

**Proposition 3 confirms a decision the Roaring API had already taken.** CRoaring
offers `roaring_bitmap_flip( x, range_start, range_end )` — a *range-bounded*
complement — and no unbounded one. Proposition 3 supplies the missing argument
for why: the $\ge 2^{47}$ lower bound holds independently of $|S|$ and of
$|\complement S|$. The lazy unbounded complement with $O(\sigma)$ cardinality,
via the identity in §4.2, is the part that is not in the Roaring libraries.

**The classical bitmap-index literature works the orthogonal axis, and it is
worth being clear which one this article is on.** Chan and Ioannidis [20] give a
design space for bitmap indexes, drawing a parallel to *number representation in
different bases*: how attribute values map onto bitmaps — equality, range and
interval encodings, and multi-component decompositions — trading index count
against the bitmaps a query must touch. O'Neil and Quass's variant indexes,
including bit-sliced indexes, sit in the same space.

That axis is about **which bitmaps exist**. Roaring's three containers are about
**how one bitmap is stored**. The two compose and neither subsumes the other.
yesno fixes the first axis at its simplest point — one set per key, which in that
vocabulary is equality encoding — and does all of its work on the second. A
system wanting range predicates over an ordered attribute would go back to Chan
and Ioannidis for the encoding and keep §4 for the containers; Pinot's range
bitmap index is a worked example of exactly that combination.

For the same problem — representing and intersecting sorted integer sets —
Elias-Fano encodings are the principal alternative. Vigna's quasi-succinct
indices [4] and the partitioned refinement of Ottaviano and Venturini [5] achieve
space within a small factor of the information-theoretic minimum with $O(1)$
`rank`/`select`, which Roaring does not. The trade is exactly the RUM triangle of
§12.5: Elias-Fano indexes are built once and are not updated in place, whereas
Roaring containers are mutable and $\cap$ on two containers is a word-level
operation rather than a decode. Trie-based intersectable sets [6] compare the two
families directly.

## 12.2 Lazy evaluation ( §5 )

The stream model is the Volcano open/next/close iterator, and its Boolean
specialisation is Lucene's `DocIdSetIterator` with `advance( target )`, whose
`ConjunctionDISI` leapfrogs a set of iterators by advancing the laggards to the
current lead. At the relational level the same idea is Veldhuizen's leapfrog
triejoin [7], which is worst-case optimal for conjunctive queries; §4.3's
$O(m\log(1+n/m))$ galloping bound is the same bound leapfrog triejoin relies on.

**The theory of not materialising has a name.** Olteanu and Závodný's
*factorised representations* [18] represent query results as relational-algebra
expressions built from unions, Cartesian products and singletons, using
distributivity of product over union to avoid writing out the result — with
asymptotically tight size bounds, and a `d`-representation variant that adds
sharing of repeated subexpressions.

Theorem 2 is exactly such a factorisation, and the correspondence is syntactic
rather than analogical:

$$
S \;=\; \biguplus_{p \in \operatorname{supp}(S)} \{p\} \times S_p ,
$$

a union of products with the common prefix factored out of each term. A Roaring
set *is* an f-representation of the set of its ordinals, and §5's streams are
what evaluation over a factorised representation looks like operationally — the
factored form is consumed directly and the product is never expanded.

The size-bound theory does **not** transfer. Olteanu and Závodný bound
factorisations of *join* results in terms of hypergraph width measures; here the
factorisation is fixed by the ordinal's bit layout and has one level. The
borrowing is the representation and its evaluation discipline, not the bounds.

Two further differences from the iterator literature are worth stating precisely.

**`peek` is weaker than Lucene's contract.** `DocIdSetIterator.docID()` is exact:
a document either matches or does not. §5's `peek` is a *lower bound* only,
because at chunk granularity a candidate prefix can cancel to $\emptyset$ under
$\triangle$ or $\setminus$. So yesno operates one level coarser than Lucene — on
chunks, not documents — and pays for it with a weaker invariant that the type
system cannot enforce. This is a real cost of the chunk-aligned design and the
article states it as a contract precisely because nothing else can.

**No scoring, hence no WAND.** Broder et al. [8] prune by bounding a *score*
contribution; yesno has no scores, so its pruning is purely structural
( containment, disjointness, occupancy ). The WAND family is what one would add
to make this a ranked retrieval engine, and its absence is a scope decision, not
an oversight.

## 12.3 Planning ( §6 )

Cost-directed rewriting over an algebraic term language is Volcano/Cascades
transformation-based optimization. Theorem 7's termination argument — a
well-founded weighted term measure — is standard term rewriting, and is stated
here only because the implementation also carries an iteration cap that could
otherwise be mistaken for the termination argument. It is worth noting what
the measure deliberately excludes: **the cost function plays no part in it**,
which is unusual for a cost-directed optimizer and is what makes the argument
survive the cost model being revised.

**Two cost functions.** In a Cascades-style optimizer, cost is computed per plan
against *required physical properties* ( sort order, distribution ). §6.1's
$\mathrm{Y}$ versus $\mathrm{C}$ is the same device with an unusual property: the
required property is which question the caller asks. A `Range` node costs $O(1)$
to count and $\lceil (hi-lo)/2^{16}\rceil$ to enumerate, a gap of up to $2^{48}$,
so a single cost function would misplan one of the two modes by any margin one
likes. The underlying distinction is not new — Demaine, López-Ortiz and Munro
[28] separate computing a *proof* of an answer from computing the answer itself,
noting that for unions and differences "enumerating the elements of the answer
can take more time than computing the proof". §6.1's pair is a specialisation of
that split, selected by the caller's question rather than by the operator.

**Evaluating a set expression over sorted sets is a named problem with matching
bounds.** Demaine et al. [28] give adaptive algorithms for intersection, union
and difference under an instance-dependent measure: a **partition certificate**
for a query is a partition of $[0,u)$ in which every element of the answer
appears as a singleton and every non-element lies in an interval empty in at
least one operand. Barbay and Kenyon [29] prove the resulting measure $\delta$
optimal, with

$$\Theta\!\left( \delta \sum_{i} \lg (n_i/\delta) \right)$$

comparisons necessary and sufficient. Chiniforooshan, Farzan and Mirzazadeh [30]
extend this to whole expressions — union, intersection, difference, complement
and symmetric difference, which is exactly §5's signature — with a lower bound
their algorithm meets. Their bound is stated for expressions of the form $E_1$ or
$E_1 \setminus E_2$ with $E_1, E_2$ over $\cup, \cap, \oplus$; whether nested
differences are harder is an open conjecture, so `Expr`, which nests `AndNot` and
`Not` freely, sits just outside what is proved. Bille, Pagh and Pagh [31] attack
the same expressions in the word-RAM with a cell-probe lower bound; Culpepper and
Moffat [32] give the experimental counterpart, in which plain pairwise
smallest-first is repeatedly hard to beat.

So §6 is not planning in a vacuum. There is a known optimum to be measured
against, and this system is not positioned to attain it — worst-case optimality
there counts comparisons on sorted sequences, whereas §4's containers trade
comparisons for word-parallel kernels. The *measure* transfers where the
algorithm does not.

**Certificate reasoning under the stated access model.** In the comparison model,
an adaptive executor already discovers a near-minimal partition certificate.
Precomputing the same comparisons solely to gate a rewrite cannot improve the
asymptotic comparison bound; it merely moves work earlier. Planning can still
help when metadata supplies facts outside that access model, when a rewrite
removes later work, when a certificate is reused across executions, or when
constant factors dominate. The useful criterion is therefore conditional:

> planning evidence should either be cheaper than discovering the same fact from
> the data, be reusable, or change the subsequent execution certificate.

The artifact measurement is consistent with this criterion but does not prove it
universally. Across 4096 generated expression trees, the evaluated sketch found
no disjointness fact beyond interval metadata, four containment-gated rewrites
never fired, and planning on the ragged mixed-container corpus cost between two
and twelve times the execution it was intended to reduce. These observations
apply to the tested corpora, allowances, and implementation.

**Scope.** The negative artifact result concerns these statistics as one-shot
gates on rewrite rules. It does not show that the underlying summaries are
generally unhelpful. Prefix occupancy supports the segmentation of §6.3, exact
shared-prefix scans account for observed planning wins, and fullness metadata
can drive a demand-directed aligned walk. Conversely, a full-range absorption
that deletes an operand changes all subsequent work and falls directly within
the useful side of the criterion.

The operators expose different containment information:

| operator | relation of output to inputs | planning consequence |
|---|---|---|
| $A \cap B$ | subset of both operands | the smaller or more selective support can lead evaluation |
| $A \setminus B$ | subset of $A$ only | the left support bounds enumeration; right-side metadata may eliminate probes |
| $A \cup B$ | superset of each operand | every distinct input element must appear in a materialised result |
| $A \triangle B$ | no containment relation to either operand | cancellation may make the result empty; work bounds require a verification argument, not output monotonicity |

Table: Containment information available to the planner by operator.

In particular, symmetric difference is not a superset operation:
$A\triangle A=\emptyset$. A comparison-based implementation can still require
linear worst-case work to distinguish equal from nearly equal operands, but that
is an input-verification bound rather than an output-size bound.

For a candidate optimisation with planning cost $C_p$, conditional saving $S$,
and success probability $p$, the elementary break-even condition is
$pS>C_p$. Both $S$ and $p$ depend on metadata, data distribution, reuse, and the
execution strategy; the operator alone does not determine whether the
optimisation is profitable.
**Lucene already costs its operands.** `DocIdSetIterator.cost()` and
`ScorerSupplier.cost()` are estimates in the spirit of §6.1, and
`BooleanScorerSupplier` uses clause costs to choose a scoring strategy as
`ConjunctionDISI` uses them to pick the cheapest lead iterator. So "has a cost
model" is not a differentiator. The difference is scope: Lucene's decisions are
local and made at execution time over a query whose shape is fixed, where §6
rewrites the expression algebraically before anything is opened — De Morgan in
both directions, absorption against ranges, and the segmentation of §6.3.

## 12.4 Statistics ( §7 )

**Occupancy is a zone map.** Moerkotte's small materialized aggregates [9] are
the origin; the technique now appears as zone maps ( Oracle ), min/max indexes
( Parquet, ORC ), data-skipping indices ( ClickHouse ) and BRIN ( PostgreSQL ).
§7.1's contribution is not the structure but the *framing*: stating $\alpha$ as
one half of a Galois connection in the sense of Cousot and Cousot's abstract
interpretation makes Proposition 9's one-sidedness a soundness theorem rather
than a comment, and makes explicit which direction of error is tolerable.

**The sketch is KMV, and the estimator used is not the best available.**
Bottom-$k$ / $k$-minimum-values sketches are due to Bar-Yossef et al.; Beyer,
Haas, Reinwald, Sismanis and Gemulla [10] give the analysis for distinct-value
and Jaccard estimation under multiset operations, including the **AKMV** synopsis
and an *unbiased* estimator. §7.2's $\hat{J} = b/k$ followed by the inversion
$\widehat{|X\cap Y|} = \hat{J}(n_X+n_Y)/(1+\hat{J})$ is the simple biased form.
Theta sketches ( Dasgupta et al. ) generalise the same family with rigorous
error bounds and set-operation closure. See §12.9.

The Bloom-filter refutation in §7.2 is a standard birthday-bound argument and is
included because the *design* it rules out is attractive and was actually built.

## 12.5 Storage and recovery ( §8, §9 )

**The watermark of §8.1 is Silo's durable epoch.** Tu, Zheng, Kohler, Liskov and
Madden [19] gate result release on a *prefix-closed durability watermark*: the
durable epoch $D$ is the minimum over all log tails, and every transaction with
epoch $\le D$ is known to be durably logged. Proposition 11's

$$
W \;=\; \max\{\, v : \forall u \le v,\ \mathrm{resolved}(u) \,\}
$$

is the same construction — a monotone frontier, computed as a minimum over
participants, below which everything is durable and above which nothing is
visible. The corollary about aborts is the same fact seen from the other side:
any unresolved slot pins the frontier, so Silo must close every epoch and yesno
must resolve every version.

The two differ in what the frontier is *for*. Silo's epochs are coarse and
time-based ( tens of milliseconds ), and exist to avoid per-transaction
synchronisation between cores on the read path. yesno's is per-commit-version
and exists to make a batch spanning several shards atomic. Same mechanism,
different problem — which is worth knowing, because it means the tuning
intuitions from Silo's epoch length do not carry over at all.

**§8.2 is a position in the Härder–Reuter taxonomy [11], and naming it settles
the argument.** In that vocabulary, invariants I3 and I4 make yesno **NO-STEAL**
( no effect of an unresolved transaction reaches the data file ) and **NO-FORCE**
( a commit writes the log, not the data file ). NO-STEAL removes UNDO; NO-FORCE
requires REDO. Hence "recovery is redo-only" is not a design achievement to be
tested for, it is a corollary of the buffer policy — which is exactly what
Proposition 12 says. ARIES sits at STEAL/NO-FORCE and therefore needs both.

**Shadow paging is Lorie's**, by way of System R, and copy-on-write B-trees are
Rodeh's [12]; the same combination is in WAFL, ZFS and Btrfs. The closest
single system is **LMDB**: shadow-paged COW B-tree, memory-mapped, single writer,
MVCC readers, and — the detail §8.3 shares — *two alternating meta pages* as the
entire commit protocol, with freed pages reusable only after two transactions.
`RECLAIM_CKPT_DELAY = 2` is that rule. LMDB differs in having no write-ahead log
at all: it forces the root at commit, where yesno logs and checkpoints later,
which is the NO-FORCE choice above.

**The reclamation scheme combines epoch-like reachability with explicit alias
tracking.** Obligation (A) uses a monotone checkpoint sequence per reader and
permits reuse only after live roots have passed the retirement point. This has
the shape of epoch-based reclamation, although the epoch is a checkpoint order
rather than a transaction version. Obligation (C) is discharged by tracking live
references to mapped extents. It is related to the problem addressed by hazard
pointers [13], but the implementation uses reference-counted aliases rather than
the classic publication-and-scan hazard-pointer algorithm.

The API premises make the three obligations of §8.3 necessary: captured roots,
recovery roots, and escaped buffers can each retain an extent. They do not make
this particular implementation mechanism unique. Reference-counted roots,
different ownership lifetimes, or storage that is never reused could satisfy or
avoid the same obligations. The ERA theorem [48] contextualizes trade-offs
among safe-memory-reclamation schemes; it does not establish that this combination is
forced.

**§9's model is the LFS cleaning analysis.** Rosenblum and Ousterhout's write
cost is $2/(1-u)$ [14]; the factor of two counts the read of the segment as well
as the write, so it is not in conflict with Proposition 14.2's
$\mathrm{WA} = 1/(1-C)$, which counts writes only. The same steady state under a *greedy*
victim-selection policy is analysed in the flash literature ( Bux and Iliadis;
Agarwal and Marrow give a closed form ), where the answer is materially more
complicated; yesno uses a fixed threshold, which is what makes the elementary
exponential-decay derivation apply. Athanassoulis et al.'s RUM conjecture [15]
is the frame for Proposition 15: minimising the $\mathrm{SA}\cdot\mathrm{WA}$
product is choosing a point on the read–update–memory surface.

## 12.6 Antecedents in mathematics and elsewhere

§12.1–§12.5 place the system against other systems. The structures it is built
from are older than any of them, and naming them changes the status of several
results.

**Theorem 2 is an instance of the powerset decomposition associated with
Tarski duality.** The contravariant powerset functor is an equivalence [39]
$\mathcal{P} : \mathbf{Set}^{\mathrm{op}} \to \mathbf{CABA}$, where $\mathbf{CABA}$
is complete atomic Boolean algebras **with complete Boolean homomorphisms** — the
morphisms matter, since only maps preserving arbitrary joins and meets make the
functor an equivalence. Being contravariant, it carries coproducts to products.
Writing $\mathbb{U} = \coprod_{p} \mathbb{L}_p$ for the chunk decomposition,
$\mathcal{P}(\mathbb{U}) \cong \prod_p \mathcal{P}(\mathbb{L}_p)$ is one corollary
of that equivalence, not the equivalence itself. §3 proves it by hand, which is worth doing once for
concreteness, but the content is that **any** partition of the universe induces
this decomposition: the 16-bit split is a choice of *where* to cut, never of
whether the algebra factors. What §3 must then justify is only the width.

**Proposition 6 is Möbius inversion on the Boolean lattice, and the general case
is exponential.** Cardinality is a valuation on a distributive lattice in Rota's [40]
sense, and inclusion–exclusion is the Möbius function
$\mu(B,A) = (-1)^{|A|-|B|}$ of that lattice. For $m$ operands the Venn vector has
$2^m$ regions and one constraint, and Möbius inversion recovers each region from
the $2^m-1$ intersection cardinalities $\lvert \bigcap_{i \in S} A_i \rvert$ over
non-empty $S$:

$$
\Big\lvert \text{exactly } T \Big\rvert \;=\; \sum_{S \supseteq T} (-1)^{|S|-|T|}
\Big\lvert \bigcap_{i \in S} A_i \Big\rvert .
$$

Proposition 6 is the $m = 2$ instance, where $2^m - 1 = 3$ — the triple
$(|A|, |B|, |A \cap B|)$.

This bounds §4.2's engineering claim, and more sharply than an earlier draft
said. That draft claimed the *binary* `and_cardinality` still suffices at every
$m$, only more often. It does not. The inversion needs the **higher-order**
intersection cardinalities $\lvert \bigcap_{i\in S} A_i \rvert$ for every
non-empty $S$, and pairwise counts do not determine them: on
$A = \{1,2,3,4\}$, $B = \{1,2,5,6\}$ and either $C = \{1,2,7,8\}$ or
$C' = \{1,5,3,9\}$, all singleton and pairwise cardinalities agree while the
triple intersection is $2$ in one case and $1$ in the other. A $k$-fold
intersection cardinality is not recoverable from binary ones and needs either a
$k$-way kernel or materialisation.

So the identity route costs $2^m-1$ *distinct higher-order* quantities, not
$2^m-1$ calls to an existing kernel, which is worse than the earlier claim in
both respects. **That is the reason §5 exists.** A streaming evaluation counts an
arbitrary $\varphi$ in one pass over the operands, and against an alternative
that is exponential in the number of operands *and* requires kernels the system
does not have, it is not one option among two.

**§7.1's abstraction is an adjunction.** A Galois connection between posets is an
adjunction between the corresponding categories, so Proposition 9's one-sidedness
— $\alpha$ may over-approximate but never under-approximate — is the unit/counit
inequality rather than a convention. Cousot and Cousot's abstract interpretation
is the computing name for the same structure.

### Beyond mathematics

**§4.1 implements a named object: the non-ideal relay hysteron.** In the
Krasnosel'skii–Pokrovskii theory of hysteresis operators [41], the elementary operator
$h_{\alpha\beta}$ is a *delayed relay* with an up-threshold $\alpha$, a
down-threshold $\beta < \alpha$, and binary output. §4.1's encoding rule is
$h_{\alpha\beta}$ with $\alpha = 4096$, $\beta = 3584$ and output
$\{\text{array}, \text{bitmap}\}$ — not an analogy but the same object. Two
things follow. The property §4.1 depends on has a name: the operator is
**rate-independent**, so the encoding depends on the *path* of $|A|$ and not on
how fast it moves, which is exactly why the rule cannot be stated as a function
of the current state. The Preisach model is a weighted superposition of hysterons
over $\{(\alpha,\beta) : \alpha \ge \beta\}$, and the array/bitmap pair is one
such hysteron. Whether the three-encoding ladder together with the $7/8$ rule
forms a Preisach *superposition* is **not established here**: that would require
the ladder to decompose as a weighted family of relays over that plane, and the
$7/8$ rule compares encoded sizes rather than thresholding $|A|$. The
identification is exact for the pair and speculative for the ladder.

**And `optimize` resembles model selection.** Choosing among three encodings to
minimise stored bytes has the shape of a two-part code in Rissanen's sense [43] —
a model index plus the data given the model. The resemblance is an analogy and
is left as one: §4.1's $7/8$ rule is a *relative-improvement* threshold, and it is
not derived here from a two-part code length, because doing so would require
pricing the model index and the switching cost in the same bits as the payload,
and neither is quantified. What survives without the derivation is the mapping
that connects §4 to §13 — the counting bound of §13.1 is the *data* term and the
container kind is the *model* term, so the density gap measures what the
three-model family costs against an ideal coder.

### Three frames that changed results rather than renaming them

The connections above rename results. Three more corrected them, which is the
better test of whether a frame is worth importing. Two are recorded where the
statements they corrected live, since a finding belongs beside its proposition
and not in a survey:

- **Rewriting theory** supplied the question Theorem 7 had not asked — whether
  the normal form is *unique*. It is not: the Definition rule and the
  Range-algebra split overlap on $R_1 \setminus R_2$ and reach distinct normal
  forms. Termination comes from Theorem 7, not from the lemma; with it in hand,
  Newman [44] runs backwards from non-confluence to give **not locally
  confluent**. The same critical pair falsified the theorem's original exception
  clause. Both now stated in §6.2.
- **Ergodic theory** [46] supplied the hypothesis Proposition 14 was using
  silently: $\bar u$ is a time average and $\mathrm{SA}$ an ensemble quantity,
  and equating them needs a stationary age distribution. That hypothesis is now
  the dividing line between Proposition 14 and Corollary 14.1 rather than a
  remark beside them, and §9 measures what it costs. The same frame reads §4.1's hysteron as a dynamical system
  with memory, and §6's fixpoint as one whose non-confluence means more than one
  attractor.

The third has no home elsewhere in the article, so it is given here in full.

**Ultrametrics, and why the $p$-adic metric is the wrong one.** The chunk
decomposition is a ball structure for the *longest-common-prefix* ultrametric on
$\{0,1\}^{64}$, and §13.6's binary trie is its ball hierarchy. It is tempting to
call this $p$-adic, and that would be exactly wrong: the $2$-adic absolute value
$|x-y|_2 = 2^{-v_2(x-y)}$ measures agreement in the **low** bits, where chunking
groups by the **high** ones. The two ultrametrics are related by bit reversal,
and the difference is not decorative — it is the assumption the whole
representation rests on.

Roaring's chunking is a bet that data is clustered in the high bits. The
transposed bet is that it is clustered in the low bits, i.e. concentrated on a
few residues mod $2^{16}$, and each is the other's worst case. For the
arithmetic progression $\{\,i\cdot 2^{16} + r\,\}$ with $10^5$ terms:

| grouping | groups | ordinals per group |
|---|---:|---:|
| by high 16 bits ( chunks ) | 100 000 | 1 |
| by low 16 bits ( $2$-adic [45] balls ) | 1 | 100 000 |

Table: The arithmetic progression that is the worst case for chunking and the ideal case for its transpose.

That is the degenerate case of §4 — one ordinal per chunk, every container a
singleton, maximum per-ordinal overhead — and it is the *ideal* case under the
$p$-adic grouping. Nothing here argues for changing the decomposition: the bet on
high-bit locality is right for identifier and timestamp workloads, which is what
the system is for. What the frame supplies is the precise statement of what the
bet is, and a name for the workload that defeats it.

## 12.7 Formal treatment, and how correctness is established

Machine-checked work on closely adjacent structures does exist. Affeldt et al.
[16] verify the tree algorithms underlying succinct data structures in Coq —
`rank` and `select` over bit sequences, the operations §4.2 requires to be
mutually inverse. The `hs-to-coq` project has verified subsets of Haskell's
`containers`, including `IntSet`, which is a Patricia-trie integer set: a
different point in the same design space as an ordinal set. Verified file
systems, which is what §8 is a special case of, are an established line of work.

The portable Roaring specification [47] provides the normative serialized
layout, including the cardinality rule used to distinguish non-run array and bitmap
containers. It does not specify this system's stateful hysteresis, lazy expression
language, or persistence protocol. The present model is therefore a
system-specific formalization built on the standard format, not a replacement
for that specification and not a claim that Roaring lacks one.

The historical export defect illustrates a gap between state coverage and trace
coverage. Byte-level differential tests over isolated states did not exercise a
promotion followed by sufficient removals; a history-sensitive regression now
does. The lesson is methodological: when representation depends on history,
interoperability validation must generate histories as well as denotations.

### Resource observables as test oracles

Section 11 argues that output-only semantic oracles cannot separate two
extensionally equal implementations; a resource or interaction observable is
required. That framing is not new. Goldsmith, Aiken and Wilkerson [17] measure
**empirical computational complexity** by fitting basic-block execution counts to
linear and power-law models across workloads spanning orders of magnitude, and
comparing the fit against the programmer's expected asymptotic bound — which
found real performance bugs. Performance regression *unit* testing is likewise an
established practice with its own case-study literature.

The crash-testing layer has a direct ancestor too. Zheng et al. [21] subjected
eight production databases to simulated power faults and found ACID violations in
every one, including commercial systems — which is the empirical case for §11's
`crash_matrix` existing at all. The methodological difference runs the opposite
way from the one above: they inject faults at realistic scale and must therefore
*sample*, while `crash_matrix` is exhaustive over every byte offset of the log
and both superblock slots. Exhaustiveness is affordable here only because the
commit protocol is two alternating slots and a redo-only log — the same
smallness that Proposition 12 depends on. A system with ARIES-shaped recovery
could not be tested this way, and would need their approach.

So the allocation-budget technique is prior art, and the honest narrowing is
this. TrendProf infers
an exponent and compares it to an expectation, which requires a workload *series*
and yields a statistical answer. §11's budgets are constants asserted at a single
workload — "this operation allocates at most $N$ regardless of chunk count" —
which is a weaker claim, cheaper to run, and deterministic enough to gate a
merge. What is specific to this design is not the tool but the *necessity*: here
the two implementations are a materialising and a non-materialising walk of the
same identity, so they agree on every output for every input. No output-only
semantic oracle or differential comparison can separate them, because there is
no denotational disagreement to observe. That is a statement about §4.2's
identities, not a new testing method, and §11 should be read that way.

## 12.8 The closest systems

§12.1–§12.7 compare claim by claim. This table compares system by system, and it
is included with a warning about how to read it: no column here is a
differentiator, and several of them are filled in by systems that do that one
thing better than yesno does. What the table is for is the shape of the last row
taken whole.

| | data model | encoding | persistence | isolation | boolean evaluation | count without materialising |
|---|---|---|---|---|---|---|
| **CRoaring / roaring-rs** | one bitmap, $2^{32}$ or $2^{64}$ | Roaring, spec bytes | serialize / deserialize only | none | eager, in memory | yes, `*_cardinality` |
| **Pilosa v1** | key $\to$ bitmap, $2^{64}$ | Roaring-*like*, **not** spec-compatible | mmap file + op-log append, periodic full snapshot rewrite | none ( snapshot rewrite ) | eager | — |
| **FeatureBase RBF** | key $\to$ bitmap | Roaring containers in 8 KiB pages | COW B-tree + WAL, full ACID | MVCC transactions | eager | — |
| **Lucene / Tantivy** | term $\to$ doc ids | postings, skip lists, roaring doc-id sets | immutable segments, merge | segment-level | lazy, `advance`, leapfrog | via `cost()`/`docFreq` |
| **LMDB** | key $\to$ bytes | opaque | shadow-paged COW B-tree, mmap, no WAL | MVCC, single writer | — | — |
| **yesno** | key $\to$ ordinal set, $2^{64}$ | Roaring, spec bytes | COW B-tree + WAL + mmap extents | MVCC snapshots | **lazy, cost-planned** | **yes, at every level** |

Table: The closest systems, by capability. The last row is a claim about the combination, not about any column.

Read the last row as a claim about the *combination*, not about any column. Every
individual capability exists elsewhere. What is not found elsewhere
is a system that is simultaneously byte-compatible with the Roaring format,
zero-copy over an mmap, MVCC-versioned, and evaluates Boolean expressions lazily
under a cost-based rewriter.

Two honesty notes on that row. "Cost-planned" means *algebraic rewriting
before opening* — Lucene costs its operands too, as §12.2 records, and calling
this a differentiator without that qualification would be misleading. And the
analytical engines were checked as a family rather than one by one: Druid, Pinot,
Doris, StarRocks and ClickHouse all use Roaring for index filters, and all of
them combine filters **eagerly** into a new bitmap and store their data in
**immutable segments**. Immutability is the important half — it dissolves the
reclamation problem of §8.3 rather than solving it, because nothing is ever
superseded in place. A system that never mutates does not need the three
conditions. That makes them a different design rather than a nearer competitor,
but the reader should know they were checked at family granularity and not
individually.

Two caveats stated plainly. Pilosa v1 already had 64-bit keys, mmap'd containers
and pointers taken directly into the mapping — the zero-copy property of (R3) is
its idea, not this project's; it gave up spec compatibility to get 64-bit keys,
which is precisely the trade §4.4 declines to make. And FeatureBase's RBF is
close enough that it is worth noting a granted US patent, 11,886,411, *"Data
storage using roaring binary-tree format"*, describes storing Roaring containers
in a B-tree whose updated pages are written to a write-ahead log and later folded
into the data file. This is a factual pointer, not legal advice, and this article is not
qualified to give the latter — but a project storing Roaring containers in a
COW B-tree with a WAL should have someone qualified read it.

## 12.9 Implementation implications and open questions

The comparison yields four concrete implications.

**Serialization canonicalization (resolved).** Portable non-run containers [47]
encode no independent array/bitmap kind; the kind is inferred from cardinality at the
$4096$ boundary. In-memory hysteresis may retain a bitmap below that threshold,
but export must canonicalize it. An earlier exporter failed to do so. The current
exporter normalises the representation, and a promoted-then-shrunk regression
checks byte-level interoperability for both 32-bit and 64-bit forms. The
methodological result is that state-only generators are insufficient for
history-dependent representations.

**Bottom-$K$ estimation (open).** Under independent uniformly distributed hashes,
order-statistic estimators such as

$$
\widehat{D}=\frac{k-1}{U_{(k)}}
$$

provide an unbiased distinct-count estimate, and multiset extensions can estimate
intersection size directly [10]. Such a guarantee is probabilistic: a fixed
deterministic permutation does not by itself provide a sampling distribution over
adversarial inputs. Replacing the present planning heuristic therefore requires
an explicit random-seed or workload model and an evaluation of variance as well
as bias. Because the estimate affects cost selection rather than semantic
correctness, this remains an optimisation question.

**Cleaning policy (conditional).** The model of §9 assumes uniform independent
death, whereas log-structured-file-system measurements show hot/cold segregation
and motivate age-sensitive cleaning [14]. The derived formula is valid under its
stated hypothesis, and §9 bounds sensitivity to alternative decay laws. Threshold
evacuation is disabled by default in the evaluated artifact, so the first
experimental question is whether evacuation should be enabled; only then does
the choice between threshold and age-sensitive policies become operational.

**Encoding alternatives (design fork).** Partitioned Elias-Fano [5] and related
succinct structures can improve space in sparse immutable regimes, but adopting
them would abandon direct portable-Roaring byte identity. This is a change of
interoperability contract, not a drop-in container optimisation.

# Conclusion and limitations

This article presents a unified model for a persistent Roaring-style set store.
The chunk decomposition reduces Boolean operations to independent finite fibres;
canonical ordered streams lift those operations to lazy evaluation; and
cardinality identities permit result-size queries without result materialization.
The rewrite system is semantics-preserving and terminating under a
source-audited weighted measure.

The persistence model separates transaction visibility from physical
reachability. Prefix-closed visibility is atomic only under explicit publication
and snapshot-consistency premises. Zero-copy mapped buffers introduce three
independent reclamation obligations: no captured root, recovery root, or live
alias may retain an extent when it is reused. The checkpoint-sequence
counterexample demonstrates why transaction version alone is not a sound proxy
for root reachability.

The quantitative conclusions have deliberately narrower scope. Representation
crossovers follow from portable payload formulas, while transition policy also
depends on runtime cost. Space-amplification results are conditional on their
decay and cleaning assumptions. Planner measurements characterize the evaluated
corpora and budgets; comparison-model reasoning does not establish a universal
law about all planners or metadata.

Artifact evidence complements these arguments without replacing proof. The most
instructive validation result was a repaired serialization defect reachable only
through update history. Its regression illustrates the need to generate
histories when representation depends on history and to validate exported bytes
against an independent implementation.

The remaining open questions are empirical and contractual: whether randomized
bottom-$K$ estimation improves planning enough to justify its assumptions,
whether cleaning should be enabled and under which workload model, and whether a
denser encoding is worth abandoning direct Roaring interoperability. The model
is intended to make these premises and trade-offs explicit, not to establish
mechanized correctness or exhaustive novelty.

# 13. Appendix A: Density gaps

Section 4 compares the three container encodings and derives the portable-format
crossover. It does not establish that this set of encodings is adequate. This
appendix asks how far a chunk can remain from an information-theoretic bound, finds two
distinct gaps, and reports which known representation closes each.

## 13.1 The bound

To say a container is wasteful, one needs something to be wasteful *against*.
The reference used here is a counting bound: a code that distinguishes all
subsets of a given size cannot be shorter than the logarithm of how many there
are. It assumes nothing about the data and is attainable by no useful structure,
which makes it a floor rather than a target — and that is exactly what is wanted,
because the question is how much room exists at all before it is worth asking who
could occupy it. The bound also prices only the payload, so §13.1 ends by adding
the per-chunk index cost that the counting argument cannot see.

For $A \subseteq \mathbb{L}$ with $|A| = m$ and $n = |\mathbb{L}| = 65536$, any
encoding that distinguishes all $m$-subsets needs at least

$$
\mathcal{H}(m) \;=\; \log_2 \binom{n}{m} \ \text{bits}
$$

in the worst case. For a fixed-length code the argument is immediate: there are
$\binom{n}{m}$ subsets to distinguish, so some codeword has length at least
$\lceil \mathcal{H}(m) \rceil$.

The *average* statement needs a little more care, because a variable-length code
may assign short strings to everything at once — with $N = 3$ objects, all three
fit in strings of length $\le 1 < \log_2 3$. What rules that out is unique
decodability. Any uniquely decodable binary code satisfies Kraft's inequality
$\sum_A 2^{-\ell(A)} \le 1$, and under the uniform distribution on $m$-subsets
this gives $\mathbb{E}[\ell] \ge \mathcal{H}(m)$, with the accompanying
incompressibility statement: at most a $2^{-d}$ fraction of $m$-subsets admit a
codeword shorter than $\mathcal{H}(m) - d$ bits. Both the distribution and the
decodability requirement are hypotheses, not decoration — §13.7 returns to what
the uniform model decides.

That is why the appendix measures against $\mathcal{H}$ rather than against any
particular rival encoder: a scheme can beat $\mathcal{H}$ on structured inputs,
and none can beat it on average.

Against that, the three containers cost $2m$, $8192$ and $2 + 4r(A)$ bytes.

**The table below is a plug-in model, not an exact expectation.** For a uniformly
distributed $A$ the expected run count is $\mathbb{E}[r] = m(n-m+1)/n$, and the
table substitutes that mean into the cost before taking the minimum — it reports
$\min\big(2m,\,8192,\,2+4\,\mathbb{E}[r]\big)$, whereas the true expected Roaring
cost is $\mathbb{E}\big[\min(2m,\,8192,\,2+4r)\big]$. The minimum is concave in
$r$, so these differ. At the dense peak $m = 63\,422$ the plug-in value is
$8189.1$ bytes against an exact expectation of $8177.8$ — a gap of $0.14\%$,
which is why the model is kept: the extrema of Proposition 16 reproduce under it
and the labelling is what needed correcting, not the conclusions.

| $m$ | density | $\mathcal{H}$ | array | bitmap | run | Roaring | Elias-Fano | R/$\mathcal{H}$ | EF/$\mathcal{H}$ |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 256 | 0.39% | 301 | 512 | 8192 | 1022 | **512** | 320 | 1.70 | 1.06 |
| 1024 | 1.56% | 950 | 2048 | 8192 | 4034 | **2048** | 1024 | 2.15 | 1.08 |
| 2048 | 3.12% | 1643 | 4096 | 8192 | 7938 | **4096** | 1792 | 2.49 | 1.09 |
| **4096** | **6.25%** | **2762** | 8192 | 8192 | 15362 | **8192** | 3072 | **2.97** | 1.11 |
| 6144 | 9.38% | 3676 | 12288 | 8192 | 22274 | **8192** | 4096 | 2.23 | 1.11 |
| 8192 | 12.5% | 4452 | 16384 | 8192 | 28674 | **8192** | 5120 | 1.84 | 1.15 |
| 16384 | 25.0% | 6645 | 32768 | 8192 | 49155 | **8192** | 8192 | 1.23 | 1.23 |
| 32768 | 50.0% | 8191 | 65536 | 8192 | 65540 | **8192** | 12288 | 1.00 | 1.50 |
| 61440 | 93.75% | 2762 | 122880 | 8192 | 15366 | **8192** | 15872 | **2.97** | 5.75 |
| **63422** | **96.77%** | **1683** | 126844 | 8192 | 8189 | **8189** | 16120 | **4.86** | 9.58 |
| 65280 | 99.61% | 301 | 130560 | 8192 | 1026 | **1026** | 16352 | 3.40 | 54.25 |

Table: Encoded size against the counting bound, by fiber cardinality.

All figures in bytes. Elias-Fano is $m\lfloor\log_2(n/m)\rfloor + m + n/2^{\lfloor\log_2(n/m)\rfloor}$ bits, before select support.

**The run container is what makes the dense end survivable.** An analysis that
omits it mis-locates the problem entirely, concluding that the worst case is a
nearly-full chunk — when such a chunk has few runs and encodes in a few bytes.

### What the bound does not price

$\mathcal{H}(m)$ prices the *contents* of one chunk given that the chunk is
known. A stored set also has to say *which* chunks are occupied, and the ratios
above do not include that term. The omission is not uniform, and it is total at
one end: the table starts at $m = 256$, but for $m \le \texttt{INLINE\_MAX} = 3$
the container lives inside the index word and no payload extent is allocated at
all. For those chunks the ratio column is not approximate, it is empty — every
stored byte is index.

The two terms admit an honest comparison. Naming an occupied set of $\sigma$
prefixes out of $2^{48}$ costs $\log_2 \binom{2^{48}}{\sigma}$ bits, or about
$48 - \log_2 \sigma + \log_2 e$ bits each. Against that, a leaf entry is
$\texttt{ksuf\_len} + 8$ bytes, and the narrowest legal suffix width is $2$, so
$10$ bytes per occupied chunk is a floor — independent of node fill, which can
only make it worse.

| $\sigma$ | naming bound, B/chunk | index floor, B/chunk | ratio at least |
|---:|---:|---:|---:|
| $10$ | 5.70 | 10 | 1.8 |
| $10^3$ | 4.93 | 10 | 2.0 |
| $10^5$ | 4.10 | 10 | 2.4 |
| $10^7$ | 3.27 | 10 | 3.1 |
| $2^{30}$ | 2.43 | 10 | 4.1 |

Table: The per-chunk index cost, which the counting bound does not price.

The gap widens with $\sigma$ because the index spends a fixed width per chunk
while the bound spends less per chunk as the occupied set grows denser in
$2^{48}$.

**Within the floor, the per-entry key is not where the excess is.** Of the $10$
bytes, $2$ are the truncated key suffix and $8$ are the `ChunkRef`. The suffix is
*below* the naming bound at every $\sigma$ in the table: `choose_ksuf` retains
only the bytes in which the keys of a leaf actually differ, which is the same
redundancy the $\log_2 \sigma$ term removes.

That does **not** license the stronger claim that the total key cost is below
the bound, and the stronger claim would be wrong. Each leaf also stores a full
$14$-byte common key in its header, leaf occupancy depends on fill so the trailing
slack is unmodelled, and internal nodes carry separator keys of
$\texttt{CHUNKKEY\_BYTES} + 4$ bytes each. None of those three is quantified here.
What can be said is bounded and is stated as such: the $10$-byte per-entry floor
splits $8$ to the reference and $2$ to the suffix, and the total index-versus-bound
excess cannot be apportioned between key and reference without measuring the
overheads above. §13.9 is where that measurement belongs.

The reference is not slack. In its out-of-line form $58$ of its $64$ bits are
occupied, by a $40$-bit cell offset, a $16$-bit cardinality and a $2$-bit kind.
( The inline form of the same word is laid out differently — up to three $16$-bit
ordinals, a kind, an inline flag and a $2$-bit count — which is the form that
matters in the $m \le 3$ regime this section opened with. ) What the cardinality
and kind fields buy is the subject of the rest of this article. Because
cardinality and kind sit in the index, `cardinality()` and the `Full` test are
$O(1)$ without touching a payload byte, which is what makes the counting path of
§10 non-materialising, `cardinality_hint` answerable in §5, and the metadata
criterion of §12.3 satisfiable at all: a planner can only decide from metadata
what metadata has been paid for. The counting bound prices *storing* a set; this
term buys *deciding about* a set without decoding it, and no bound on the former
charges for the latter.

So the ratios in this appendix are payload ratios, and should be read as such.
The consequence for §13.9 is direct: a histogram of stored encodings measures
the term this appendix models, and in the sparse regime that is a minority of
the bytes and eventually none of them.

## 13.2 Two gaps, and an asymmetry

**Proposition 16 ( where Roaring is worst ).** Under the uniform model, the ratio
of Roaring's cost to $\mathcal{H}(m)$ has exactly **two interior local maxima**:
one of $2.97$ at $m = \texttt{ARRAY\_MAX} = 4096$, and one of $4.86$ at
$m = 63\,422$ ( $96.77\%$ ). The first is the global maximum over the sparse half
$m \le n/2$. The ratio then dips to a local minimum at $m = 65\,522$ and rises
monotonically to a **boundary** maximum of $5.00$ at $m = n-1$, where a
single absent ordinal costs a $10$-byte run container against a $2$-byte
bound. $\square$

That coincidence is not one. Proposition 4 fixes $4096$ as the point where the
array and bitmap costs are *equal*; the two cost curves cross there, and their
crossing point is where the smaller of them is furthest above a concave bound
that peaks at $m = n/2$. **`ARRAY_MAX` is optimal for switching representations
and pessimal for compression, and it is the same number for the same reason.**

**Proposition 17 ( the bound is complement-symmetric; Roaring is not ).**
$\binom{n}{m} = \binom{n}{n-m}$, so $\mathcal{H}(m) = \mathcal{H}(n-m)$: a chunk
and its complement are equally hard to describe. Roaring's encodings do not
respect this. The array container exploits sparsity at the low end and has **no
counterpart at the high end** — the cost of storing $n-m$ absences as an array,
$2(n-m)$ bytes, beats the bitmap for all $m > n - 4096 = 61440$, and no container
does it. $\square$

So there are two gaps, not one, and they have different characters:

- **The middle-sparse band**, roughly $1\%$ to $15\%$ density, peaking at $2.97
  \times$ at exactly $6.25\%$. Nothing in the container set is near-optimal here;
  arrays have become expensive and the bitmap is flat.
- **The scattered-dense band**, peaking at $4.86\times$ at $96.77\%$. Here the
  run container *does* rescue clustered gaps — that is why the table's last row
  costs 1026 and not 8192 — but it fails when the gaps are scattered, and a
  complement array would cost $512$ where the run costs $1026$ and the bound is
  $301$.

The dense gap is **by far the cheaper fix**. A complement array is the existing
array container with the sense of membership inverted, so cardinality stays
$O(1)$ and the payload stays a sorted `u16` sequence — the *mathematics* is a
relabelling. The implementation surface is larger than "one new kernel arm"
suggests, and saying so is fairer to whoever would build it: a fourth kind needs
a discriminant the on-disk `ChunkRef` has no spare value for, a decode path,
promotion and demotion rules against the other three, and format work throughout.
The kernel arithmetic is the part most easily overstated. Three kinds give $9$
ordered kind-pairs per operator, which is the $36$ arms `ARCHITECTURE.md` counts
across four operators; a fourth kind gives $16$ pairs and $64$ arms, so the *new*
arms number $28$. And none of them is obligatory for correctness — not as a fact
about the current code but by Proposition 4': every arm computes the same function
on $\mathcal{P}(\mathbb{L})$, so the generic iterator kernel is a complete
implementation of all of them and specialisation is an economic decision taken
pair by pair. Smaller than a new algebra, larger than
a patch. The hybrid-bitvector literature calls this **minority-position
encoding** and treats it as basic; Roaring does not have it.

It is also worth more than its size suggests. Adding a complement
array drops the dense peak from $4.86\times$ to $2.51\times$ — a saving of
$3\,962$ bytes at the worst chunk — and closes the boundary spike outright: at
$m = n-1$ a complement array stores the single absent ordinal in $2$ bytes,
exactly the bound. Since $2.51 < 2.97$, that makes $\texttt{ARRAY\_MAX}$ the
**global** worst case rather than merely the worst of the sparse half. That is
the return on the work priced above: it converts Proposition 16 from a statement
about three maxima into a statement about one.

## 13.3 Candidates

Both gaps of §13.2 are closable, and by known structures — that is not the
difficulty. The difficulty is that this system's containers must be *mutated in
place* and must *serialize to Roaring's bytes*, and most of the structures that
close a density gap give up one or both. The last two columns are therefore the
ones that decide, not the third.

| representation | target band | cost vs $\mathcal{H}$ | usable in place? | mutable? |
|---|---|---|---|---|
| **Complement array** | $> 93.75\%$ | ~1.7× | yes, trivially | yes |
| **Elias-Fano** ( `sd_vector` ) | 0.1%–25% | **1.05–1.23×** | `select` / gallop only | no |
| **RRR** ( $H_0$-compressed ) | medium–dense | near $nH_0$ | rank/select only | no |
| **Per-sub-block hybrid** | all | close throughout | per block | rebuild |
| **Golomb-Rice on gaps** | uniform sparse | near-optimal | sequential only | no |
| **Compressed binary trie** ( rTrie ) | all | see §13.6 | intersection natively | not as published |
| ~~Tree-Encoded Bitmaps~~ | — | — | — | — |

Table: Candidate representations, by the band each targets and what it costs elsewhere.

**Elias-Fano is the strongest candidate for the middle band** and the size case is
not marginal: $3072$ bytes against $8192$ at the crossover, a $2.7\times$
reduction, landing within $11\%$ of the information-theoretic bound. It supports
`select` in constant time, so leapfrogging — the operation §4.3 says the design
is *for* — works natively. This is exactly Vigna's quasi-succinct index [4]
applied one level down.

**Tree-Encoded Bitmaps [22] are ruled out, and it is worth recording why**, so
that nobody repeats the search. TEB encodes runs through a binary tree and is
strong when bitmaps are dense *and* clustered; its own evaluation reports that
Roaring dominates below a clustering factor of about 16, that TEB needs $f > 128$
to win, and that TEB *falls behind* Roaring at low bit densities because sparse
trees become deep and imbalanced. The gap identified here is at medium density
and **no** clustering — precisely the region TEB does not target. It is the most
recent serious work in this area and it is aimed elsewhere.

The per-sub-block hybrid [23] is the most general answer: partition the chunk and
choose per sub-block among minority-positions, run-length and plain encodings.
That is Roaring's own thesis applied one level down, and it subsumes the
complement-array idea as a special case.

## 13.4 Entropy coding proper, and why it blocks

§13.1's bound is a counting argument, so it is worth asking what actually attains
it. The answer separates cleanly into two families, and only one of them is
usable here.

**Arithmetic coding attains the bound and destroys addressing.** A binary
arithmetic coder driven by the density $p$ emits $nH(p) + O(1)$ bits, which is
the bound to within a couple of bytes. It does so by making the entire chunk one
indivisible number: bit $i$ of the input has no position in the output, so
`rank`, `select` and `contains` are all "decode from the start", and a Boolean
kernel over two such chunks has no word-parallel form whatsoever. The literature
recognises this as the central difficulty — there is a DCC paper devoted to
random-access decompression under binary arithmetic coding [24] — and modern
ANS [25] improves matters only in that encoder and decoder share one state, so a
seek needs the pair ( bit offset, decoder state ) rather than being impossible.
Neither gets to word-parallel operations.

**Enumerative coding attains the bound *and* is addressable.** Cover's
enumerative source coding [26] indexes a sequence by its rank in the lexicographic
order of the set it belongs to. For our problem the set is the $m$-subsets of
$\mathbb{L}$, so the code word is an integer in $[0, \binom{n}{m})$ and the code
length is exactly

$$
\big\lceil \log_2 \tbinom{n}{m} \big\rceil \ \text{bits} \;=\; \lceil \mathcal{H}(m) \rceil ,
$$

not asymptotically but by construction. The combinatorial number system gives
both directions in $O(m)$ binomial operations.

 **But at chunk scale the code word is a bignum.** At $m = 4096$ the index is a
$22\,105$-bit integer — encoding and decoding become arithmetic on 2.7 KB
numbers. That, not decode speed, is the first reason the technique must be
*blocked*.

**Blocking it gives RRR.** Split the chunk into blocks of $t$ bits and store each
as a pair ( *class* $k_i$ = the block's popcount, *offset* = its enumerative index
within that class ). This is Raman, Raman and Rao's succinct indexable dictionary
[23], and it supports `rank` and `select` on the compressed form directly.

**The block size is fixed by machine-word arithmetic, not by taste.** The widest
offset a block can produce is $\log_2\binom{t}{t/2}$:

| $t$ | widest offset | machine words | total vs bound at 6.25% |
|---:|---:|---:|---:|
| 32 | 29.2 bits | 1 | 1.35 |
| **64** | **60.7 bits** | **1** | **1.21** |
| 128 | 124.2 bits | 2 | 1.12 |
| 256 | 251.7 bits | 4 | 1.07 |

Table: Block width for a succinct indexable dictionary, fixed by machine-word arithmetic.

The largest unrestricted block whose worst-case offset fits in a `u64` is
$t=67$: $\log_2\binom{67}{33}\approx63.63$, whereas
$\log_2\binom{68}{34}\approx64.63$. Among power-of-two widths, $t=64$ is the
largest single-word choice and aligns naturally with machine words. Larger
power-of-two blocks require multi-word arithmetic.

**Where that lands against the alternatives**, computed exactly rather than
asymptotically. §13 fixes a uniformly random $m$-subset of a chunk, so the
popcount of a $t$-bit block is **hypergeometric**, $k \sim \mathrm{HG}(n, m, t)$,
sampling without replacement; the binomial $\mathrm{Bin}(t, m/n)$ is the
with-replacement approximation to it. The table uses the hypergeometric law. The
distinction is worth naming and not worth worrying about here — recomputing every
row under the binomial moves no entry by as much as $0.1$ byte, so the rounded
figures below are identical either way ( bytes ):

| $m$ | density | bound | RRR $t{=}64$ | RRR $t{=}256$ | Elias-Fano | Roaring |
|---:|---:|---:|---:|---:|---:|---:|
| 512 | 0.78% | 539 | 1266 | 756 | **576** | 1024 |
| 1024 | 1.56% | 950 | 1614 | 1159 | **1024** | 2048 |
| 2048 | 3.12% | 1643 | 2253 | 1836 | **1792** | 4096 |
| 4096 | 6.25% | 2762 | 3339 | **2942** | 3072 | 8192 |
| 8192 | 12.5% | 4452 | 4991 | **4612** | 5120 | 8192 |
| 16384 | 25.0% | 6645 | 7119 | **6796** | 8192 | 8192 |
| 32768 | 50.0% | 8191 | 8630 | 8334 | 12288 | **8192** |

Table: Entropy-coded dictionaries against the counting bound and against Roaring, in bytes.

Three readings, and the third is the one that matters:

- **The two candidates split the band.** Elias-Fano wins below roughly $3\%$
  density; blocked enumerative coding wins from about $4\%$ to $25\%$, which is
  precisely where §13.2 located the gap. They are complements, not rivals.
- **RRR loses to a plain bitmap at $50\%$** ( 8334 against 8192 ). At maximum
  entropy there is no redundancy to find and the directory is pure overhead —
  a useful sanity check on the model.
- **The directory is the entire cost.** At $t = 64$ the class array is 896 bytes
  against offsets of 2443; shrinking it by widening blocks is the only lever, and
  it is the lever that costs word-aligned arithmetic.

 **These figures are for *sequential* decode and are optimistic for random
access.** Classes are fixed-width, so a streaming reader derives each offset's
width as it goes and needs no pointer structure at all — which is exactly what a
Boolean kernel does. `contains` at an arbitrary position additionally needs the
sampled rank directory that a real `rrr_vector` carries, and that is not counted
above. The distinction is convenient here: kernels stream, and a point query can
decode the chunk once.

**A side benefit worth recording.** The class array is a per-block popcount
vector, which is exactly the shape §7 wants. It bounds intersections for free:

$$
\sum_i \max(0,\ k_i^A + k_i^B - t)
\;\le\; |A \cap B| \;\le\;
\sum_i \min(k_i^A,\ k_i^B),
$$

both computable from directories alone with no payload access.  Do not oversell
it: for two independent operands at $6.25\%$ density the upper bound is around
$16\times$ the truth, so it is a *bound*, not an estimate. It is worth exactly
one thing — the upper bound is zero iff every block has an empty side, which
**proves** disjointness, and §7.2 requires disjointness to come from an exact
source. That makes it useful in the sparse regime and decorative in the middle.

**The verdict is unchanged by any of this.** Enumerative coding gets within
$7\%$ of the counting bound while remaining addressable, which is a genuinely
strong result and the best available answer to §13.2's first gap. It is still not
**word-parallel**: intersecting two RRR chunks means decoding both block by block
through a divide-and-multiply chain, against 1024 `AND` instructions for two
bitmaps. Every representation in this appendix is a *storage* code, and the
constraint that decides their fate is the subject of §13.5.

## 13.5 What this system's constraints remove

Three of this crate's commitments bear directly on the choice, and two of them
are more restrictive than the space numbers suggest.

**Zero-copy is in tension with near-entropy coding, and this is the real
obstacle.** Requirement (R3) says a container *is* a slice of the mapping;
§8.3 shows the whole reclamation design follows from it. An array or bitmap
container satisfies this because it is compact *and directly operable* — bitmap
$\wedge$ bitmap is 1024 word ANDs against mmap'd memory, with no decode. Elias-Fano and
RRR are compact and **not** directly operable: `select` needs its support
structures, and $\cup$, $\triangle$ and $\setminus$ have no word-parallel form.
So adopting one means choosing per operation: keep zero-copy leapfrogging for
$\cap$, which EF supports natively, and decode for the rest. The space win is
real and it is not free — it converts some operations from pointer arithmetic
into decoding.

This is not a dead end in the literature — set operations *can* be computed
directly over compressed representations and produce compressed output [27], and
the RLE family ( WAH, EWAH ) has always done so, intersecting two compressed
bitmaps in time linear in their *compressed* sizes. That is the property Roaring
gave up run-length encoding to obtain more cheaply, and it is the property every
near-entropy code in this appendix gives back. The question a design must answer
is therefore not "is it smaller" — §13.1 settles that — but **how much of the
operation mix can run without a decode**, and the honest answer for enumerative
codes is: $\cap$ by leapfrogging, and nothing else.

**Byte identity ( §4.4 ) is bounded but no longer fatal.** A new encoding cannot
appear in a `.roaring` file, so export for such chunks becomes $O(m)$ rather than
$O(1)$. But the boundary where that conversion belongs *now exists*: `export_kind`
was added to fix §12.9's first finding, and it is exactly the place where the
in-memory or on-disk kind is reconciled with what the format can express. An
at-rest-only encoding is therefore an extension of a mechanism already present,
not a new concept.

**The on-disk kind field has one free value, and one hazard.** `ChunkRef` stores
kind in two bits at `[56:58)`, with $0,1,2$ used.  The decoder is

```rust
match (self.0 >> 56) & 0b11 { 1 => Bitmap, 2 => Run, _ => Array }
```

so `kind` alone still resolves $3$ to `Array`. That arm is deliberate: it is a
`const fn` with nowhere to put an error. **The refusal lives one level out.**
`ChunkRef::validate` rejects discriminant $3$ outright — "reserved: file written
by a newer format" — and `ShardStore::read_container_for` calls `validate` before
interpreting any payload, so the normal read path now refuses such a reference
rather than misreading it. That closes the silent-misread hazard an earlier
version of this section described; `validate` was previously reachable only from
`fsck`, which is what made the hazard real.

What remains is a genuine format-version question rather than a correctness one.
The slot is now reserved by behaviour, so introducing a fourth kind still needs a
superblock version gate — an older binary will refuse the file, which is the
correct failure — and the cost of the fourth kind is the compatibility break, not
a decoding accident.

## 13.6 A published structure that beats Roaring on both axes

Arroyuelo and Castillo [35] report a compressed structure that is
**simultaneously smaller and faster than Roaring** on two of three standard
collections. It is recorded here in full because §12 positions Roaring as the
reference implementation, and this is the closest the literature comes to
refuting that position.

Their structure is a **compressed binary trie** over the universe, with the set's
elements at the leaves. Space is $2(\mathrm{trie}(S) - n + 1) + o(\mathrm{trie}(S))$
bits, and $k$-way intersection runs in $O(k\delta \lg(u/\delta))$ where $\delta$
is the alternation measure of §12.3. Average bits per integer and milliseconds
per query, from their Table 2:

| structure | Gov2 space | Gov2 time | ClueWeb09 space | ClueWeb09 time | CC-News space | CC-News time |
|---|---:|---:|---:|---:|---:|---:|
| Roaring | 8.77 | 1.09 | 12.62 | 3.75 | 9.86 | 5.56 |
| RUP ( structure of [36] ) | 5.04 | 1.10 | 8.44 | 4.27 | 8.41 | 5.44 |
| PEF Opt ( structure of [5] ) | 3.62 | 1.88 | 5.85 | 6.50 | 5.80 | 17.33 |
| rTrie ( v ) | **4.81** | **0.77** | **7.96** | **1.96** | 9.95 | 6.09 |

Table: The published measurement, every row single-source from [35]'s Table 2.

**Every row is [35]'s own measurement.** The bracketed references name the
*structures*, not the sources of the numbers — [36] reports materially different
figures for two of these rows, and the two tables must not be mixed.

On Gov2 that is $0.55\times$ the space at $1.4\times$ the speed; on ClueWeb09
$0.63\times$ the space at $1.9\times$ the speed. On CC-News the two are level.
There is no space-time tradeoff being made here — it wins on both axes at once.

**Four qualifications, none of which dissolve the result.**

*It is intersection-only.* The stated problem is the **offline set intersection
problem**: preprocess a family of sets, answer $\bigcap_{i \in Q} S_i$. Union,
difference, complement and symmetric difference appear in their introduction as
motivation and nowhere in their results. §5's signature needs all five, and the
$\mathcal{H}$-optimal structures of §13.4 have the same shape of gap — good at
one operation, silent on the rest.

*It is static.* Sets are preprocessed and never updated. The authors note that
dynamic binary tries would support insertion and deletion, as future work, unmeasured.
§8's write path — memtable, WAL, checkpoint, MVCC versions — is not something the
published structure addresses, and R2's atomic visibility is a requirement it
was never asked to meet.

*The universe is $2^{25}$–$2^{26}$, not $2^{64}$.* Trie depth is $\lg u$, so the
$O(k\delta\lg(u/\delta))$ bound is roughly $2.5\times$ worse at yesno's universe
for the same $\delta$. §3's fixed 16-bit chunking is deliberately
universe-independent; a trie is not. This is a real structural advantage that
their datasets cannot exhibit.

*Absolute bits per integer are not portable between papers.* [35]'s Roaring
baseline is $32\%$ larger on Gov2 than [36] and [38] report for the same library
on the same collection — but [36] reports both baselines, and RUP, which has no
run containers to disable, is inflated by $17$–$20\%$ in the same direction. The
cause is therefore common to both ( dataset preparation, index build, or document
ordering ) rather than a property of Roaring's configuration. The consequence is
methodological: figures may be compared **within** a paper and not across, so the
head-to-head above is [35]'s own and is quoted as such.

**What transfers regardless of the qualifications.** Their §5 compresses runs by
recognizing **a trie node whose subtree is full** — a full *dyadic* subtree, at
every scale. That is `ChunkClass::Full` generalised: §7's `ChunkProfile`
classifies `Empty` / `Full` / `Present` at exactly one scale, the $2^{16}$ chunk,
and a binary trie does the same classification recursively at all 48 scales above
it. So the crate's profile is a one-level binary trie, and a segmentation that
cuts the prefix domain at `Full` / `Empty` transitions — §14.4's leaf collapse
applied at the boundaries §6.3 chooses — is a flattened two-level version of what
this structure does natively.

That reframes the profile from "a statistic that has so far earned nothing" into
"the bottom rung of a ladder that is known to pay when climbed". It does not make
it earn anything today, and it is not an argument for building a trie. It is an
argument that the *idea* is load-bearing and the *granularity* is what is
impoverished — and it is one of the three attestations §12.3 now cites against
its own earlier over-reading of the certificate criterion.

**The trie itself was costed as a fourth container kind, and declined.** That
happened after this section was first written, so the outcome is recorded here
rather than left as an open candidate. Adding it would have bought about $6\%$ of
stored bytes as an upper bound, against the roughly $6\%$ §13.9 names as the
threshold below which a format change does not pay; against bitmaps the ratio
$2d(\log_2(1/d) + 1)$ is independent of the universe, so its mid-density losses
survive every chunk width; and it is not part of the Roaring format, so export
would require canonical conversion. The normalisation boundary exists, but its
conversion cost remains part of the trade-off. The conclusion is narrow: what was declined is
this structure **as a fourth container kind at this granularity**. The ladder
argument above is untouched, and it is about climbing to a granularity a
container kind does not have.

**A historical note worth keeping.** The adaptive algorithm they analyse is Trabb
Pardo's [37], from a 1978 Stanford thesis under Knuth — twenty-two years before
Demaine et al. [28]. It went unnoticed because the original analysed only the
average case. The first adaptive set intersection algorithm existed for two
decades before the framework that could show it was optimal.

## 13.7 What the uniform assumption decides

Every ratio in §13.1 pins the run count to its expectation
$\mathbb{E}[r] = m(n-m+1)/n$ *because the set is assumed uniformly distributed*,
and real data is precisely where that fails. Conditioning properly, a set of $m$
elements in exactly $r$ maximal runs is one of

$$N(m,r) \;=\; \binom{m-1}{r-1}\binom{n-m+1}{r}$$

such sets, so an encoder told both needs $\log_2 N(m,r)$ bits — the fair
comparison for a run container, which stores both. ( As a check,
$\sum_r N(m,r) = \binom{n}{m}$. )

At fixed $m$ the cost ratio moves by roughly $2.5\times$ with $r$: $1.60$–$3.08$
at $m = 1024$, $1.48$–$3.85$ at $4096$, $1.23$–$3.10$ at $16\,384$. The uniform
model fixes $r$ within a few percent of its expectation, so §13.1 evaluates one
column of each of those rows. Two consequences:

- **The histogram below must be two-dimensional**, over $(m, r)$ rather than $m$
  alone; a one-dimensional histogram samples the same single column.
- **Ratios do not locate the prize.** Away from degenerate cells the ratio sits
  in a narrow band — median $2.09\times$, mostly $1.5$–$3.1\times$ — so there is
  no region where Roaring is catastrophically bad, and the prize is set by where
  the *bytes* are. A chunk that is a single contiguous interval illustrates the
  divergence: 6 bytes against a $\approx 2$-byte bound, which is $3.0$–$4.0\times$
  at every density and worth four to six bytes.

## 13.8 A third asymmetry: the space crossover is not the time crossover

Everything above prices a representation in bytes. The container set has an
asymmetry in *time* as sharp as §13.2's two, and pointing the other way. From the
kernels: bitmap intersection is a word loop, 1024 iterations of `a & b` whatever
the chunks contain — $\Theta(1)$ per chunk, independent of density — while array
intersection is a merge, or a gallop once one side is `GALLOP_RATIO` times the
other, and so $\Theta(m)$.

A time crossover $m^\ast = 1024\,\beta/\alpha$ therefore exists, for $\alpha$ the
cost of a merge step and $\beta$ of a word `AND`. Proposition 4 fixes
$\texttt{ARRAY\_MAX} = 4096$ where the two cost the same *bytes*; they cost the
same *time* at that point only if $\alpha/\beta = 0.25$, and there is no reason
for that. If $\alpha > \beta$ — a merge step being a load, a compare, an
unpredictable branch and a conditional push against a word `AND` that vectorises
— then $m^\ast \ll \texttt{ARRAY\_MAX}$, and across part of the middle band
Roaring stores an array that is space-optimal and slower to intersect than the
bitmap it declines to use.

This inverts §13.3's prescription for that band: the space argument wants
something *smaller* than an array, the time argument something *larger*.

Two cautions on what the asymptotics do and do not give. They do not establish
that a crossover falls **inside the legal array range** at all — an array
container exists only for $m \le \texttt{ARRAY\_MAX}$, and $m^\ast$ could lie
above it, in which case arrays are the faster form everywhere they are actually
used. And $m^\ast$ is not derivable from the two cost shapes alone, so it must be
measured; the implementation now carries such a measurement
( `intersect_crossover`, alongside streamed per-kind timings ), which is the only
thing that can settle both the value and whether it is reachable. What the
argument above establishes is that $m^\ast$ is a *different constant* from
`ARRAY_MAX`, and that assuming they coincide is unmotivated.

## 13.9 The measurement that comes first

Everything above states what a chunk *could* cost. What decides whether any of it
is worth building is a distribution: how many chunks, weighted by bytes, fall in
each band. The expected saving is

$$
\text{saving} \;=\; \sum_{\text{bands}} \big(\text{byte fraction in band}\big)
\times \Big(1 - \tfrac{\text{new cost}}{\text{Roaring cost}}\Big),
$$

where the second factor is known from §13.1 and the first is not. If a tenth of
stored bytes sit in the middle band, Elias-Fano buys about $6\%$ overall and is
not worth a format change; if half do, it is.

Such an instrument was built and run. It evaluates the bound per
chunk at its exact $(m,r)$ and accumulates before binning, so totals are
independent of bin edges; it orders cells by absolute waste rather than ratio,
because a full chunk has bound exactly zero and an infinite ratio against six
bytes of real waste. A first corpus of 200 appended ranges, 200 scattered-id
arrays and 30 dense bitmaps gives 475.5 KiB stored against a 280.2 KiB bound —
$1.70\times$ overall, inside the band §13.7 predicts. In that corpus the
contiguous band is 46% of chunks and 0.4% of the waste, and the leading cell by
waste is the sparse arrays rather than the bitmaps, at a *narrower* ratio than
the cell below it. Which cell leads is a property of the corpus, which is why the
question needed an instrument rather than an analysis.

One hand-built corpus is not a distribution. What it establishes is that the
measurement reports something the analysis could not.

**And then it answered a question and was deleted, which is the part worth
recording.** The instrument was extended to model the trie of §13.6 as a fourth
container kind, and the answer it returned closed that option: the trie's space
advantage is bounded at about $6\%$ of stored bytes, against the roughly $6\%$
this section had already named as the threshold below which a format change is
not worth making. Against bitmaps there is no advantage to bound — the ratio
$2d(\log_2(1/d) + 1)$ is independent of the universe size, so the mid-density
losses hold at every chunk width — and on time it would have had to beat a merge
that had just become several times faster. The decision was not to build it.

Two things about that episode outlive it. **The model was wrong in a direction
that flattered the candidate, and only the instrument caught it**: expected trie
nodes were first computed with a balls-with-replacement occupancy formula, which
understated the node count by nearly $14\%$ at half density while looking correct
in exactly the sparse rows anyone would spot-check. And **a ratio improved while
the quantity did not**: widening the chunk raised the trie's advantage over the
best alternative from $15\%$ to $43\%$ while total bytes per ordinal moved by
under $1\%$, because a wider universe inflates the array baseline an advantage is
measured against. A ratio is not a result until both of its terms have been
checked for movement.

The instrument itself no longer exists. It was research code that had landed in
the crate, it answered the question it was built for, and keeping it would have
meant carrying an $O(m)$ walk per observed container against no remaining caller;
its source is preserved outside the tree. The consequence for this appendix is
concrete and should be read as a cost rather than a tidy ending: **§12.9's fourth
finding and the complement-array option of §13.2 are now questions whose
instrument would have to be rebuilt before they could be answered.** That is the
price of the removal, and it was paid knowingly.

---

# 14. Appendix B: Restriction algebra

§6.3 cuts the prefix domain into segments, evaluates the expression on each, and
reassembles the results by concatenation. Proposition 8 states the decomposition
for the one case the planner uses — cuts at operand span endpoints, which are
prefix boundaries — and takes the reassembly for granted. This appendix supplies
the algebra underneath it.

The reason to do so is not completeness. Exactly one clause of the decomposition
depends on cuts falling between chunks; it is the clause that licenses
concatenation; and an evaluator that cut anywhere else would inherit the
proposition, silently lose that clause, and produce chunks out of order. So the
useful form of the statement is the one that says which half is alignment-free.

## 14.1 Windows

A **window** is a closed prefix interval $\Pi=[a,b]\subseteq\mathbb{P}$. Its
ordinal realization is the chunk-aligned range

$$
R_\Pi=\big[\,a\cdot2^{16},(b+1)\cdot2^{16}\,\big)\cap\mathbb{U}.
$$

For composition and execution, write restriction as the endomap

$$
\rho_\Pi:\mathcal{P}(\mathbb{U})\longrightarrow\mathcal{P}(\mathbb{U}),
\qquad
\rho_\Pi(S)=S\cap R_\Pi.
$$

For Boolean-algebra structure, use its corestriction

$$
\bar\rho_\Pi:\mathcal{P}(\mathbb{U})\longrightarrow\mathcal{P}(R_\Pi),
\qquad
\bar\rho_\Pi(S)=S\cap R_\Pi.
$$

The distinction is type-theoretic rather than computational: both return the
same set. The endomap composes directly across windows but does not preserve the
top of $\mathcal{P}(\mathbb{U})$; the corestriction maps the top $\mathbb{U}$ to
the top $R_\Pi$ of its codomain.

Chunk alignment makes restriction commute with the decomposition of §3:
$(\rho_\Pi S)_p$ is $S_p$ for $p\in\Pi$ and empty otherwise. No fiber is
partially restricted, so the existing fiber kernels evaluate a restricted
expression unchanged.

## 14.2 Restriction as an inverse-image homomorphism

**Proposition 18 ( restriction is a Boolean-algebra homomorphism into the
window ).** For all $S,T\subseteq\mathbb{U}$,

$$
\bar\rho_\Pi(S\cap T)=\bar\rho_\Pi S\cap\bar\rho_\Pi T,\qquad
\bar\rho_\Pi(S\cup T)=\bar\rho_\Pi S\cup\bar\rho_\Pi T,
$$
$$
\bar\rho_\Pi(S\setminus T)=\bar\rho_\Pi S\setminus\bar\rho_\Pi T,\qquad
\bar\rho_\Pi(S\triangle T)=\bar\rho_\Pi S\triangle\bar\rho_\Pi T,
$$

and complements are preserved relative to the corresponding top elements:

$$
\bar\rho_\Pi(\mathbb{U}\setminus S)=R_\Pi\setminus\bar\rho_\Pi(S).
$$

More generally, for a bounded complement over $Q\subseteq\mathbb{U}$,

$$
\bar\rho_\Pi(Q\setminus S)=(Q\cap R_\Pi)\setminus\bar\rho_\Pi(S).
$$

*Proof.* The map is the inverse image along the inclusion
$\iota:R_\Pi\hookrightarrow\mathbb{U}$. Inverse images preserve finite unions,
intersections, and complements relative to their domain and codomain tops.
Difference and symmetric difference follow from those operations. $\square$

The same identities hold for the endomap $\rho_\Pi$ after inclusion of
$\mathcal{P}(R_\Pi)$ into $\mathcal{P}(\mathbb{U})$. Every binary operator can
therefore be restricted operandwise with no semantic side condition. A planner may still decline pushdown for cost reasons. Bounded
complement is the only operator whose explicit parameter changes: its bound
narrows from $Q$ to $Q\cap R_\Pi$.

Proposition 29(2) gives the same inverse-image result for an arbitrary map.
Chunk alignment is not required for the algebraic identity; it is required only
for the operational property that no encoded fiber is split.

## 14.3 Windows compose, and partitions reassemble

**Proposition 19.** $\rho_\Pi \circ \rho_{\Pi'} = \rho_{\Pi \cap \Pi'}$. In
particular $\rho_\Pi$ is idempotent, and windows act on expressions as a
meet-semilattice. $\square$

**Proposition 20 ( decomposition, and what alignment adds ).** Let
$\Pi_1 < \Pi_2 < \dots < \Pi_k$ be consecutive *ordinal* intervals partitioning
$[lo,hi)$, not necessarily chunk-aligned. Then for every $S \subseteq \mathbb{U}$:

1. $\rho_{[lo,hi)}S \;=\; \biguplus_{i=1}^{k} \rho_{\Pi_i} S$, and the union is
   **disjoint**;
2. for each prefix $p$, the segments contributing to fiber $p$ are **contiguous**
   in the ordering, and their contributions arrive in strictly increasing ordinal
   order;
3. if in addition every cut is chunk-aligned, the decomposition is
   **prefix-ordered**: every prefix occurring in $\rho_{\Pi_i}S$ is strictly below
   every prefix occurring in $\rho_{\Pi_{i+1}}S$.

*Proof.* (1) The $\Pi_i$ partition $[lo,hi)$, so membership is decided in exactly
one segment. (2) A chunk occupies an interval of $\mathbb{U}$ and the $\Pi_i$ are
consecutive intervals, so those meeting it form a contiguous run; and every
ordinal of $\Pi_i$ precedes every ordinal of $\Pi_{i+1}$. (3) Under alignment no
fiber straddles a cut, so each prefix occurs in one segment only and the segments
are ordered by their prefix ranges. $\square$

**Clause 3 is what §6.3 spends and what it would cost to lose.** Concatenation's
precondition — every prefix of the left operand strictly below every prefix of the
right — is exactly clause 3, and under aligned cuts it is *established by
construction* rather than checked at run time. That is also why segmentation is
obviously meaning-preserving where §6.2's cost-guided rewrites are not: every
fiber lands in exactly one segment, and clauses 1 and 3 say so.

**Clause 2 is the replacement when alignment is unavailable**, and it is nearly
free. A segmentation whose boundaries come from operand *structure* rather than
from span endpoints cuts wherever the operands change character, and a range leaf
changes character at an arbitrary ordinal. Two adjacent segments then contribute
to the same chunk, clause 3 fails, and concatenation is no longer a valid
reassembly. But by clause 2 at most one fiber is partially built at any moment —
the one containing the current boundary — so a single scratch container suffices,
emitted as soon as a segment begins at or beyond the next chunk boundary. The
pieces being disjoint and arriving in increasing order, combining them is an
**append**: no comparison, no deduplication, no union kernel. The buffering is
$O(1)$, independent of the segment count and of how ragged the operands are.

Clause 2 is not only about segmentation, which is worth knowing before building
anything on it. Any construction that cuts $\mathbb{U}$ on a stride unrelated to
$2^{16}$ meets the same obligation for the same reason, and §15.5 is a second
instance already in the tree: an object smaller than a fiber can still span two
of them, containment is a property of the stride rather than of the size, and the
resolution is the same one — gather at the boundary, operate on a normalised
form, scatter back.

The failure mode if clause 3 is assumed anyway is worth naming, because it is a
quiet one. Concatenation does not check its precondition — it cannot, cheaply,
which is the reason the precondition was arranged to hold by construction — so it
emits chunks out of prefix order, and every operator above it mis-merges without
complaint. A wrong answer, not a slow one.

## 14.4 Leaf collapse: where a split pays

**Proposition 21 ( leaf collapse ).** Let $\ell$ be a leaf and $\Pi = [a,b]$ a
window. Then

1. $\rho_\Pi \ell = \mathbf{0}$ if $\operatorname{supp}(\ell) \cap \Pi = \emptyset$;
2. $\rho_\Pi \ell = \ell$ if $\operatorname{supp}(\ell) \subseteq \Pi$;
3. $\rho_\Pi \ell = R_\Pi$ if $\ell_p = \mathbb{L}$ for every $p \in \Pi$;
4. $\rho_\Pi R(l,h) = R\big(\max(l,\, a\,2^{16}),\ \min(h,\, (b{+}1)\,2^{16})\big)$,
   unconditionally;
5. otherwise $\rho_\Pi \ell = \ell \cap R_\Pi$, which is an expression already in
   the signature. $\square$

Cases 1 to 4 are the entire economic case for splitting. They are the only rules
in the system that **remove a restriction by consuming it**; case 5 leaves a
restriction node behind and the segment pays for it. So the value of a cut is
decided by how many operands it collapses, and Proposition 18 guarantees the cut
can be pushed to the leaves where that question is asked.

**The four cases are not equally answerable, and the asymmetry is §7's.** Cases 1
and 2 are decided by the occupancy abstraction of §7.1: emptiness over a window
and support containment are both questions about which buckets are marked, and
Proposition 9 makes the first sound in the safe direction. Case 4 is arithmetic.
Case 3 is the odd one — *fullness* over a window is not an occupancy question at
all. $\alpha$ records that a bucket is occupied, never that it is saturated, and
no refinement of $\tau$ changes that: the abstraction has the wrong codomain, not
insufficient resolution. Case 3 therefore needs a statistic §7 does not define,
and it is the case with the largest payoff, since a full operand annihilates an
intersection outright by §6.2's absorption.

That is the sharpest thing this algebra says about the implementation. The
containment predicate behind absorption is asked whether an operand is full
across another's *entire* span, and on ragged mixed-kind operands the answer is
essentially always no. Proposition 21 case 3 asks the same question of a
*window*, and a window can be chosen where the answer is yes. The predicate is
not weak. It is being asked a question whose answer is usually no, and
segmentation is the change of question rather than a change of predicate.

---

# 15. Appendix C: Reinterpretation layers

Everything above treats $S \subseteq \mathbb{U}$ as a *set*. It is also a bit
vector of length $\lvert\mathbb{U}\rvert$, and three layers built after the rest
of this article read it as something else: as a stack of dense boolean matrices,
as a series of arbitrary-precision unsigned integers, and as several sets sharing
one ordinal space. They are consumers of the model in the sense §1 reserves for
the Arrow boundary — they read §3's decomposition rather than shaping it — and
they would deserve only a scope note, except that between them they instantiate
**four** statements proved earlier in this article at carriers those statements
were not written for, and the third of them turns out to *contain* two more as
special cases. That is the argument for the appendix: a formulation that only
redescribes its own system is worth less than one whose statements turn out to be
about something more general, and this is the first evidence available either
way.

## 15.1 A layout is an affine embedding, and it need not respect the chunking

Both layers are parameterised the same way. Fix an index space and an injection
of it into $\mathbb{U}$ that is affine in each index. For matrices, element
$(r,c)$ of matrix $k$ sits at ordinal

$$
k\,\mu \;+\; r\,\lambda \;+\; c
\qquad\text{( row-major; column-major exchanges $r$ and $c$ )},
$$

with $\lambda$ the distance between consecutive lines and $\mu$ the distance
between consecutive matrices, both free. For integers the same shape with one
index: the ordinal $k\,\mu + j$ carries the $2^{j}$ term of integer $k$.

**Proposition 22 ( layout-induced reinterpretation ).** Every such layout induces
a decomposition of $\mathbb{U}$ into consecutive blocks and hence a family of
objects $(O_k)_k$ with $O_k$ determined by $S \cap [k\mu, k\mu + \text{span})$.
The decomposition is a partition exactly when $\mu$ equals the span, and is
otherwise a partition of a subset, the excess being unaddressed padding.
$\square$

Nothing in Proposition 22 mentions $2^{16}$, and that is the point: it is the
same observation §12.6 makes about Theorem 2 by way of Tarski duality — *any*
partition of the universe induces a decomposition, and the sixteen-bit split was
a choice of where to cut. §15 cuts somewhere else, on a stride that is a
parameter rather than a constant, and the two cuts are **independent**. Their
independence is the source of every difficulty below.

## 15.2 Dense layouts and addressed domains

Let $D\subseteq\mathbb{U}$ be the image of all valid object coordinates under
the layout. A dense layout has $D=[0,N)$ for the total number $N$ of addressed
bits.

**Proposition 23 ( elementwise algebra is set algebra on the addressed
domain ).** For a dense layout, the interpretation map from
$\mathcal{P}(D)$ to valid object stacks is a bijection. For every
$\otimes\in\{\cap,\cup,\triangle\}$, applying $\otimes$ elementwise to two stacks
is equivalent to applying it to their subsets of $D$.

*Proof.* Each valid coordinate maps to exactly one ordinal in $D$, and every
ordinal in $D$ has exactly one coordinate. Membership operations are pointwise,
so the bijection commutes with the three operators. $\square$

For an arbitrary $S\subseteq\mathbb{U}$, the interpreted stack depends only on
$S\cap D$; no bijection with all of $\mathcal{P}(\mathbb{U})$ is claimed. A
padded injective layout likewise preserves information on its addressed domain
when padding bits are fixed or ignored. What padding loses is the identification
of the layout with a contiguous prefix and the ability to apply whole-set
operations without respecting masks and strides, not necessarily a bijection
between valid logical values and valid representations.

## 15.3 Operationally admissible semiring additions

Packed bits support several mathematical semirings. Both
$(\{0,1\},\vee,\wedge)$ and $(\{0,1\},\wedge,\vee)$ are Boolean semirings, and
$(\{0,1\},\oplus,\wedge)$ is the field $\mathrm{GF}(2)$. The second Boolean
semiring has all ones as its additive identity.

**Proposition 24 ( a sparse additive identity selects $\vee$ and $\oplus$ ).**
Among $\{\cup,\triangle,\cap,\setminus\}$, requiring an associative addition whose
identity is the empty set excludes $\cap$ and $\setminus$; the remaining
operations are $\cup$ and $\triangle$, each of which forms a semiring with
intersection as multiplication.

*Proof.* Union and symmetric difference are associative with identity
$\emptyset$, and intersection distributes over both. Difference is not
associative. Intersection is associative, but its identity is the full addressed
domain rather than $\emptyset$. $\square$

The empty-identity requirement is operational rather than algebraic: an absent
object should denote zero without materializing the address space. Proposition 3
shows why the all-ones identity is unsuitable for this sparse interface. It does
not imply that the dual Boolean semiring fails to exist.

The two admitted structures have different interpretations. The Boolean
semiring models reachability, whereas $\mathrm{GF}(2)$ models parity. In
$\mathrm{GF}(2)$ there is one nonzero scalar, so elimination uses bit-valued
multipliers and no division. Partial pivoting remains necessary to replace a
zero pivot.

Singularity over $\mathrm{GF}(2)$ is exact rather than tolerance-dependent: a
pivot exists in a column or it does not. The same elimination therefore returns
the rank without introducing a numerical conditioning threshold.

## 15.4 Canonicity again, at a carrier Proposition 6' was not written for

Both layers carry a **canonical form** — a matrix as row-major words with each
row padded to a whole word, an integer as little-endian limbs with no trailing
zero limb — and in both the canonicity is not a convenience.

**Proposition 25 ( representational equality forces a canonical form ).** If
equality on the concrete type is defined by comparing representation words, then
any bit not determined by the denotation must be fixed by an invariant. Otherwise
two objects with equal denotation compare unequal, and every derived quantity
computed from the representation is wrong. $\square$

The two instances are the same statement. A matrix's last word per row holds bits
at or above the column count; they are not "don't care", because a dirty tail
makes equal matrices compare unequal and makes the population count wrong, so
every operation that writes a row masks it. An integer's limb vector must not end
in a zero limb, because equality compares limbs and ordering compares lengths
first, so a trailing zero makes equal integers unequal and the bit length wrong.
Each layer carries a debug-time guard for its own invariant, and the second is
explicitly the analogue of the first.

This is Proposition 6' at a third carrier. §5.1 states canonicity for *streams* —
ascending, non-empty, one production per prefix — and §11 records that the gap
was invisible to every denotational test for exactly this reason: a malformed
stream still collects to the right set, because the layers above absorb the
malformation. A dirty tail is the same shape of defect. The denotation is right
and the object is wrong, and only something that inspects the representation can
tell.

**A corollary about caching, which the two layers answer differently and both
correctly.** The matrix caches its population count; the integer caches no bit
length. §4.2 requires cardinality to be $O(1)$ and treats that as a proof
obligation; the decision rule that falls out of it is that a cache is worth its
invalidation burden only when the quantity is not already $O(1)$. A population
count over packed words is linear, so it is cached. A bit length is the top
limb's leading-zero count, so caching it would be state to invalidate in exchange
for nothing.

## 15.5 Straddling, and why this is Proposition 20 clause 2 again

Here is the sharpest recurrence, and the one with a live trap in it.

**Proposition 26 ( containment is a property of the stride, not of the size ).**
An object of span $s < 2^{16}$ lies within a single fiber if and only if
$\lfloor k\mu / 2^{16} \rfloor = \lfloor (k\mu + s - 1) / 2^{16} \rfloor$, which
depends on $\mu$ and $k$ and not on $s$ alone. In particular $s < 2^{16}$ does
not imply containment for any $k$. $\square$

A hundred-by-hundred matrix is ten thousand bits, comfortably under a fiber's
sixty-five thousand — and matrix seven spans bits seventy thousand to eighty
thousand, crossing the boundary. So a fast path conditioned on "small enough to
be in one chunk" is wrong, and must **test** containment rather than infer it
from size.

That is Proposition 20 clause 2 with the words changed. §14 proves that a
decomposition into arbitrary ordinal intervals stays disjoint and stays
per-fiber contiguous, but is prefix-ordered only when every cut is chunk-aligned
— and warns that an evaluator cutting at run boundaries therefore cannot
reassemble by concatenation, because two adjacent segments can contribute to one
chunk. A layout whose stride is not a divisor of $2^{16}$ cuts at arbitrary
ordinals for exactly the same reason, and pays for it in exactly the same
currency: a piece that spans two fibers must be gathered before it can be
operated on, and scattered back afterwards.

Which is what both layers do, and it is worth stating as a design theorem rather
than as an implementation note. **The seam is paid once at the boundary and never
inside a kernel.** The reader gathers an arbitrary-layout object into the
canonical form of §15.4; every kernel is defined only on that form and is a
branch-free word loop; a sink scatters the result back. One shift-and-carry path
inbound, one outbound. The alternative — teaching every kernel to handle an
arbitrary layout — would put the same shift path into the product, the transpose,
the inverse and every reduction at once, and in the integer layer it would sit
directly beside arithmetic carry logic that it has nothing to do with.

This generalises §4.5. There, representation independence is enforced by making a
generic kernel the *oracle* that every specialised arm must agree with. Here it
is enforced by making a canonical form the *domain* every kernel is defined on.
Both buy the same property — that the answer does not depend on how the operand
happened to be stored — and neither is available without a statement of what the
representations have in common.

**And the trap is in the test suite, which §11 predicts.** A generator that
produces only object indices which happen not to straddle leaves the seam
completely untested while every property passes, because every property is about
denotations and the denotation is right on the non-straddling path. That is
§11's third failure mode — a test that cannot fail — in the precise form §11
warns about: the generator must be biased toward the boundary, because uniform
sampling of an index reaches a straddling one only by luck. The remedy is the
same remedy §11 already records for ordinals near the top of the address space,
and it is the second time the same generator argument has been needed.

## 15.6 A third layer, and §15.1's injectivity is exactly what it drops

A third layer reads an ordinal set as **several sets sharing one ordinal
space**: $n$ constituent sets, each with its own logical ordinals, packed into a
single $S \subseteq \mathbb{U}$ under a descriptor of precisely the §15.1 kind —
constituent $i$'s logical ordinal $x$ at $x\,n + i$, or at $i\,\sigma + x$ for a
stride $\sigma$. Both are affine in each index and both are injective, so §15.1
through §15.5 apply to it unchanged, and §15.2 in particular already says the
useful thing: two sets packed under the *same* descriptor combine elementwise in
all $n$ pairs at once, through no new code and at no cost.

What is new is the other direction, and it is a single hypothesis. §15.1 fixes a
layout as an injection of an index space *into* $\mathbb{U}$; read backwards it
is a partial map *out of* $\mathbb{U}$, and its injectivity is the statement that
each index is carried by exactly one ordinal. Discard one coordinate of the index
— which constituent an ordinal belongs to, keeping only the logical ordinal it
carries — and that fails: $n$ ordinals land on one index. No object is then
determined by a block of $S$, because a fibre of $n$ bits has to be reduced to
one before there is an object at all, and nothing in §15.1 says how.

That reduction is not private to this layer. Two earlier constructions fit the
maps below in different directions: §14's restriction is inverse image along an
inclusion, with singleton fibres on the window and empty fibres outside it;
§7.1's occupancy abstraction is existential image along a surjection with
non-singleton fibres.

**The setting.** Let $\iota$ be a §15.1 layout on an index space $I \times X$,
let $D = \iota(I \times X) \subseteq \mathbb{U}$ be the ordinals it addresses,
and let

$$
\theta \;=\; \pi_X \circ \iota^{-1} \;:\; D \longrightarrow X
$$

be the *deindexing* map, which forgets the $I$ coordinate. $\iota$ is injective
and $\theta$ is not: $\theta^{-1}(x)$ is the **fibre** of $x$, the $\lvert I
\rvert$ ordinals carrying that logical ordinal in the several constituents.
The operational reduction model takes $X=\theta(D)$ and finite fibres, hence a
surjection with no empty logical coordinates. Proposition 29 later separates
the adjunction and inverse-image laws that remain valid for arbitrary maps. Four
maps are available: three reductions $\mathcal{P}(D)\to\mathcal{P}(X)$ and the
inverse image $\mathcal{P}(X)\to\mathcal{P}(D)$:

$$
\exists_\theta S = \{\,x : \theta^{-1}(x) \cap S \neq \emptyset\,\},
\qquad
\forall_\theta S = \{\,x : \theta^{-1}(x) \subseteq S\,\},
$$
$$
\oplus_\theta S = \{\,x : \lvert \theta^{-1}(x) \cap S\rvert \text{ odd}\,\},
\qquad
\theta^{-1} T = \{\, o \in D : \theta(o) \in T\,\}.
$$

Propositions 27 and 28 take $X$ to be the image of $\theta$, so every fibre is
nonempty. This avoids emitting results for unaddressed logical ordinals and is
needed by one of Proposition 28's inclusions. Proposition 29 states separately
which conclusions require surjectivity; inverse images and the adjunctions
themselves do not. Universal quantification over an empty fibre follows the
standard vacuous-truth convention.

Finiteness of each fibre is required for $\oplus_\theta$. If an implementation
compares a fibre count with a common constituent count rather than with the
fibre's actual size, it additionally assumes uniform fibre size. The reserved
top ordinal can violate that assumption in the final fibre.

**Proposition 27 ( there are three reductions, and the fourth is Proposition 3
again ).** Let a reduction keep $x$ exactly when the bits of its fibre reduce to
$1$ under a binary operation $\ast$ on $\{0,1\}$ that is associative and unital
( so that the reduction is a fold ) and commutative ( so that it does not depend
on how a fibre is enumerated ), and require that a fibre carrying no member of
$S$ be dropped. Then $\ast$ is one of $\vee$, $\wedge$, $\oplus$, giving
$\exists_\theta$, $\forall_\theta$, $\oplus_\theta$ respectively.

*Proof.* A commutative operation on $\{0,1\}$ is determined by the triple
$(0 \ast 0,\ 0 \ast 1,\ 1 \ast 1)$, and a unit $e$ fixes two entries of it:
$e = 0$ forces $0 \ast 0 = 0$ and $0 \ast 1 = 1$, leaving $\vee$ and $\oplus$;
$e = 1$ forces $1 \ast 1 = 1$ and $0 \ast 1 = 0$, leaving $\wedge$ and
$\leftrightarrow$. All four are associative, so there are exactly four
commutative monoids on $\{0,1\}$ and no fifth candidate to consider. Since
$u \leftrightarrow v = u \oplus v \oplus 1$, folding $m$ bits under
$\leftrightarrow$ from its unit yields $1 \oplus m \oplus \bigoplus_k b_k$. For
odd $m$ that is the parity and $\leftrightarrow$ is not a new reduction; for even
$m$ it is the parity's complement, and a fibre of zeros is *kept*. The result
then contains every index whose fibre misses $S$ entirely, so its support has the
size of $X$ however small $S$ is — the object Proposition 3 shows cannot be
produced from a sparse operand. $\square$

Difference is not a candidate because it is neither commutative nor associative.
The sparse-interface criterion of Proposition 24 excludes intersection as an
addition because its identity is the all-ones vector. The argument here similarly
excludes $\leftrightarrow$ because folding an absent fibre yields $1$. Both are
applications of Proposition 3's density observation, not claims that the
corresponding algebraic structures fail to exist.

The same requirement is what keeps the three survivors affordable. An existential
reduction is bounded by the constituents' total cardinality and a universal one
by the smallest constituent, so neither has to enumerate $X$ — which matters,
because $\lvert X \rvert$ is $\lvert \mathbb{U} \rvert / n$ and no reduction may
be linear in it.

**Proposition 28 ( each reduction is exact for exactly one operator, and for
$\setminus$ none is ).** For all $S,T\subseteq D$ and every surjection
$\theta:D\to X$:

| reduction | $\cup$ | $\cap$ | $\setminus$ | $\triangle$ |
|---|---|---|---|---|
| $\exists_\theta$ | $=$ | $\subseteq$ | $\supseteq$ | $\supseteq$ |
| $\forall_\theta$ | $\supseteq$ | $=$ | $\subseteq$ | neither |
| $\oplus_\theta$ | neither | neither | neither | $=$ |

Table: Row $F$ against column $\otimes$, relating $F(S \otimes T)$ to $F(S) \otimes F(T)$. Every inclusion is strict for some $\theta, S, T$; "neither" means neither one holds in general.

*Proof.* Exactness. A fibre meets $S \cup T$ iff it meets one of them, giving the
first entry. A fibre lies in $S \cap T$ iff it lies in both, giving the second.
For the third, $\lvert f \cap (S \triangle T)\rvert = \lvert f \cap S\rvert +
\lvert f \cap T\rvert - 2\lvert f \cap S \cap T\rvert$ for every fibre $f$, and
the parities agree.

One-sided laws. If $f$ meets $S \cap T$ it meets both, so $\exists_\theta(S \cap
T) \subseteq \exists_\theta S \cap \exists_\theta T$. If $f \subseteq S$ then $f
\subseteq S \cup T$, so $\forall_\theta S \cup \forall_\theta T \subseteq
\forall_\theta(S \cup T)$. If $f$ meets $S$ and misses $T$ then it meets $S
\setminus T$ and it meets $S \triangle T$, which gives the two $\supseteq$
entries of the first row. If $f \subseteq S \setminus T$ then $f \subseteq S$
and, *because $f$ is non-empty*, $f \not\subseteq T$; this is the one place the
surjectivity hypothesis is needed for an inclusion rather than for an identity.

Failures. Take a single fibre $f$ and $X = \{x\}$. With $f = \{a,b\}$, $S =
\{a\}$, $T = \{b\}$: $\exists_\theta(S \cap T) = \emptyset$ against
$\exists_\theta S \cap \exists_\theta T = \{x\}$, and the same pair makes the
first row's two $\supseteq$ entries strict; $\forall_\theta(S \cup T) = \{x\}$
against $\forall_\theta S \cup \forall_\theta T = \emptyset$; and
$\oplus_\theta(S \cap T) = \emptyset$ against
$\oplus_\theta S \cap \oplus_\theta T = \{x\}$. With $f = \{a,b\}$, $S =
\{a,b\}$, $T = \{a\}$: $\forall_\theta(S \setminus T) = \emptyset$ while
$\forall_\theta S \setminus \forall_\theta T = \{x\}$, and together with the
previous pair — under which $\forall_\theta(S \triangle T) = \{x\}$ against
$\emptyset$ — neither inclusion survives for $\triangle$; the same $S, T$ send
$\oplus_\theta(S \cup T)$ to $\emptyset$ against $\{x\}$ and
$\oplus_\theta(S \setminus T)$ to $\{x\}$ against $\emptyset$, and $S = \{a\}$,
$T = \{a,b\}$ supplies the opposite failure for $\setminus$. For the parity's
$\cup$ entry the opposite failure needs a fibre of three: with $f = \{a,b,c\}$,
$S = \{a,b\}$, $T = \{b,c\}$, $\oplus_\theta(S \cup T) = \{x\}$ while
$\oplus_\theta S \cup \oplus_\theta T = \emptyset$. ( At fibre size two that one
inclusion holds accidentally, since two even sets cannot union to an odd one
there. The table is a statement about all fibre sizes. ) $\square$

The table is worth reading in both directions. Along a row, each reduction is
exact for exactly one operator and no reduction is exact for two. Along a column,
each of $\cup$, $\cap$ and $\triangle$ has exactly one exact reduction, so the
assignment is a bijection — and it is Proposition 27's list of monoids seen from
the other side, $\vee$ with $\cup$, $\wedge$ with $\cap$, $\oplus$ with
$\triangle$. The column left over is $\setminus$, the operator that was not a
monoid, and it has no exact reduction at all. Its two one-sided entries point in
*opposite* directions, so it is not even sandwiched: nothing about
$F(S \setminus T)$ can be concluded from $F(S)$ and $F(T)$ alone by any single
reduction.

**Proposition 29 ( inverse-image adjunctions and their round trips ).** Let
$\theta:D\to X$ be any function, with finite fibres where parity is used:

1. $\exists_\theta \dashv \theta^{-1} \dashv \forall_\theta$. That is, for all
   $S \subseteq D$ and $T \subseteq X$,
   $$
   \exists_\theta S \subseteq T \iff S \subseteq \theta^{-1} T,
   \qquad
   \theta^{-1} T \subseteq S \iff T \subseteq \forall_\theta S .
   $$
2. $\theta^{-1}$ is a homomorphism of the whole signature: it commutes with
   $\cap$, $\cup$, $\setminus$ and $\triangle$ without side condition, and
   $\theta^{-1}\big(\neg_{\,\Xi}\, T\big) = \neg_{\,\theta^{-1}\Xi}\,
   \theta^{-1} T$ for a bounded complement over $\Xi \subseteq X$.
3. For every $\theta$, $S\subseteq\theta^{-1}\exists_\theta S$, with equality
   iff $S$ is a union of fibres. If $\theta$ is surjective, then
   $\exists_\theta\theta^{-1}=\forall_\theta\theta^{-1}
   =\mathrm{id}_{\mathcal{P}(X)}$. The parity round trip
   $\oplus_\theta\theta^{-1}$ is the identity iff every fibre has odd size.
4. $\forall_\theta S = \neg_X\, \exists_\theta\, \neg_D S$, both complements
   bounded, the right one by $D$ and the left by $X$.
5. $\exists_\theta$ and $\forall_\theta$ are monotone; $\oplus_\theta$ is not,
   and therefore participates in no adjunction and admits no reading as an
   abstraction.

*Proof.* (1) $\exists_\theta S \subseteq T$ says every $x$ with a witness in $S$
lies in $T$, which says every $o \in S$ has $\theta(o) \in T$, which is $S
\subseteq \theta^{-1}T$. For the right adjunction: if $T \subseteq \forall_\theta
S$ and $o \in \theta^{-1}T$ then the fibre of $\theta(o)$ lies in $S$, so $o \in
S$; conversely if $\theta^{-1}T \subseteq S$ and $x \in T$ then $\theta^{-1}(x)
\subseteq \theta^{-1}T \subseteq S$, so $x \in \forall_\theta S$. (2) $o \in
\theta^{-1}A$ is decided by the single test $\theta(o) \in A$, and every
connective named is defined pointwise on that test; the complement is relative
because $\theta^{-1}$ lands in $\mathcal{P}(D)$, which is the same phenomenon
Proposition 18 records for a window. (3)
$\theta^{-1}\exists_\theta S$ is the saturation of $S$ by fibres, so it contains
$S$ and equals it exactly when $S$ is a union of fibres. For
$\exists_\theta\theta^{-1}$ and $\forall_\theta\theta^{-1}$, both round trips
equal the identity for all $T$ exactly when every fibre is nonempty. The parity
round trip keeps $x\in T$ exactly when its fibre size is odd. (4) A fibre lies in
$S$ iff it does not meet $D \setminus S$. (5)
Monotonicity of the first two is immediate; for the third, on a fibre $\{a,b\}$
the set $\{a\}$ is kept and its superset $\{a,b\}$ is not. $\square$

Clause 5 is why the triple has three arrows and the section has three reductions,
and the counts do not match. A parity reduction is a perfectly good fold and a
perfectly bad abstraction, and no amount of care recovers a Galois connection for
it.

### §7.1 is the existential case, and Proposition 9 is one line of Proposition 28

Take $D$ to be the prefixes in range, $X$ the buckets, and $\theta(p) =
\lfloor (p - \beta)/2^{\tau}\rfloor$ — which is the deindexing of an interleaved
layout with $2^{\tau}$ constituents, translated by $\beta$. Then $\alpha =
\exists_\theta$ and $\gamma = \theta^{-1}$, verbatim. §7.1's Galois connection is
the left half of Proposition 29 clause 1; its coreflection $\alpha\gamma =
\mathrm{id}$ is clause 3, and the reason §7.1 gives for it — every bucket is a
non-empty set of prefixes — is surjectivity onto $X$ and nothing else. Finally,

$$
\alpha(X') \cap \alpha(Y') = \emptyset
\;\implies\;
\exists_\theta(X' \cap Y') = \emptyset
\;\implies\;
X' \cap Y' = \emptyset,
$$

the first implication being Proposition 28's $\cap$ entry for $\exists_\theta$
and the second the fact that an existential reduction is empty only on the empty
set. **That is Proposition 9**, which therefore stops being a standalone lemma
about a planner statistic and becomes one entry of a table.

Two things follow that §7.1 could not state, because it had only the one
instance. First, the converse of Proposition 9 fails *exactly* when some fibre
meets both operands, so the precision lost is a function of fibre size and
vanishes at size one; §15.1 to §15.5 never had to discuss it because an injective
layout has singleton fibres, where all three reductions coincide and the
abstraction is exact. Second, any caller-declared $\theta$ gives a sound
abstraction of the same shape — a zone map that can prove a disjointness and can
never prove a non-emptiness — for the same reason, and §7.3's argument that a
budget may choose *which* sound abstraction is used applies to it verbatim,
because that argument quantifies over over-approximations rather than naming one.

What does **not** generalise is the rest of §7.3's chain. The span abstraction
$\alpha_{\mathrm{span}}(X') = [\min X', \max X']$ is not $\exists_\theta$ for any
$\theta$: it fails to preserve unions, and every left adjoint preserves them. So
exactly one member of that chain is an instance of this construction, and the
others are sound for the reason §7.3 gives — over-approximation — and not for
this one.

### §14 is the injective inverse-image case

Take $D=R_\Pi$, $X=\mathbb{U}$, and let $\theta:D\hookrightarrow X$ be inclusion.
Its fibres are singletons over $R_\Pi$ and empty outside $R_\Pi$; the map is not
surjective unless the window is the whole universe. Nevertheless,

$$
\theta^{-1}(S)=S\cap R_\Pi=\bar\rho_\Pi(S),
$$

because Proposition 29(2) requires no surjectivity. It yields Proposition 18,
including the change of bound for relative complement.

If the codomain is instead restricted to the image $X=R_\Pi$, the inclusion
becomes the identity on $R_\Pi$: every fibre is a singleton and the existential,
universal, and parity reductions coincide. Thus restriction is an inverse-image
special case of Proposition 29, while the collapse of the three reductions
requires the separate image-codomain specialization.

### Operational distinction between inverse images and reductions

§14.2's headline is that a restriction pushes through every binary operator with
no side condition whatever, so that a planner declining to push one down is
declining on cost and never on correctness. **A reduction has no such property**,
and Proposition 28 is the exact statement of how it fails: one operator per
reduction, no reduction exact for two operators at once, one-sided laws at five
further entries and no exact entry anywhere in the $\setminus$ column. A reader who has
internalised Proposition 18 will assume a reduction may be pushed through a
conjunction and will be wrong, and the resulting answer is a well-formed set,
not an error.

The composable direction is the other one. By clause 2 the inverse image
distributes over the entire signature, so an expression of the form "expand a
coarse set, then intersect it with a fine one" is stable under every rewrite the
planner of §6 knows, while "reduce an intersection" is stable under none. That
asymmetry is not an implementation accident: it is clause 1 read as a statement
about which of the three maps is a left adjoint, which a right adjoint, and which
— by clause 5 — neither.

## 15.7 What this appendix does and does not claim

It does not claim that these layers are novel; dense bit-matrix algebra,
arbitrary-precision arithmetic over packed words and the packing of several sets
into one address space are all old, and the GF(2) elimination here is the
textbook $O(n^3/64)$ bitset algorithm. Nor is the adjoint triple of Proposition
29 new mathematics — quantification as adjoint to substitution is standard, and
§12.6 already records that §7.1's abstraction is an adjunction. Nor are these
layers formulated to the depth of §3–§8 — there is no complexity table, and the
propositions above are structural rather than quantitative.

The contribution of this appendix is organizational. Proposition 23 states the
addressed-domain condition needed for a layout interpretation to be bijective.
Proposition 24 separates a mathematical semiring from the additional sparse-zero
policy imposed by the interface. Propositions 27–29 distinguish existential,
universal, and parity reductions and state their exact and one-sided interaction
with set operations.

Proposition 29 also places two earlier constructions in one mapping framework,
but in different variance directions. Restriction in §14 is inverse image along
an inclusion, whereas occupancy in §7.1 is existential image along a surjective
bucket map. The former uses Proposition 29(2); the latter uses the left
adjunction and the existential row of Proposition 28. This is a structural
unification, not evidence that the underlying mathematics is novel.

The appendix therefore shows that the core set model can serve several
reinterpretation layers while making their domain restrictions explicit. It
does not establish quantitative performance bounds for those layers.

# References

1. S. Chambi, D. Lemire, O. Kaser, R. Godin. *Better bitmap performance with Roaring bitmaps.* Software: Practice and Experience 46(5):709–719, 2016. arXiv:1402.6407.
2. D. Lemire, G. Ssi-Yan-Kai, O. Kaser. *Consistently faster and smaller compressed bitmaps with Roaring.* Software: Practice and Experience, 2016. arXiv:1603.06549.
3. D. Lemire, O. Kaser, N. Kurz, L. Deri, C. O'Hara, F. Saint-Jacques, G. Ssi-Yan-Kai. *Roaring Bitmaps: Implementation of an Optimized Software Library.* Software: Practice and Experience 48(4), 2018. arXiv:1709.07821.
4. S. Vigna. *Quasi-succinct indices.* WSDM 2013.
5. G. Ottaviano, R. Venturini. *Partitioned Elias-Fano indexes.* SIGIR 2014.
6. D. Arroyuelo et al. *Trie-Compressed Intersectable Sets.* arXiv:2212.00946.
7. T. L. Veldhuizen. *Leapfrog Triejoin: A Simple, Worst-Case Optimal Join Algorithm.* ICDT 2014. arXiv:1210.0481.
8. A. Z. Broder, D. Carmel, M. Herscovici, A. Soffer, J. Zien. *Efficient query evaluation using a two-level retrieval process.* CIKM 2003.
9. G. Moerkotte. *Small Materialized Aggregates: A Light Weight Index Structure for Data Warehousing.* VLDB 1998.
10. K. Beyer, P. J. Haas, B. Reinwald, Y. Sismanis, R. Gemulla. *On synopses for distinct-value estimation under multiset operations.* SIGMOD 2007.
11. T. Härder, A. Reuter. *Principles of transaction-oriented database recovery.* ACM Computing Surveys 15(4):287–317, 1983.
12. O. Rodeh. *B-trees, shadowing, and clones.* ACM Transactions on Storage 3(4), 2008.
13. M. M. Michael. *Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects.* IEEE TPDS 15(6), 2004.
14. M. Rosenblum, J. K. Ousterhout. *The design and implementation of a log-structured file system.* ACM TOCS 10(1), 1992.
15. M. Athanassoulis, M. S. Kester, L. M. Maas, R. Stoica, S. Idreos, A. Ailamaki, M. Callaghan. *Designing Access Methods: The RUM Conjecture.* EDBT 2016.
16. R. Affeldt et al. *Proving tree algorithms for succinct data structures.* ITP 2019. arXiv:1904.02809.
17. S. F. Goldsmith, A. S. Aiken, D. S. Wilkerson. *Measuring empirical computational complexity.* ESEC/FSE 2007.
18. D. Olteanu, J. Závodný. *Factorised representations of query results: size bounds and readability.* ICDT 2012; *Size bounds for factorised representations of query results*, ACM TODS 40(1), 2015.
19. S. Tu, W. Zheng, E. Kohler, B. Liskov, S. Madden. *Speedy transactions in multicore in-memory databases.* SOSP 2013.
20. C.-Y. Chan, Y. E. Ioannidis. *Bitmap index design and evaluation.* SIGMOD 1998. See also P. O'Neil, D. Quass, *Improved query performance with variant indexes*, SIGMOD 1997.
21. M. Zheng, J. Tucek, D. Huang, F. Qin, M. Lillibridge, E. S. Yang, B. W. Zhao, S. Singh. *Torturing databases for fun and profit.* OSDI 2014.
22. H. Lang, A. Beischl, V. Leis, P. Boncz, T. Neumann, A. Kemper. *Tree-Encoded Bitmaps.* SIGMOD 2020.
23. J. Kärkkäinen, D. Kempa, S. J. Puglisi. *Hybrid compression of bitvectors for the FM-index.* DCC 2014. See also D. Okanohara, K. Sadakane, *Practical entropy-compressed rank/select dictionary*, ALENEX 2007, and R. Raman, V. Raman, S. S. Rao, *Succinct indexable dictionaries with applications to encoding k-ary trees and multisets*, SODA 2002.
24. *Random access decompression using binary arithmetic coding.* DCC 1999.
25. J. Duda. *Asymmetric numeral systems.* 2013. See also *Understanding entropy coding with ANS: a statistician's perspective*, arXiv:2201.01741, on random access into ANS streams.
26. T. M. Cover. *Enumerative source encoding.* IEEE Transactions on Information Theory 19(1):73–77, 1973.
27. F. Claude, G. Navarro et al. *Set operations over compressed binary relations.* Information Systems, 2018.
28. E. D. Demaine, A. López-Ortiz, J. I. Munro. *Adaptive set intersections, unions, and differences.* SODA 2000, pp. 743–752.
29. J. Barbay, C. Kenyon. *Adaptive intersection and t-threshold problems.* SODA 2002. See also M. Mirzazadeh, *Adaptive comparison-based algorithms for evaluating set queries*, MMath thesis, University of Waterloo, 2004, which extends the adaptive treatment to arbitrary union/intersection expressions.
30. E. Chiniforooshan, A. Farzan, M. Mirzazadeh. *Worst case optimal union-intersection expression evaluation.* ICALP 2005, and *Evaluation of general set expressions*, ISAAC 2008, pp. 366–377 — the latter covering union, intersection, difference, complement and symmetric difference.
31. P. Bille, A. Pagh, R. Pagh. *Fast evaluation of union-intersection expressions.* ISAAC 2007. arXiv:0708.3259.
32. J. S. Culpepper, A. Moffat. *Efficient set intersection for inverted indexing.* ACM TOIS 29(1), 2010. See also J. Barbay, A. López-Ortiz, T. Lu, A. Salinger, *An experimental investigation of set intersection algorithms for text searching*, ACM JEA 14, 2009.
33. M. Z. Hanani. *An optimal evaluation of Boolean expressions in an online query system.* CACM 20(5), 1977. See also R. Krishnamurthy, H. Boral, C. Zaniolo, *Optimization of nonrecursive queries*, VLDB 1986, for the quadratic-time optimal ordering of conjunctive queries under the rank rule.
34. A. Kemper, G. Moerkotte, K. Peithner, M. Steinbrunn. *Optimizing disjunctive queries with expensive predicates* ( bypass plans; Boolean Difference Calculus ), SIGMOD 1994, and *Optimization and evaluation of disjunctive queries*, IEEE TKDE 2000. See also F. Kastrati, G. Moerkotte, *Optimization of conjunctive predicates for main memory column stores*, PVLDB 9(12), 2016, and the disjunctive companion, SIGMOD 2017.
35. D. Arroyuelo, J. P. Castillo. *Trie-compressed adaptive set intersection.* CPM 2023, LIPIcs vol. 259, art. 1. See also *Trie-compressed intersectable sets*, arXiv:2212.00946.
36. G. E. Pibiri. *Fast and compact set intersection through recursive universe partitioning.* DCC 2021, pp. 293–302.
37. L. Trabb Pardo. *Set representation and set intersection.* PhD thesis STAN-CS-78-681, Stanford University, 1978 ( D. E. Knuth, advisor ).
38. G. E. Pibiri, R. Venturini. *Techniques for inverted index compression.* ACM Computing Surveys 53(6):125:1–125:36, 2021. arXiv:1908.10598 — Table 11 reports Roaring on Gov2, ClueWeb09 and CC-News with run containers enabled.
39. P. T. Johnstone. *Stone Spaces.* Cambridge University Press, 1982 — for the powerset/CABA duality and its categorical form.
40. G.-C. Rota. *On the foundations of combinatorial theory I: theory of Möbius functions.* Z. Wahrscheinlichkeitstheorie 2:340–368, 1964.
41. M. A. Krasnosel'skii, A. V. Pokrovskii. *Systems with Hysteresis.* Springer, 1989 — the relay hysteron and rate-independent operators.
42. A. Borodin, R. El-Yaniv. *Online Computation and Competitive Analysis.* Cambridge University Press, 1998 — metrical task systems and switching cost.
43. J. Rissanen. *Modeling by shortest data description.* Automatica 14:465–471, 1978.
44. F. Baader, T. Nipkow. *Term Rewriting and All That.* Cambridge University Press, 1998 — critical pairs and Newman's lemma.
45. F. Q. Gouvêa. *p-adic Numbers: An Introduction.* Springer, 3rd ed., 2020 — for the valuation and the ultrametric it induces.
46. G. D. Birkhoff. *Proof of the ergodic theorem.* PNAS 17(12):656–660, 1931.
47. RoaringBitmap project. *Specification of the compressed-bitmap Roaring formats.* [Project specification](https://github.com/RoaringBitmap/RoaringFormatSpec/) ( accessed 2026-09-13 ).
48. G. Sheffi, E. Petrank. *The ERA Theorem for Safe Memory Reclamation.* PPoPP 2023:435–437. [doi:10.1145/3572848.3577491](https://doi.org/10.1145/3572848.3577491).

Implementation documentation and source consulted for contextual comparison
include CRoaring, Java Roaring, Pilosa and FeatureBase, Lucene's iterator APIs,
and the `hs-to-coq` verification of Haskell's `containers`. The storage-system
comparison also notes US patent 11,886,411 without drawing a legal conclusion.

**Source scope.** The theorem-level comparisons in §12.3 and the quantitative
comparison in §13.6 use [28], [31], [35], [36], and [38] at the cited theorem,
definition, or table. Other references provide historical or terminological
context for established mechanisms. They are not used to claim priority. The
review is selective rather than systematic, as stated in §1.2 and §12, and an
absence from this comparison should not be read as evidence that no prior
formulation exists.

Absolute figures are properties of a measurement setup as much as of a structure
— [35] and [36] differ by up to a third on the same library and collections — so
numbers from different papers are never compared. Every table in §13 is
single-source: §13.1, §13.4 and §13.7 are computed here from stated formulas, and
§13.6's is [35]'s.

## Artifact and reproducibility statement

The manuscript is maintained in Pandoc-flavoured Markdown with TeX mathematics.
The accompanying software artifact contains the implementation, validation
suites, and model-consistency checks summarized in §11. Numerical tables derived
in the article identify their formulas or external source. Implementation status
statements refer to the artifact version dated in the manuscript metadata.

The mathematical development is not mechanized, and the empirical observations
are not claimed to generalise beyond their stated generators, workloads, and
configuration. Reproduction should report the artifact revision, toolchain, and
enabled features together with the observed result.
