# Skills

Skills expose only name and short description until loaded. Every modification creates a revision with author, reason, source conversations, evaluation, and promotion state. The default creation/improvement policy is `propose`; wire values are `manual`, `propose`, and `automatic`.

Automatic promotion requires deterministic checks, recorded attempts (`attempts > 0`), and non-regression in measured success, errors, and user corrections. LLM preference alone is not evidence. Production Skills are never silently overwritten and can roll back to a previous promoted revision.

## Authorization and lifecycle

Authenticated members of a profile can list and read that profile's global Skills.
Only profile `OWNER` and `ADMIN` users can create or mutate global Skills. For a
workspace Skill, workspace members can read; `OWNER` and `EDITOR` membership can
revise or evaluate; profile `OWNER`/`ADMIN` can promote or roll back. Resources
outside the authenticated profile are deliberately reported as not found.

Lists are metadata-only and bounded to 200 records. Revision content is returned
only by the explicit authorized history operation. Successful create, revision,
evaluation, proposal, promotion, automatic-promotion, and rollback changes have
an audit event in the same transaction.

## Evidence and provenance contract

Evaluation is a single-assignment evidence object containing exactly eight keys:
`deterministic_checks_passed`, `attempts`, `successful_attempts`, `steps`,
`retries`, `errors`, `duration_ms`, and `user_corrections`. Counts are
non-negative and bounded; successful attempts and corrections cannot exceed
attempts. PostgreSQL enforces this contract, so direct SQL cannot bypass it or
rewrite recorded evidence.

Source conversation IDs are limited to 100 unique, non-nil IDs. They must be
accessible, non-deleted conversations in the Skill profile and, for workspace
Skills, in the exact workspace. Submitted order is retained. A concurrent
conversation deletion is serialized against revision creation; historical source
UUIDs are not rewritten after a permitted deletion.

At commit a Skill is either inactive with zero promoted revisions, or has exactly
one promoted revision matching `active_revision`. Schema 15 repairs old invalid
rows and records originals in append-only quarantine tables. Operators must
review quarantine evidence; it must never be updated, deleted, or truncated.

## Errors and limits

Duplicate global or workspace names return `409 conflict` with a generic name
message. Duplicate/invalid sources, malformed evaluation, and bounds violations
return `422 validation`. Repeated evaluation returns `409 conflict`. SQL details,
profile existence outside the caller's profile, and conversation content are
never returned.
