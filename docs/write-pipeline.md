# The write pipeline

The one sequence that defines Trove. Everything else is plumbing for this.

A single `write()` + `close()` from any program through the mount flows:

```
kernel write()
   ↓
FUSE forwards to TroveFs::write(fh, off, data)
   ↓
Inner.open_files[fh] dispatches by handle kind:
   ├─ Read         → reply with EINVAL (read-only handle)
   ├─ PassThrough  → fs.pwrite() — streams straight to JuiceFS, no buffer
   └─ Write        → buf.splice(off..off+data.len(), data); dirty = true
   ↓
   (more writes, possibly)
   ↓
kernel close()  → FUSE fires `flush`
   ↓
Inner.barrier(fh):
   ├─ PassThrough → writer.flush() + fsync() + version_pass_through()
   └─ Write       → commit(fh)
   ↓
   commit():
   1. validate(path, &buf):
      • parse frontmatter
      • registry.select(path, type) → schemas
      • for each: validate_against(frontmatter, schema)
      • aggregate violations
   ↓
   ┌── valid ────────────────────────────────────────────────┐
   │ 2. fs.write_all(path, &buf, 0o644)   — atomic in jfs    │
   │ 3. fs.unlink(<path>.errors)          — clear sidecar    │
   │ 4. record_version(fs, vs, path, &buf):                  │
   │      a. ensure /.trove/versions exists                  │
   │      b. clone_file(path, /.trove/versions/<hash>, true) │
   │      c. versions.record_meta(path, hash, size, author)  │
   │ 5. if embed_tx: tx.send((hash, buf.clone()))            │
   │ 6. handle.dirty = false; rejected = false               │
   │ 7. return Ok(())                                        │
   └─────────────────────────────────────────────────────────┘
   ┌── invalid ──────────────────────────────────────────────┐
   │ 2. fs.write_all(<path>.errors, report)                  │
   │ 3. handle.rejected = true                               │
   │    (so inflight_size returns None and the phantom file  │
   │     disappears immediately, before async release fires) │
   │ 4. return Err(EINVAL)                                   │
   └─────────────────────────────────────────────────────────┘
   ↓
kernel surfaces ok / EINVAL to the calling program
```

## Five properties worth pinning to the wall

### 1. Validation is at the commit barrier, not on every `write()`

A single logical file save is often dozens of FUSE `write()` calls (the
kernel chunks). Validating on each one would be insane — half-written
YAML is always invalid. We buffer the whole file in the handle, and run
validation **once, at the barrier** (FUSE `flush` on `close()`, or an
explicit `fsync`).

This is why governed files are buffered whole. The buffer is the
"proposed file" — a candidate that exists nowhere yet, including JuiceFS.

### 2. A rejected write **does not persist anything**

If validation fails:

- `fs.write_all(path, ...)` is **not called** — nothing reaches JuiceFS.
- The handle stays in memory marked `rejected`.
- The `.errors` sidecar is written.
- `EINVAL` is returned.
- When `release` eventually drops the handle, the buffer is dropped too.

The previous contents of the file (if any) survive untouched, because
nothing was ever written.

### 3. Version capture is best-effort, never blocking

`record_version` is called **after** `fs.write_all`. If the version
capture fails (Postgres hiccup, transient libjfs error), we log it and
move on. The live tree is the source of truth; the version archive is a
derived index.

This is a deliberate trade. Stronger guarantees would mean wrapping the
file write and the chain row in a single transaction, which requires
Postgres to be up before any write can succeed. We chose availability over
consistency for history capture — your writes never fail because the
history index is briefly broken.

### 4. Embedding is fire-and-forget off the write path

The line that triggers embedding:

```rust
if let Some(tx) = &self.embed_tx {
    let _ = tx.send((sha256_hex(&buf), buf.clone()));
}
```

`tx.send` on an unbounded `mpsc::Sender` is non-blocking. The background
thread picks it up when it can. The `commit()` call returns immediately —
the OpenAI round-trip never sits on the write path.

If the thread crashes (oom, panic), subsequent sends still succeed; they
just never get picked up. A `trove embed` backfill catches up later.

### 5. Read paths are coherent

A read while a write is in flight goes straight to JuiceFS via the `Read`
handle. It does **not** see the in-progress write buffer (that buffer
lives only in the writer's handle). Once the writer commits, the next
read sees the new content. Standard POSIX read semantics; no surprises.

## Walk-through with a real file

Agent writes `~/vault/people/alice.md`:

```yaml
---
type: person
name: Alice
dob: "1990-01-15"
---

Alice is a person.
```

What happens:

1. **`open(path, O_WRONLY|O_CREAT)`** — FUSE `create`. `may_govern()` says
   yes (`people/**.md` is glob-claimed). Handle kind: `Write`, buf empty,
   dirty true.
2. **Several `write()` calls** — the kernel splits the file content into
   FUSE-sized chunks. Each splices into `buf`. dirty stays true.
3. **`close()`** — FUSE `flush`. `barrier` → `commit`.
4. **`validate(path, &buf)`**:
   - `frontmatter::parse(buf as &str)` → `{"type":"person","name":"Alice","dob":"1990-01-15"}`
   - `registry.select(rel, Some("person"))` → 1 schema (`person.json`)
   - `validate_against(&fm, &schema)` → Ok
5. **`fs.write_all(path, &buf)`** — bytes land in JuiceFS.
6. **`fs.unlink("alice.md.errors")`** — no-op (no sidecar existed).
7. **`record_version(fs, vs, path, &buf)`**:
   - `hash = sha256(buf)` = e.g. `a3f9…`
   - `clone_file("/people/alice.md", "/.trove/versions/a3f9…", true)`
     (COW, zero bytes copied)
   - `record_meta(path, hash, size, None)` → rev 1
8. **`tx.send((hash, buf))`** — embed thread picks it up. Splits into 1
   chunk (single paragraph). OpenAI call → 3072-dim vector. Inserts into
   `blob_chunks`.
9. **`return Ok(())`** — kernel returns success to the agent. `close()`
   returns 0.

A `trove log /people/alice.md` now shows rev 1. A `trove search "Alice"`
returns the file. The agent did nothing special — just `write` and
`close`.

## The same, but invalid

If `dob: not-a-date`:

1-4. As before, up to `validate`. `validate_against` returns one
violation: `/dob: "not-a-date" is not of type "string" matching format
"date"`.
5. **`fs.write_all("alice.md.errors", report)`** — sidecar written.
6. **`handle.rejected = true`**.
7. **`return Err(EINVAL)`**.

The agent's `close()` returns -1, `errno = EINVAL`. It reads
`alice.md.errors` to find out why, fixes the date, and tries again.

The original file (if it existed) is unchanged. The previous rev is the
latest rev. No history pollution from invalid attempts.

**Tool-dependent visibility.** The `EINVAL` is returned at the kernel
`close()` boundary. `bash` checks the return value of `close()` on its
`>` redirects and prints `bash: echo: write error: Invalid argument`.
Many programs (`echo`, `cat`, `cp` builtins in some shells; ad-hoc
scripts that don't check close errors) will exit 0 and look like they
succeeded. The `.errors` sidecar is what every tool can read; trust it,
not the exit code.

Next: [How COW versioning works →](/docs/cow-versions)
