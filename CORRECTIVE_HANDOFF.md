# Developer-agent handoff

Read `FABRIC_CORRECTIVE_SPEC.md` in full. Treat it as a corrective milestone for the existing Rust code at baseline `8e926f9`, not a greenfield rewrite.

Your job is to **keep the proven safe lifecycle, simplify user-facing harness/profile concepts, and implement an asynchronous local worker API used naturally from OMP/om-pstack**. Do not add cloud/BYOC or a generic workflow product.

1. Begin with C-00 audit and evidence. Do not assume all previously reported tests are reproducible.
2. Work one testable acceptance criterion at a time in C-01 through C-07 order, adapting dependencies as needed.
3. Write a failing test before each behavior change; implement minimally; run focused and repository-wide checks.
4. Preserve old API behavior until a versioned migration is tested. Never delete safety guarantees just to simplify the UX.
5. Report `IMPLEMENTED`, `TESTED_WITH_FAKE`, `LIVE_VERIFIED`, `NOT_VERIFIED`, or `BLOCKED` precisely.
6. Do not claim cloud or full pstack parity while no remote backend exists. Never silently use local when cloud was required.
7. Stop and document blockers instead of inventing mock success, copying credentials or making unapproved paid model requests.

Desired final demonstration: `fabric init` followed by an OMP-controlled local 4-worker `/swarm` task, with fast submission, durable results, and verifiably safe cleanup. The controlling OMP process may exit and results must still be available.
