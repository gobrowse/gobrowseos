# ADR 0010: Leased, Revision-Fenced Embedding Jobs

Status: Accepted

Embedding jobs target an immutable Book revision and are claimed with `FOR UPDATE SKIP LOCKED`, a unique lease token, and an expiry. Workers renew leases between bounded batches. Final publication locks the job and Book, verifies the lease token, expiry, and revision, then writes per-model chunk vectors and marks the job complete in one transaction. Expired leases are retried with limits; stale revisions are canceled and the current revision is queued.
