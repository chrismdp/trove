# Provision the backend with Stripe Projects (optional)

Trove needs two pieces of infrastructure: a **Postgres database** (metadata,
version history, embeddings) and an **S3-compatible object store** (the file
data). Normally you create those yourself — a Supabase project, a Cloudflare R2
bucket — and hand the credentials to [`trove install`](/docs/quickstart).

[Stripe Projects](https://docs.stripe.com/projects) is an optional shortcut.
Despite the name it isn't a payments tool: it's a CLI (open beta) that
**provisions third-party infrastructure** — databases, object storage, hosting,
AI providers — across many vendors from one place, and writes the credentials
into a local `.env`. It's built for coding agents, which makes it a natural
front-end for trove's own [non-interactive install](/docs/quickstart).

> **Trove does not depend on Stripe Projects.** This is just one way to stand the
> backend up quickly; the [manual path](/docs/quickstart) and the
> [agent/env path](/docs/running) work exactly the same. Pick whichever suits.

## The shape of it

The exact `add` targets and the variable names that land in `.env` come from
`stripe projects catalog` and your generated `.env` — treat the names below as
illustrative, not literal.

```bash
# 1. Provision the backend (open beta — `stripe projects catalog` lists providers)
stripe projects init trove
stripe projects add supabase/database      # Postgres: metadata + versions + embeddings
stripe projects add <storage>/bucket       # an S3-compatible object store (see below)
stripe projects add openai/api-key         # optional: embeddings + `trove search`

# 2. Stripe Projects writes credentials to .env under provider-prefixed names.
#    Map them to the names trove reads (check your generated .env for the
#    exact left-hand names):
export TROVE_VERSIONS_DB="$SUPABASE_DATABASE_URL"
export TROVE_R2_BUCKET="$STORAGE_BUCKET_ENDPOINT"      # full https://… endpoint URL
export R2_ACCESS_KEY_ID="$STORAGE_ACCESS_KEY_ID"
export R2_SECRET_ACCESS_KEY="$STORAGE_SECRET_ACCESS_KEY"

# 3. Install trove — no TTY needed; it reads everything from the environment
trove install
trove doctor          # confirm all green
```

That's the point of the pairing: Stripe Projects *produces* the credentials, and
`trove install` *consumes* them with no prompts. An agent can run the two back to
back and bring a vault up end to end — including creating the database and the
bucket, which it couldn't do on its own before.

## Two things to check first

1. **Your storage provider must be S3-compatible** and hand you an **access key +
   secret key + endpoint URL**. Trove's storage layer speaks the S3 API, so
   anything S3-compatible works (Cloudflare R2, AWS S3, MinIO) — but confirm the
   provider you pick in `stripe projects catalog` gives you those three values. If
   it only hands back a bearer token or a vendor SDK, it can't serve as trove's
   object store.
2. **Stripe Projects is open beta.** Fine for spinning things up; treat the
   provider list, the emitted variable names, and any future pricing as subject to
   change. Your credentials also live in Stripe's vault — if you'd rather they
   didn't, use the manual path.

## Why the remap step?

Trove reads `TROVE_VERSIONS_DB` (or `DATABASE_URL`), `TROVE_R2_BUCKET`,
`R2_ACCESS_KEY_ID`, and `R2_SECRET_ACCESS_KEY` — see
[the three secrets](/docs/quickstart#the-minimum-three-secrets). Stripe Projects
emits provider-prefixed names like `SUPABASE_DATABASE_URL`. The four `export`
lines above bridge the two. Once `trove install` has run, the non-secret values
live in `~/.config/trove/config.toml` and the secrets in your environment, so
later commands (`trove mount`, `trove doctor`, …) need no remap.

## Next

- Back to the [Quickstart](/docs/quickstart) for what `trove install` does.
- [Running it end-to-end](/docs/running) for the full operating manual.
