# Quality Gates and Project Tooling

## Summary

The project gate combines formatting, clippy, tests, documentation checks, public-API containment, and deeper memory and race tools. The scripts themselves are production infrastructure: they need negative controls, path-stable execution, and completeness checks because a checker can report success without checking what its message claims.

## Key Facts

- Plain `cargo` is the supported workspace entry point.
- Whole-tree formatting is clean; `yesno-core/fuzz` requires a separate `cargo fmt` invocation because it is outside the workspace.
- Routine Rust completion requires workspace-wide clippy with `-D warnings`, `cargo fmt --check`, and `cargo test -p yesno-core`.
- Deep validation uses Valgrind, ASan, TSan, and selected measurements; MIRI was removed from the gate after its intrinsic coverage and runtime ceased to justify the step.
- ASan and Valgrind catch heap errors but cannot detect a logical overread that stays inside a file mapping. Gate labels must describe that narrower scope.
- `scripts/gate.sh` counts executed steps and rejects an incomplete run even if `$fail` stayed zero.
- The gate changes to its repository root once so invocation location cannot alter individual steps.
- The routine clippy gate runs the *default* toolchain while CI runs *stable*, and when those differ by a release the gate cannot emit the newer version's lints at all. Reproduce CI with an explicit `+stable`.
- AddressSanitizer test invocations use `--lib --tests`; sanitizer-instrumented doctests cannot link against dependency rlibs built without the runtime.
- A reusable workflow uses a literal concurrency namespace. Deriving it from `github.workflow` lets the caller control the callee's group and can make the callee cancel the run waiting for it.
- `yesno-e2e` has two crate roots and Cargo builds only one, so a whole class of source file is invisible to every Cargo check. Editing it obliges the Bazel gate.
- A wire-contract change obliges every client's own gate. One acknowledgement field has seven independent decoders across five languages, and only three are reachable from a Cargo invocation.
- Enumerate client directories rather than grepping for a field name; naming differs per language and a grep found three of seven.
- Building a test target is not running it: one gate compiles the C++ client's test inside an image while only a different gate executes it. For each changed file, name the command that runs an assertion covering it.
- The Java toolchain on a developer machine may be a JRE, so a Gradle build needs JAVA_HOME pointed at a JDK before it can compile anything.
- `check-layout.py` checks both missing and stale paths in the architecture map.
- `check-r1.py` evaluates effective visibility through ancestor modules.
- `check-plan-measure.py` pins the planner source region whose rewrite table was manually transcribed.
- `check-model-constants.py` validates named article sites from source-derived values without modifying the article.
- `check-todo-refs.py` requires every source-level backlog slug to resolve under `.agents/docs/` and has an empty debt baseline. Its preferred repair is self-contained reasoning, then recording genuinely open work, then repointing closed work.
- Rustdoc now runs with `-D warnings` workspace-wide; the 38-warning baseline reached zero on 2026-09-17. The judgment at nearly every private-link site was to **demote the link, not widen the API**, because a `pub( crate )` kernel made `pub` for rustdoc's sake is a semver promise for something nothing calls.
- **A check whose success is not evidence it checked anything** has five recorded forms: a missing scope flag, a gate nothing runs, a target compiled but never executed, an arch the host compiles out, and a platform no job builds.
- `cargo test --workspace` is **default features**, so a test file gated behind a non-default feature is type-checked by Clippy on every run and executed by nothing.
- `yesno-c` is a separate workspace, so `--workspace` cannot reach its gate; it needs its own CI job.
- Both `scripts/gate.sh` and `scripts/gate-pg.sh` must run for a `yesno-core` or `yesno-flight` change, and a passing `gate-pg` with no `CARGO_BAZEL_REPIN` prompt is a **negative result worth recording** -- it is the lockfile-drift check being exercised.
- CI and `gate.sh` must be compared **command by command**; step names are free text on both sides and a name comparison reports false differences.
- Any baseline, fallback, or exclusion whose quiet state is the only observed state is untested by construction. Perturb it and check the number moves.
- A baseline key must be constructible from the check's own output, or its failure message points at the wrong problem.
- Running individual commands is not equivalent to running `scripts/gate.sh`. The step-count verdict is part of the gate and must be updated only after proving whether a mismatch is an added step or an early exit.
- The repository contains no emoji in prose, comments, strings, or fixtures. Mathematical and typographic notation remains allowed.

## Details

### Formatting history

The initial formatting baseline contained many line-wrapping hunks. It was deliberately not mixed into unrelated changes. The maintainer later closed the baseline with a one-shot `cargo fmt --all` and a separate fuzz-crate pass. Routine formatting is now a whole-tree no-op when changes are clean.

Bare `rustfmt <file>` remains wrong because rustfmt follows `mod` declarations and may rewrite child modules. Use Cargo formatting commands.

### Deep tools and scope

- MIRI is a targeted manual tool only. Partial AArch64 intrinsic support and proptest runtime made a broad green step both slow and weaker than its label implied.
- Valgrind catches heap-access errors along executed paths but treats a mapped file region as addressable; it also serializes threads and does not substitute for a race detector.
- ASan covers heap memory errors across ordinary and concurrency suites but does not own file mappings and cannot detect an in-mapping logical overread.
- TSan is the available race detector for group commit, snapshot slots, and lock ordering.

Tool scope must be reported explicitly. A green MIRI tier says nothing about modules excluded by the filter.

### Checker failure modes

The project found four broad ways checks became misleading:

1. a helper existed and had tests but no production caller;
2. a validator ran only in an offline tool rather than the online path it guarded;
3. a checker implemented the wrong definition, such as spelling-level `pub` instead of effective public visibility;
4. the gate exited zero after a mid-script interruption and skipped later steps.

Negative controls must exercise the exact blind spot. A self-test that shares the checker's misunderstanding is not evidence.

The model-constant checker evolved from unscoped needle searches, to occurrence counts, to named role-specific sites. It now mutates only an in-memory article copy during self-tests. A validator must never rewrite the canonical artifact it validates, especially in a shared checkout.

### Citation and compiler-documentation gates

Backlog slugs in source once survived after their journal entries were consolidated away, leaving comments that looked supported while pointing nowhere. `scripts/check-todo-refs.py` scans source, tests, benches, scripts, and E2E files and resolves a slug if it appears anywhere under `.agents/docs/`. Its temporary debt baseline went from 16 to zero on its first day and must remain empty.

Resolution alone is the weakest repair. Prefer restating the rationale at the code site so the pointer leaves the check's domain; if the work is genuinely open, recover and verify the item against the current tree before adding it to `TODO.md`; if it closed, repoint only when navigation still adds value. Recovered comments produced three defective backlog entries out of eight because detailed stale prose was mistaken for current evidence.

The checker scans its own docstring, so examples and repair text cannot be exempt from the rule. This is intentional: an instrument excluded from its own domain can report a clean tree while blind.

The historical `r1-is-not-enforced` slug is closed. `scripts/check-r1.py` walks ancestor-module visibility rather than trusting an item's local `pub`, and its known-violation baseline is empty. The durable policy lives in `ARCHITECTURE.md`; the checker is its executable enforcement.

Rust documentation has a stronger existing authority. `cargo doc` was already reporting broken and ambiguous intra-doc links, but the warnings were buried among softer private-item links. Eleven broken links were repaired and `rustdoc::broken_intra_doc_links` is now denied separately. This keeps the zero-baseline class strict without turning 38 judgment-heavy private links into a baseline.

### A gate that exists, passes, and has no automatic runner

The recurring class is **a check whose success is not evidence it checked what it claimed**, and it has now appeared in five distinct forms, each found from a different direction:

- **Scope flags.** A root Clippy invocation lints `default-members` and reports a workspace-wide result. Fixed in `scripts/gate.sh` while the hand-run instructions in `AGENTS.md` kept the old command, so six days of "lint-clean" reports covered two crates.
- **No runner.** `yesno-c/gate.sh` existed, passed when a person typed it, and appeared in no CI job -- and `scripts/gate.sh` cannot reach it either, because `yesno-c` is a separate workspace and `--workspace` stops at this one's members. Nothing else in CI compiled a C translation unit against the published header, so the header could have drifted from `lib.rs` with every job green. Closed with a `c-abi` job needing only a Rust toolchain and the runner's `cc`.
- **Compiled but not executed.** `scripts/gate.sh` and `ci.yml` both run `cargo test --workspace`, which is **default features**, while `yesno-tantivy/tests/flight.rs` opens with `#![cfg( feature = "flight" )]`. Its 105-line integration test -- a Flight server plus current-versus-strict version pinning -- had never been executed anywhere; run by hand it passes in 0.32 s. It *reads* as covered because Clippy runs `--all-features --all-targets`, so the file is fully type-checked and lint-clean on every run. No summary line in either tool distinguishes *compiled under the gate* from *executed by the gate*. Note where the gap sits: the `cargo test` line is one line below a 17-line comment explaining how a missing `--workspace` on the neighbouring Clippy command made that step report success. **A rationale written for one command does not propagate to the one beneath it.**
- **Host-arch blindness.** On an aarch64 host the gate compiles `#[cfg( target_arch = "x86_64" )] mod simd` to nothing and goes green having never built it, and lints only the host arch. CI's `ubuntu-latest` runner covers the x86 arms by accident of being x86; an aarch64 developer gets no local signal, and nothing in the gate's output says so. The cross-check that closes it is in [SIMD Arch Arms and Kernel Selection](./simd-arch-arms-and-kernel-selection.md).
- **Platform blindness.** No job builds `yesno-core` on macOS, a platform `README.md` names as supported in two places, and an `extern "C"` symbol resolves at **link** time -- so neither a Linux gate nor a cross-*compile* can catch a wrong one. A wrong `errno` accessor name stood behind a comment claiming portability until someone built on a Mac. Tracked as `macos-build-is-not-gated`.

**Both cargo and Bazel gates must run for a `yesno-core` or `yesno-flight` change, and neither subsumes the other.** A session's worth of core changes once went through `scripts/gate.sh` all day and never `scripts/gate-pg.sh`, which is the same shape one level up -- at the level of *which gate*, where there is no step counter to catch it. The `gate-pg` run afterwards passed six steps and found **no lockfile drift**, worth recording as a negative result because that is the specific failure the two-gate rule exists to catch and "it passed" does not say whether the check was exercised. Cost: about forty minutes cold, since it builds arrow C++, MySQL and a KVM guest image from pinned source; artifacts cache, which argues for running it **often enough to stay warm** rather than saving it for the end.

### Two negative results about this tree's own check documentation

Both were audited after a consumer found the same class in theirs, and both came back clean -- recorded so they are not re-run hopefully:

- **Every script `gate.sh` invokes is named under `.agents/docs/`** ( all 12 ). A consumer's documented step table listed three of five checks actually run, invisible because each individual check passes and a layout checker verifies only `.rs` files against the architecture document, so a new *script* lands unlisted in silence.
- The first version of that audit matched gate step **names** against the docs and reported **13 of 20 undocumented** -- a crude matcher finding prose describing the same checks in different words. Matching the *scripts* invoked reported **0**, and the second is right because a reader looking for a check greps for its script, not for a step's printed label.

`check-unsafe-count.py`'s header also states what it does **not** check -- that the entry's reasoning is sound, that its file list is right, that MIRI's limits are as described. Those are judgement and stay with the reader; it checks two integers. A check that says what it does not cover is how a passing check stops implying more than it verified.

### CI drifts from the gate unless something compares them

`ci.yml` carries an explicit warning not to drift from `scripts/gate.sh`, written after the first instance. A command-by-command comparison later found **three more** missing checks ( `check-todo-refs.py`, `check-unsafe-count.py`, and the `cargo doc` step ), two of them added by this project's own recent sessions.

**Comparing by step name does not work and actively misleads**: a first attempt matched names and reported `clippy` and `tests ( workspace )` as missing, both of which are in CI under different names. The names are free text on both sides; only the commands are the same object.

And the mechanism matters more than the exhortation, since the exhortation was already there and already failed: a check is added to `gate.sh` by the session that needed it, and **nothing makes that session open `ci.yml`**. What would hold is a check comparing the two files, which does not exist -- tracked as `nothing-compares-gate-sh-to-ci-yml`.

### Baselines, exclusions, and quiet states must be exercised

A baseline, a fallback, or a recovery path whose **quiet state is the only state ever observed is untested by construction**, and the passing case looks identical whether it works or not. The test is to make the quiet state noisy on purpose and check that the number moves.

- A consumer's baseline mechanism **had never worked**: its key embedded a line number, so inserting anything above a baselined exception broke it in both directions. It had never fired because the set has always been empty. **A safety net nobody has fallen into is a claim, not a net.**
- `check-docs-selfcontained.py` keys on `rel::hit` and prints the *site* ( `docs/data-modeling.md:169` ) and never the key, so a key built from the message -- which is what the message invites -- does not match, and the entry is then reported as **stale**, pointing at the opposite of the real problem. Fixed by printing the key beside the site, and exercised with four arms: not-baselined, baselined, baselined-but-fixed, and clean.
- `check-todo-refs.py` needed no change because its key *is* the slug and the slug appears in its `UNRECORDED:` line -- exercised anyway.
- An exclusion count can be **structurally always zero**: script stems were dropped inside `cited()`, at the point of collection, so they never reached the summary and `N script stem( s ) not treated as slugs` could never be non-zero. That is worse than silence, because a reader sees an explicit zero. Caught by injecting a stem and noticing the count did not move.

Two further faults in the same family, both found by perturbation: `check-unsafe-count.py` used `re.search` ( match **zero or one** ) over the **whole file** independently of where the entry's slug was, so the slug and the claim could drift apart and it would verify a sentence belonging to a different entry. Now `finditer`, failing on ambiguity and naming the lines, verified with four arms plus a control -- wrong count in the doc, new `unsafe` block in the tree, and a second matching claim all caught.

**A check can be tripped by prose naming a script.** `check-todo-refs.py` reads backticked kebab-case with three or more segments as a slug, and a bare script stem is lexically a slug: three of fifteen three-segment stems resolved nowhere and would have been reported as dangling citations, while the other twelve "resolved" only because the docs mention them *with* an extension -- resolution by substring accident. Fixed by excluding stems **derived from `scripts/`** rather than from a hand-written list. The first version of that fix tripped the check it was fixing, because its own comment used a backticked three-segment example -- two paragraphs below the docstring warning about exactly that. The warning did not prevent the mistake; the automated check caught it in seconds.

**A weak rule measured at zero instances stays.** `check-todo-refs.py` resolves a slug appearing anywhere in the corpus, substring included, and of 40 cited slugs **zero** resolve only as a substring. A stricter "must be an entry heading" rule would reject the many legitimate citations of closed JOURNAL findings. The measurement is recorded so the next person does not re-derive that it is safe.

**And a check's output sentences are part of the check.** An audit of every gate output line found one claiming more scope than it measured: `check-tex-safe` reported `N file( s ) clean` while `strip_math` blanks every math span before scanning -- **6.1% of `docs/` by volume, never examined**. The exclusion is deliberate; leaving it unsaid was not, and it now reads `N file( s ) clean ( math spans not examined )`. A sentence can also be **correct by accident of variable choice**, which is not a comfortable property to find in a check that passes.

### Mechanize the arithmetic, read the judgement

Three detectors drew the line. An orphaned-doc-comment sweep flagged **16, 8 false** ( 12 with 7 false on a later run ). A check for backlog entries naming vanished identifiers flagged **14, all false, and none of the three real cases**. A count check is exact and free. The difference is not care: **a count has exactly one correct value, re-deriving it is cheaper than reading the sentence that states it, and a disagreement is a fact rather than a judgement.** "Does this claim still follow from its premise" has none of those properties.

`scripts/check-unsafe-count.py` ( gate step 15 ) is the first check of that second kind here: it verifies the two integers in `miri-cannot-reach-the-mmap-unsafe-sites` against the tree, sabotage-checked in both directions. The subject earned a check rather than another correction, because it had quoted a wrong count **twice** -- and the second wrong number was written by someone who was paying enough attention to correct the first. **A date stamp records when a number was taken; it does not notice when it stops being true.**

Two scoping rules come with it, and neither implies the other. A per-entry check is anchored to its entry's slug. A count inside a **closed, date-stamped** entry is a record of what was true when the work was done, so the tempting generalization -- "every number in `TODO.md` must match the tree" -- is **actively wrong** and would falsify history rather than repair it. Twice now the useful result has been a *split* rather than a rule, and both times the unifying statement was the tempting next step.

### The rustdoc baseline reached zero, and what the decisions were

`cargo doc` carried 38 judgment-heavy warnings, so only `rustdoc::broken_intra_doc_links` was denied at first, with the step's comment deferring the rest. The baseline is now zero and the step denies `-D warnings`.

**The judgment went the same way at all but one site: demote the link, do not widen the API.** Every private-link target was a `pub( crate )` kernel inside a `pub mod` -- `words_popcount`, `gallop_end`, `merge_array_run`, `simd::block_and_cardinality`. Making one `pub` to satisfy rustdoc buys a semver promise for something nothing outside the crate calls, which is the `stats.rs` failure by another route. The exception was **repointed** and is better than before: `yesno-flight`'s `visibility_wait` is a private field with a public builder, so the link names `with_visibility_wait`, which a reader can click through to and actually call.

Two mechanical traps bit before being understood. **`cargo doc` fails fast per crate**, so the first workspace measurement of "2" was an artifact -- `could not document yesno-flight` aborted the run before `yesno-e2e` was reached, and fixing those two revealed three more. A single pass's count is a lower bound; iterate to a fixed point. And **a find-and-replace on link text is wrong here**: three occurrences of one link name in `ops/bitmap.rs` are errors in only one place, the others being docs on *private* items where the link resolves correctly and would have been silently downgraded. The repair must be driven by the file and line rustdoc names, and the only reason this was caught is that the script asserted its expected match count.

**A tool whose baseline is 30 errors answers no question you actually have.** Before the cleanup, confirming that none of the 30 pre-existing errors were newly introduced worked only because the moved items had distinctive names to grep for -- luck, not method. The ordering constraint is therefore explicit: the gate step goes in *after* the baseline reaches zero, not before.

### Multiple build authorities

The routine script must invoke Clippy over the workspace explicitly. A root invocation once checked only two crates while reporting a workspace-wide result. The sabotage is a warning in a satellite crate that must turn the step red.

`yesno-pg` is built by Bazel against pinned PostgreSQL sources and has its own six-step gate across versions 17 and 18. Python uses a locked uv environment with Ruff, mypy, interoperability tests, and wheel inspection. Java uses its checksum-pinned Gradle wrapper with warnings-as-errors, tests, and Javadocs. These gates complement rather than replace the Cargo workspace checks.

### The lint gate is only as new as its toolchain

Continuous integration ran fifteen times without ever passing, and the clippy half of that had nothing to do with the code being wrong. CI resolves `stable`; the working copy's default toolchain was a release behind, and the lint that failed every run exists only in the newer one. A gate that runs `cargo clippy` therefore reported success while being structurally unable to see the failure, for twelve consecutive runs.

The remedy is cheap because the newer toolchain is usually already installed: run the lint with an explicit `+stable` to reproduce what CI will do, rather than trusting the default. The same reasoning applies to any check whose strictness is a property of the tool rather than of the repository.

Two lints were involved and only one was about style. The first wanted a constant-size chunk iterator replaced by the fixed-size form, which is a mechanical and behaviour-identical rewrite. The second was a genuinely duplicated import inside a test module, already provided by the module's glob of its parent — invisible to the older toolchain, and real.

### Which consumers encode a wire contract

Widening the ingest acknowledgement field broke seven independent decoders of it across five languages: the Rust client, the command-line tool, the end-to-end harness, and the published Python, C++, Go, and Java clients. Only the first three are reachable from any Cargo invocation, and both Rust gates reported success throughout while every Python, C++, Go, and Java ingest failed outright.

For a change that alters a wire contract, the useful question is not whether the gates passed but which consumers encode that contract and whether anything executed them. Enumerate the client directories to answer it: grepping for the field name found three of seven, because one client names its buffer after the operation and decodes through a shared helper, another capitalises the field differently, and a first search omitted one language's file extension altogether.

Three distinct traps sat behind the seven. Two consumers tested the width with no else branch, so they reported a zero row count rather than a decode error and the symptom named the wrong cause. One client decoded the acknowledgement with the same helper that decodes its single-value action results, which really are eight bytes, so relaxing the shared helper would have weakened four unrelated decoders and the acknowledgement needed its own. And one client's test fixture is a fake server that emitted the old width, encoding the very contract it exists to validate, so that client's own gate would not have caught the break either.

Four clients ship their own gate scripts and one builds through Gradle, so a wire change has six independent authorities rather than two.

Compilation is not execution, and this distinction survived a green gate. The PostgreSQL gate builds an all-in-one image whose Dockerfile compiles the C++ client's unit test as part of the MySQL artifact step, so that gate's log contains those source filenames; the build command is a plain build, which produces a test binary without running it. Only the MySQL gate executes that test. The inverse reading misleads too: the same run lists only the PostgreSQL targets, because those are the outer ones and the C++ compilation happens a layer deeper inside the image, so the target list alone suggests the C++ sources were never touched at all. For each changed file, identify the command that executes an assertion covering it rather than inferring coverage from a log or a target list.

One further duplication to know about: the MySQL storage engine compiles the C++ client through CMake rather than through its Bazel library target, so that source has two independent build paths and a change can satisfy one while breaking the other.

### A source file Cargo cannot see

`yesno-e2e` has **two crate roots**. `lib.rs` declares `pub mod world;`, while `fixture_lib.rs` declares `pub mod world { include!( "fixture_world.rs" ) }` and is a separate, minimal crate that only Bazel builds, because nothing in `Cargo.toml` names a target for it. `--all-targets` enumerates Cargo's targets, so no Cargo invocation can reach `fixture_lib.rs`, `fixture_world.rs`, or anything they include.

A change that referenced `crate::world::scenario_prefix` from `fixture_world.rs` therefore passed workspace-wide clippy, formatting, the whole unit suite, and every end-to-end scenario, and then failed the Bazel build with an unresolved-function error. This is a stronger form of "neither gate subsumes the other" than the stale-lockfile case that phrase usually refers to: there the two builds disagree about a dependency version, here one build never parses the file at all.

Two consequences. Editing those files obliges the Bazel gate before reporting completion. And an included file is a source of the Bazel target even though no module declares it, so it must appear in that target's explicit `srcs` list; omitting it reproduces the same failure one build later.

### One retained all-in-one E2E image

Every containerized gate — PostgreSQL 17/18, MySQL 8.4, OpenSearch 3.8.0, Elasticsearch 9.5.2, the Kubernetes operator, and the native filesystems — runs in one retained `yesno-e2e:local` Docker image, built by a single helper so no gate can drift onto a private tag. It contains both Bazel output graphs, JDK 21, Cargo and Gradle outputs, checksum-verified engine archives, the Kubernetes tools, and the bootable QEMU guest. Each public gate still selects only its own fixtures; the expensive immutable inputs and caches are shared, while no gate subsumes another.

The consolidation replaced three per-integration images on 2026-09-01. They had duplicated the source copy, the release-binary build and the pinned toolchain three ways, and had drifted onto two different Rust versions for the same `yesno-e2e` binary. The cost is that any gate's first run in a fresh checkout pays for every integration's artifacts; the benefit is one definition and one build. The opt-in real-AWS gate keeps a slim runner of its own, because its image is pushed to a per-run ECR repository and pulled onto an EC2 host, where the artifact tree would be paid for in transfer on every billable run.

One image with two entry identities. The database and search gates must run as the non-root builder because initdb, mysqld and the search engines refuse root; the operator and filesystem gates need a bind-mounted Docker socket and `/dev/kvm` and enter with an explicit `--user 0:0`. The image default is the builder identity and the entrypoint guards the database gates by argument, so a mistaken root entry fails with a clear message rather than an unexplained initdb error deep inside a gate.

The image is intentionally large because it retains reviewable, pinned build state across sessions. Documentation-only paths and ordinary build outputs are excluded from its context so they do not invalidate the artifact layers. Runtime stages must build every binary they copy; clean image builds have exposed missing declared artifacts that incremental local trees concealed.

The retained image measured 32.3 GB and roughly 31-35 minutes for a cold aarch64 build. Because `COPY . /workspace` sits below the artifact builds, an ordinary source edit and even a comment edit to `e2e/Dockerfile` invalidate those layers. After changing the image or an entry command, `gate-search` plus `gate-filesystems` is the cheapest pair that exercises both the non-root builder identity and the explicit root identity.

### Self-contained human documentation

Standing files under `docs/` describe operator-visible behavior, not the repository machinery that verifies it. The real-AWS Terraform acceptance harness was removed from `docs/operations.md` because it is project test infrastructure, while operator drill and measured RPO/RTO guidance remains. The durable test is whether the reader performs the action on a deployment or the project performs it on its own code.

`scripts/check-docs-selfcontained.py` rejects source, benchmark, test, example, and script paths in `docs/`; its baseline remains empty. Agent documentation has the opposite job and may name source paths that layout checks keep current.

Repository-authored text also has a whole-tree lexical rule: pictographic markers are forbidden in documentation, comments, commit messages, and strings a program prints. Words and Markdown emphasis carry their meaning; arrows, set operators, floor brackets, and other notation read as part of a sentence remain permitted. PostgreSQL SQL and expected-output fixtures must change together because echoed SQL comments are part of the byte-exact oracle.

### Memory-document lifecycle

`good-sleep` consolidates chronological journal entries into source-topic documents, `deep-sleep` refreshes broader synthesis documents without deleting sources, and `distill-memories` promotes canonical facts into overview, architecture, or gate documentation. The index keeps synthesis and source-topic tables separate so each workflow edits only its owned layer.

### CI

The workflow has gate, MSRV, lean-core, client, and scheduled/manual deep jobs. Local command success does not prove runner configuration, action versions, cache behavior, reusable-workflow composition, or PR-only logic. Remote history must be inspected directly rather than inferred from a configured workflow or local green gates.

The first audit found fifteen remote runs and no success: twelve `ci` runs reaching back to 2026-08-29 and three `release` runs. Three unrelated mechanisms explained the failures and missing coverage. CI's `stable` toolchain emitted a lint the default local toolchain did not have; the CI AddressSanitizer command omitted the local gate's `--lib --tests` scope fix; and the reusable CI workflow inherited the release caller's concurrency group and cancelled the run waiting on it. Each was invisible from a different vantage point.

Under AddressSanitizer, rustdoc compiles examples with the sanitizer flag but links them against dependency rlibs built without the sanitizer runtime, failing on undefined `__asan_*` symbols before any example executes. Restricting that invocation to library and integration-test targets matches the local deep gate and is not permission to delete doctests.

The concurrency collision is specific to reusable workflows: inside the called workflow, `github.workflow` resolves to the caller's name. If both caller and callee build their concurrency group from that value with cancellation enabled, the called gate cancels the caller that is awaiting it. The CI workflow therefore owns a literal `ci-` prefix, and release tag runs use a guard that prevents a publish gate from being cancelled. A badge or scheduled run is not evidence that pushes are gated; push coverage must be observed on the remote trigger path itself.

The complete remote release graph first succeeded in run 34706845864 at 2026-09-12T16:58Z after fifteen consecutive failures. Every reusable gate job passed, followed by the architecture plan, both binary builds, manifest assembly, publication, and image smoke test. The published `edge` and immutable commit tag resolve to a two-architecture manifest, so the ordinary CI and release path is proven by the event path itself rather than by local equivalents.

The scheduled/manual `deep` job is a separate evidence path. It is skipped on push by design, so local deep-gate success does not prove that the fixed AddressSanitizer invocation works on the remote Sunday schedule or an explicitly authorized manual run.

The release pipeline is one graph: `.github/workflows/release.yml` invokes the reusable CI gate before compiling or publishing. Keeping the gate and publish in separate workflows would allow a fast image build to publish a commit whose slower test workflow later failed. Pull requests skip the called gate because `ci.yml` already runs independently; downstream jobs explicitly distinguish a skipped dependency from a failed one, since GitHub Actions otherwise blocks on both.

`dist/build.Dockerfile` cross-compiles the seven shipped binaries separately for `linux/amd64` and `linux/arm64`; `dist/Dockerfile` only assembles them. A multi-call entrypoint accepts an explicit shipped binary for ECS and defaults flag-shaped arguments to `yesnod` for Kubernetes and local Docker. This is unambiguous only while `yesnod` has no positional argument, a property pinned in its CLI tests. The runtime stage performs no target-architecture `RUN`: the development host's binfmt registration lacks the fix-binary flag, so foreign execution can fail with a misleading `no such file or directory`.

Foreign `lvm2` userspace is downloaded and unpacked without executing target code. The package closure must exclude target-base packages, handle Debian's merged `/usr` layout, and include `dmsetup` explicitly because multi-arch resolution may otherwise satisfy it with the native package and the architecture filter then drops it. The image remains non-root; the deployment, not the artifact, grants uid 0 and `CAP_SYS_ADMIN` to `yesno-snapshot-agent` alone.

Release tags are deliberately classified: `edge` follows `main`, exact three-part stable versions move `latest` and major/minor aliases, prereleases move no aliases, and `sha-<12>` is immutable. Every published alias must resolve to the same verified multi-platform manifest. A workflow's own path filter is a live cross-reference: renaming `cd.yml` to `release.yml` required updating the filter as well as comments and documentation.

## Files

- `scripts/gate.sh` - routine and deep orchestration.
- `scripts/check-layout.py` - architecture diagram parity.
- `scripts/check-r1.py` - Arrow public-API containment.
- `scripts/check-plan-measure.py` - rewrite termination measure.
- `scripts/check-model-constants.py` - source-to-article numeric drift guard.
- `scripts/check-todo-refs.py` - source-level backlog citation resolution with an empty debt baseline.
- `scripts/check-tex-safe.py` - pdflatex-unsafe character detection.
- `scripts/check-unsafe-count.py` - verifies the `unsafe`-block and `unsafe fn` counts quoted by one backlog entry against the tree.
- `scripts/check-docs-selfcontained.py` - source-path references in `docs/`, with a baseline whose key is now printed beside each site.
- `scripts/fmt-scoped.py` - retired from routine use and deliberately kept for a baseline reformat event.
- `yesno-c/gate.sh` - separate-workspace C ABI gate, run in CI by the `c-abi` job.
- `scripts/gate-pg.sh` - the Bazel PostgreSQL gate, obligatory for `yesno-core` and `yesno-flight` changes.
- `.github/workflows/ci.yml` - CI jobs.
- `.github/workflows/release.yml` - gated multi-architecture image publication.
- `dist/{build.Dockerfile,Dockerfile,entrypoint.sh}` - binary cross-build, image assembly, and multi-call dispatch.
- `scripts/{build-release-image.sh,check-image-binaries.py}` - local/CI build driver and binary-list drift guard.

## Test Coverage

Remote workflow composition is a separate test surface. A local reproduction can validate commands and toolchains, but only a remote run can validate triggers, reusable-workflow contexts, concurrency groups, action versions, caches, and runner capabilities.
Every checker should have at least one known-positive case and a deliberate negative case. Gate completeness is asserted by declared expected step counts for each current mode; changing the step list requires changing and sabotage-checking that declaration. A full gate run found both an expected-step mismatch and a `yesno-e2e` library failure that an individually assembled command list could not reach.

## Pitfalls

- Do not report a gate result from a run that predates shared changes.
- In a shared checkout, first check ownership and modification time when a gate fails in an untouched module.
- Do not report a workspace-wide lint result unless the command explicitly selects the workspace.
- Keep the hand-run lint instructions and `scripts/gate.sh` aligned; they are two implementations of the same check.
- Do not run normal and deep Cargo gates concurrently in the same checkout.
- Do not infer that a checker inspected its subject from its successful exit alone.
- Do not add a whole-tree check that is red on the accepted baseline.
- Do not describe an exported BuildKit layer cache as a Cargo compilation cache; changed source still recompiles.
- Do not derive a reusable workflow's concurrency namespace from `github.workflow`; the caller controls that context.
- Do not report CI fixed until the remote trigger path and complete job graph have succeeded.
- Do not call ASan or Valgrind coverage of heap accesses coverage of an mmap-backed logical slice.
- Do not restore a dangling backlog pointer without first verifying the claim it used to reference against the current tree.
- Do not replace `scripts/gate.sh` with a remembered list of its commands; the script's composition and verdict are checks too.
- Do not infer that a successful multi-platform build put each architecture's binaries in the matching manifest entry; inspect both.
- Do not treat a gate that exists as coverage; ask what runs it automatically, in which arch, with which features.
- Do not compare `scripts/gate.sh` and `ci.yml` by step name. Names are free text on both sides; compare commands.
- Do not trust a count printed by a single `cargo doc` pass: it fails fast per crate, so the number is a lower bound.
- Do not add a whole-tree lint step while its baseline is non-zero; a 30-error baseline cannot answer whether your change added one.
- Do not let an exclusion be silent, and do not report one from inside the collection step where its count can never be non-zero.
- Do not restrict a caller sweep by file type: `build-database-artifacts.sh` looked orphaned only because the search read `.sh` / `.yml` / `.md`, while its callers are `e2e/Dockerfile` and an assertion in `yesno-e2e/src/operator.rs`. The `gate-*.sh` family is unreferenced **by design**.
- An uninvoked instrument is not automatically dead. The distinguishing question is not *is it called* but **does its question recur**: `stats.rs` was deleted because its question had been answered, while `scripts/fmt-scoped.py` is retained -- `AGENTS.md` states the `mod`-graph rustfmt hazard but does not solve it, and a bulk reformat, an edition bump or a rustfmt version change recurs. A caller sweep finds both and cannot tell them apart.

