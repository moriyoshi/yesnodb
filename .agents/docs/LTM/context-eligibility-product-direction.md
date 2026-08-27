# Context Eligibility Product Direction

## Summary

`yesno` is best positioned as an operational context-eligibility plane: it evaluates exact, continuously changing membership policy over candidate identifiers while leaving ranking, content retrieval, and authoritative records to other systems. The differentiated product is governed composition across independently owned predicates, with versioned evidence and disclosure controls, rather than a general bitmap DBaaS, OLAP engine, vector database, or authorization graph.

## Key Facts

- The first product primitive should be candidate filtering, not unrestricted enumeration: `filter_candidates( candidates, policies, minimum_versions ) -> mask + receipt`.
- A governed share is a capability binding owner, grantee, expression, identity namespace, permitted operation, output policy, expiry, revocation state, and version policy.
- Cross-owner algebra is meaningful only when every party agrees what an ordinal identifies.
- Candidate-only filtering reduces disclosure but does not prevent singleton probing, differenced counts, or repeated-query reconstruction by itself.
- A decision receipt needs a version vector across independently updated sources; one invented global version would claim atomicity that does not exist.
- Commit-version acknowledgements and exact-version reads make part of the receipt possible, but minimum-version reads, idempotency keys, and conditional writes remain prerequisites.
- The recommended commercial sequence is self-hosted evaluation, vendor-supported BYOC, dedicated managed instances, and multitenancy only after the operational contract is measured.

## Details

### Product boundary

The engine owns exact Boolean eligibility over application-defined memberships. In a retrieval flow such as `candidates AND tenant_access AND publisher_ai_license AND region_policy AND NOT revoked`, the vector or text engine still ranks, the object store still owns content, and `yesno` decides which identifiers may participate. This keeps the service retrieval-engine-neutral and avoids importing document storage, similarity search, or general row semantics into the set engine.

The differentiated feature is composition across owners. A consumer, publisher, and regulator may each control a predicate, and the result can be useful without any party exporting its complete population. Portable bitmap support remains a mechanism, not the product boundary: an exported copy cannot be revoked and therefore represents a categorically broader capability than an online filtered result.

### Identity and governed shares

A share must be an explicit publisher-owned capability, never a foreign numeric key. It binds the parties, the published expression, the identifier namespace, allowed operations and outputs, expiry, revocation, and version policy. Authorization must be checked again when delivering a result so an old Flight ticket, snapshot lease, or cached plan cannot outlive revocation.

Cross-tenant ordinal equality requires a shared identity namespace. The first version should accept natural public identifiers or platform-issued identifiers whose equality both parties can verify. Hashing enumerable personal identifiers does not create privacy, and a general identity-resolution service or cryptographic private-set intersection is a separate product decision.

### Disclosure boundary

Returning only positions within caller-supplied candidates is safer than exposing enumeration or arbitrary counts, but repeated singleton candidates, complements, and differenced results can still reconstruct a protected population. The initial cross-tenant surface should therefore exclude unrestricted enumeration, export, complement, and counts; impose candidate and result thresholds; constrain expression shapes; and apply query budgets, rate limits, purpose binding, and audit.

Differential privacy and private-set-intersection protocols should not be introduced without a concrete disclosure budget and trust model. They do not substitute for defining the first capability narrowly.

### Versions, receipts, and write semantics

A receipt should identify the consumer snapshot, every share and share version, the authorization grant, evaluation time, and every source-ingestion watermark. Independently updated tenants have no naturally atomic global version, so the receipt carries a version vector. The first source adapter should expose a durable mapping from a PostgreSQL outbox or Kafka offset to a `yesno` version, together with reconciliation and rebuild procedures.

The transport can now return a commit version and issue an exact read at that version after waiting for visibility. That makes a commit identity observable, but it does not finish the product contract. A minimum-version request is distinct from an exact-version request after reclamation; retries remain ambiguous without an idempotency key; and a blind whole-key replacement remains unsafe without a conditional-write precondition. These are prerequisites to governed automation, not parallel conveniences.

### Deployment sequence

The initial supported envelope should be vendor-supported BYOC over rebuildable derived membership data. Authoritative objects remain in the customer's systems, while the deployment contract names the supported cloud, filesystem, upgrade, backup, fencing, recovery, and operator responsibilities. Dedicated deployments should produce measured workload limits, p99 latency, RPO/RTO, tenant-isolation evidence, and a working release pipeline before a multitenant DBaaS is attempted.

The durable advantage is the trust protocol and ecosystem: publisher-controlled shares, consumer-constrained filtering, identity namespaces, revocation-aware versions, source watermarks, decision receipts, and adapters across retrieval systems. Roaring compatibility, Arrow transport, exact cardinality, and SIMD kernels support that protocol but are individually copyable mechanisms.

## Files

- `.agents/docs/TODO.md` - open contract, identity, sharing, disclosure, receipt, and BYOC design items.
- `yesno-flight/` - commit acknowledgements, exact-version requests, tickets, and snapshot leases.
- `yesno-wire/` - versioned request and expression encodings.
- `yesno-core/src/db/` - commit versions, visibility waits, snapshots, and conditional-write boundary.
- `yesno-tantivy/` and `yesno-search-java/` - examples of keeping ranking outside the eligibility engine.
- `README.md` and `docs/operations.md` - human-facing product limits and deployment responsibilities.

## Test Coverage

- Concurrency tests demonstrate that a returned commit may be temporarily above the visible prefix and that an interleaved retry can undo another client's intent.
- Flight tests pair the same exact-version request with visibility waiting enabled and disabled, proving that the wait changes only the transient refusal.
- Wire and client tests pin legacy and versioned acknowledgement widths and exact-version request encoding.
- Any future service contract needs an end-to-end layer that can exercise authorization, revocation, identity agreement, disclosure limits, and receipt contents independently of the implementation.

## Pitfalls

- Do not present `yesno` as the ranking engine, content authority, general row store, or authorization graph.
- Do not equate foreign numeric ordinals without a shared identity namespace.
- Do not treat hashes of enumerable identifiers as a privacy boundary.
- Do not treat candidate-only output as proof against repeated-query disclosure.
- Do not let snapshot leases or cached plans bypass revocation checks at result delivery.
- Do not describe online filtering, population enumeration, and portable export as one capability.
- Do not invent a single global version across independently updated owners.
- Do not build catalog or UI machinery over ambiguous retries and blind replacement writes.

