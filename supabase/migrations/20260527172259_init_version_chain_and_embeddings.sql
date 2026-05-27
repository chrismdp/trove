-- Trove's history + search side of the substrate.
--
-- JuiceFS (R2 blobs + its own Postgres metadata) holds the *live* tree. This
-- schema holds what JuiceFS does not: a per-file version chain (history, diffs,
-- restore) and semantic-search embeddings. Both hang off a content-addressed
-- blob registry, so identical content — across paths or revisions — is stored
-- and embedded exactly once.

-- pgvector for semantic search over file contents.
create extension if not exists vector;

-- Content-addressed blob registry. One row per unique file content (sha256).
-- Dedup falls out free: the same bytes under two paths/revs => one blob row and
-- one embedding. The embedding is filled asynchronously after a commit, so it
-- is nullable; `embedding_model` records which model produced it (so a model
-- change is detectable and re-embeddable).
create table blobs (
    hash            text primary key,           -- sha256 hex of the content
    size            bigint not null,
    embedding       vector(3072),               -- OpenAI text-embedding-3-large
    embedding_model text,                        -- model id that produced `embedding`
    created_at      timestamptz not null default now()
);

-- Per-path version chain. Append-only: every validated write through the
-- mount's commit barrier appends a row. `rev` is monotonic per path; the head
-- is the row with the largest rev. `parent_rev` links the chain for diffs and
-- restore; null marks a path's first version. The content lives in `blobs`,
-- content-addressed, so a version is just (path, rev) -> blob_hash.
create table file_versions (
    id          bigint generated always as identity primary key,
    path        text    not null,
    rev         integer not null,
    blob_hash   text    not null references blobs (hash),
    parent_rev  integer,                         -- null = first version of `path`
    author      text,
    size        bigint  not null,
    created_at  timestamptz not null default now(),
    unique (path, rev)
);

-- Head-of-chain and history lookups: `... where path = $1 order by rev desc`.
create index file_versions_path_rev_desc on file_versions (path, rev desc);

-- ANN index for `trove search`. pgvector's hnsw caps the `vector` type at 2000
-- dimensions, so index the halfvec cast (supported to 4000 dims). Full 3072-dim
-- vectors are still stored on `blobs.embedding`; only the index is halfvec.
-- Cosine distance matches OpenAI embedding similarity.
create index blobs_embedding_hnsw on blobs
    using hnsw ((embedding::halfvec(3072)) halfvec_cosine_ops);
