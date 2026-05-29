-- Trove's history + search side of the substrate.
--
-- This schema lives in the SAME Postgres as JuiceFS's own metadata (its
-- `jfs_*` tables). JuiceFS holds the live tree; version *bytes* are COW clones
-- inside JuiceFS at `/.trove/versions/<hash>` (no duplication). This schema
-- holds only what JuiceFS doesn't: the per-file version chain (history / diff /
-- restore) and the semantic-search embeddings. Everything hangs off a
-- content-addressed blob registry, so identical content is recorded once.

-- NOTE: the `vector` extension is created by `trove install` in a shared
-- location BEFORE this migration runs — it's database-global, not per-volume, so
-- it must not live in (or be dropped with) a volume's schema. This migration
-- runs with `search_path` pointed at the volume's own schema, so the tables
-- below are created there, isolated from `public`.

-- Content-addressed registry: one row per unique file content (sha256). The
-- bytes themselves are NOT here — they're a COW clone in JuiceFS keyed by this
-- hash. This row exists so the version chain can reference content and so
-- `blob_chunks` can hang embeddings off it. Dedup is free: same bytes => one row.
create table blobs (
    hash        text primary key,           -- sha256 hex of the content
    size        bigint not null,
    created_at  timestamptz not null default now()
);

-- Per-path version chain. Append-only: every validated write through the
-- mount's commit barrier appends a row. `rev` is monotonic per path; the head
-- is the largest rev. `parent_rev` links the chain for diffs/restore; null marks
-- a path's first version. A version is (path, rev) -> blob_hash; its bytes are
-- the JuiceFS clone `/.trove/versions/<blob_hash>`.
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

-- Embeddings, one blob -> many chunks. A text file is split (by markdown
-- heading, etc.) into ordered chunks, each embedded independently so search
-- matches the relevant *section*, not the whole file. Whole-file embedding is
-- simply the N=1 case (one chunk, null heading). `start_byte`/`end_byte` locate
-- the chunk in the blob for deep-linking. Filled out-of-band by the server-side
-- `trove embed` worker (libjfs read + OpenAI); "needs embedding" = a blob with
-- no rows here. (Step 7, deferred.)
create table blob_chunks (
    id              bigint generated always as identity primary key,
    blob_hash       text    not null references blobs (hash),
    ordinal         integer not null,            -- order within the blob (0,1,2…)
    heading         text,                         -- section heading (null = whole file / preamble)
    start_byte      integer not null,
    end_byte        integer not null,
    embedding       vector(3072),                 -- OpenAI text-embedding-3-large
    embedding_model text,
    unique (blob_hash, ordinal)
);

-- ANN index for `trove search`. pgvector's hnsw caps the `vector` type at 2000
-- dimensions, so index the halfvec cast (supported to 4000). Full 3072-dim
-- vectors are still stored; only the index is halfvec. Cosine matches OpenAI.
create index blob_chunks_embedding_hnsw on blob_chunks
    using hnsw ((embedding::halfvec(3072)) halfvec_cosine_ops);
