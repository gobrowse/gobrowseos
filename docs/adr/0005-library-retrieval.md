# ADR 0005: Hybrid Library Retrieval

Status: Accepted

Retrieve lexical and vector candidate sets independently, filter by authorized metadata before ranking, normalize component scores, and combine them with weighted reciprocal rank fusion plus recency/source/workspace signals. Optional reranking is a final provider-neutral stage. Search returns snippets and IDs; full content requires an explicit get.
