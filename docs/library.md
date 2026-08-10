# The Library

The Library stores durable context as Books. Canonical bodies, immutable revisions, chunks, links, provenance, trust, scope, and security classification are independent. Chunking never destroys the body. An embedding model change marks affected vectors stale and schedules re-embedding.

Search uses authorized metadata filters before lexical and semantic candidate retrieval. Weighted reciprocal-rank fusion combines those rankings with bounded recency, source, and workspace signals. Optional rerankers operate only on the small fused candidate set. Search returns IDs, snippets, ranking metadata, provenance, and trust; clients explicitly load full Books.

The Autobiography is one profile-scoped Book with a database and domain limit of 99,999 Unicode characters. Its default update policy is `propose`. Proposals retain before, after, reason, and source references. Accepted changes still produce a Book revision.
