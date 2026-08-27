---
name: deep-sleep
description: "Consolidate related long-term memory documents under .agents/docs/LTM/ into broader synthesis documents and refresh the LTM index. Use when LTM has grown into overlapping topic files that would be easier to orient in as a smaller set of durable syntheses."
user-invocable: true
allowed-tools: Bash, Read, Write, Edit, Grep, Glob
---

# Deep Sleep: Consolidate Long-Term Memory Documents

This skill reads `.agents/docs/LTM/INDEX.md` and the relevant documents under `.agents/docs/LTM/`, then groups overlapping topics into broader synthesis documents while keeping the source documents intact for traceability.

Use this after `good-sleep` has already distilled chronological journal entries into topic-oriented LTM files and those files now need a second-stage consolidation.

## Goal

Turn a set of narrow LTM notes into a smaller set of durable synthesis documents. Source documents are preserved; the synthesis is an orientation layer on top of them, not a replacement.

## Step 1: Inspect the Existing LTM Set

Read `.agents/docs/LTM/INDEX.md` first, then open only the LTM documents needed to understand the candidate clusters. Do not bulk-load every file unless the LTM set is still small enough that doing so is cheaper than selective reading.

Look for:

- Multiple documents about the same module or subsystem ( containers, streams, codec, serialization, buffers )
- Repeated pitfalls, file references, or test guidance across documents
- One overview document plus several implementation-detail documents that should be summarised together
- A performance narrative spread across several investigations that would read better as one

Natural cluster boundaries in this project tend to follow the architecture rather than the calendar:

- **Representation and size classes** — array/bitmap/run selection, promotion, demotion, hysteresis, `optimize`
- **Set algebra** — the generic kernel, specialization decisions, cardinality identities
- **Streams** — lookahead, `peek_prefix` semantics, `cardinality_dyn` overrides, `Expr` lowering
- **Format fidelity** — codec bytes, the offset-header rule, 32-bit and 64-bit layouts, foreign-file compatibility
- **Buffers and zero-copy** — `U16Store` / `BitStore`, copy-on-write, the `arrow-buffer` containment policy
- **Testing strategy** — oracle design, generator bias, allocation budgets, benchmark methodology

## Step 2: Propose Consolidation Clusters

Before writing, present a plan to the user with:

- The synthesis documents you propose to create
- The source LTM documents that feed each synthesis document
- Any source documents that should remain standalone because they are already cohesive

Cluster by durable topic, not by date and not by arbitrary file-count balancing.

## Step 3: Write Synthesis Documents

Create new synthesis documents under `.agents/docs/LTM/` using descriptive kebab-case filenames such as:

- `container-representation-synthesis.md`
- `stream-evaluation-and-cardinality-synthesis.md`
- `format-fidelity-synthesis.md`

Keep the original source LTM documents. Do not delete or overwrite them unless the user explicitly asks for replacement.

Use this structure:

```markdown
# <Synthesis Title>

## Summary

<2-4 sentence overview of the merged topic and why it matters>

## Included Documents

| Document | Focus |
|----------|-------|
| [source-a.md](./source-a.md) | <short note> |
| ... | ... |

## Stable Knowledge

<Bulleted list of the durable facts, constraints, and design decisions>

## Operational Guidance

<How an agent should approach work in this area>

## Files

<Important file paths and why they matter>

## Tests

<Which test layer covers this area, and the command to run it>

## Pitfalls

<Failure modes, tricky assumptions, and gotchas>
```

## Step 4: Synthesize, Do Not Merely Concatenate

When merging documents:

- Deduplicate repeated explanations
- Convert chronological narratives into timeless guidance
- Preserve exact file paths, constant names, function names, benchmark numbers, and test names when they are useful
- Keep contradictions visible; if two source docs disagree, call that out explicitly instead of silently choosing one

Prefer compact synthesis over exhaustive restatement. The new document should help future agents orient quickly and then drill into the source docs only when needed.

## Step 5: Watch for Material That Belongs in the Canonical Docs

Some durable knowledge outgrows LTM. While consolidating, flag ( do not write ) anything that belongs in:

- `.agents/docs/ARCHITECTURE.md` — a stable structural fact, an invariant, or a contract that constrains future changes
- `.agents/docs/OVERVIEW.md` — scope, boundaries, or a milestone-level statement
- `.agents/docs/QUALITY_GATE.md` — a convention or check that should apply to every future change

Promotion into those three documents is the `distill-memories` skill's job, not this one's. List the candidates at the end of your run so the user can invoke it. Keeping the two skills separate is what stops a consolidation pass from quietly rewriting the canonical docs.

## Step 6: Refresh the Index

Update `.agents/docs/LTM/INDEX.md` so it clearly distinguishes:

- Synthesis documents ( with the source documents each consolidates )
- Source topic documents

Both tables already exist in the index. Fill them; do not restructure the file.

## Step 7: Record the Consolidation

Append a short note to `.agents/docs/JOURNAL.md` describing:

- Which synthesis documents were created
- Which source LTM documents they consolidate
- Any documents intentionally left standalone
- Any candidates flagged for `distill-memories`

Do not edit or delete existing journal sections. Only append.

## Guardrails

- Do not delete source LTM documents without explicit user approval
- Do not collapse unrelated topics just to reduce file count
- Prefer a few high-value synthesis documents over a full rewrite of the entire LTM tree
- Preserve the documentation style rules used in repo-authored docs: half-width parentheses and half-width colons
- Do not edit `ARCHITECTURE.md`, `OVERVIEW.md`, or `QUALITY_GATE.md` from this skill

## Notes

- This skill complements `good-sleep`; it does not replace it
- Re-running this skill should extend or refresh synthesis documents when new source LTM files appear
- If the current LTM set is already small and non-overlapping, say so and avoid forced consolidation
