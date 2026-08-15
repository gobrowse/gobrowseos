# ADR 0013: Skill revision lifecycle hardening

- **Status:** accepted
- **Scope:** schema 15 and the existing Skills API

## Decision

PostgreSQL is authoritative for Skill evaluation shape and single assignment,
source provenance scope, promotion consistency, and repair evidence. Migration
0015 repairs legacy rows before installing these checks. Invalid legacy evidence
is retained in append-only `skill_revision_integrity_quarantine`; promotion
repairs are retained in `skill_integrity_quarantine`.

Revision identity, content, author, reason, source IDs, and creation time are
immutable. Evaluation can transition only from `NULL` to one valid JSON object;
a recorded value cannot be rewritten or cleared. Promotion remains mutable, but
at commit a Skill has either no active revision and no promoted row, or exactly
one promoted row matching `active_revision`. Deferred constraint triggers permit
the API's demote/promote/pointer update transaction while rejecting direct SQL
state divergence.

Sources are checked against non-deleted conversations in the Skill's profile
(and, for workspace Skills, the exact workspace). Revision insertion key-share
locks those conversations. Historical provenance remains after an authorized
conversation deletion; deletion and a concurrent revision insert serialize.

Profile-global Skills are readable by authenticated users in the profile but can
be created, revised, evaluated, promoted, or rolled back only by profile
OWNER/ADMIN. Workspace membership is checked before mutation privilege: a
same-profile nonmember is indistinguishable from a nonexistent or foreign
workspace Skill and receives 404; only a visible member with insufficient access
(including global MEMBER with workspace VIEWER access) receives 403. Workspace
membership continues to govern workspace access: OWNER/EDITOR membership
permits revision/evaluation writes only. Profile OWNER/ADMIN alone may promote
or roll back, even for a workspace OWNER/EDITOR member. Evaluation responses serialize
persisted promotion state rather than merely whether this request auto-promoted.
Race tests synchronize on observable PostgreSQL lock waits under bounded timeouts
and assert final state plus attributable audit rows; scheduler timing is not
used as race proof. Duplicate names are the only
expected database conflict mapped to HTTP 409. Invalid source IDs and evidence
are HTTP 422, while unknown database and deferred integrity failures remain
masked server errors.

The lock order is profile/workspace and membership, then Skill, then source
conversation and revision rows, followed by audit in the same transaction.
Submitted source order is preserved; a sorted copy is used only for deterministic
row locking. No sandbox, MCP execution, provider, route, or frontend behavior is
introduced.

## Consequences

Schema 14 deployments must migrate to 15 before the new application starts.
Operators must inspect both quarantine tables and retain their append-only
contents. Rollback requires restoring a pre-schema-15 database; the old binary
must not run against schema 15.
