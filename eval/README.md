# Retrieval comparison

This is a development experiment, not an alternative runtime backend. See
[`examples/retrieval_eval.rs`](../examples/retrieval_eval.rs) for the harness.

Compare Lattice's FTS5 search with two hybrid variants: FTS5 plus MiniLM and FTS5 plus Bekko Static. Both hybrid variants use the same corpus spans, cosine search, 20 candidates per input, and Reciprocal Rank Fusion with `k=60`. This example does not add a production model backend.

Run these commands from the Lattice checkout with MiniLM already cached (`lattice install`). The benchmark downloads Bekko's weights and tokenizer once. All corpus processing and inference happen locally.

```sh
work=$(mktemp -d "${TMPDIR:-/tmp}/lattice-eval.XXXXXX")
mkdir "$work/corpus"
cp -R src "$work/corpus/"
hf download hotchpotch/bekko-static-embedding-v1 tokenizer.json model.safetensors \
  --revision c18167b8f244c1bc6de21ce9b710bac8a29c4615 \
  --local-dir "$work/bekko"

export CARGO_TARGET_DIR="$work/build" RAYON_NUM_THREADS=8
export CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_OPT_LEVEL=3
cargo build --release --locked --offline --bin lattice --example retrieval_eval
(cd "$work/corpus" && "$CARGO_TARGET_DIR/release/lattice" init)
"$CARGO_TARGET_DIR/release/examples/retrieval_eval" \
  "$work/corpus" "$work/bekko" "$CARGO_TARGET_DIR/release/lattice" \
  | tee "$work/results.tsv"
```

The [labels](retrieval-queries.tsv) specify relevant `path::symbol` pairs for exact identifiers, paraphrases, and questions that distinguish related behaviors. Per-query ranks follow label order, with `-` denoting a miss in the top 20. Aggregate recall is averaged over queries. Candidate union recall is measured before fusion and may include up to 40 candidates per hybrid query. nDCG@5 uses binary relevance. The harness checks MiniLM hybrid rankings against the production CLI for each query.

Bekko discovers and parses the raw Rust files with Tree-sitter, then builds its own vector index. To verify that the Bekko arm does not require the MiniLM index or weights, run it with only the raw source and the shared lexical database. Omit the third argument to skip MiniLM retrieval and CLI parity checks:

```sh
mkdir -p "$work/bekko-only/.lattice"
cp -R "$work/corpus/src" "$work/bekko-only/"
cp "$work/corpus/.lattice/meta.db" "$work/bekko-only/.lattice/"
test ! -e "$work/bekko-only/.lattice/vectors.db" && test ! -e "$work/no-minilm" &&
  LATTICE_MODEL_DIR="$work/no-minilm" "$CARGO_TARGET_DIR/release/examples/retrieval_eval" \
    "$work/bekko-only" "$work/bekko"
```

p95 measures five timed runs per query after one warm-up, with models already loaded. It includes FTS5, query embedding, cosine search, and fusion where applicable. It excludes indexing, model loading, freshness checks, subprocess overhead, and output serialization. The MiniLM index is on disk; the temporary Bekko index is in memory. Timings are not an end-to-end CLI comparison. The 24 curated queries cover one Rust codebase and do not establish general code-search quality.
