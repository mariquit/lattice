// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Compare FTS5 and two hybrid retrievers on the Rust source dogfood corpus.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use candle_core::{DType, Device};
use rusqlite::{params, Connection};
use lattice::embed::CandleEmbedder;
use lattice::fts::{self, Filters};
use lattice::fuse::{rrf, RRF_K};
use lattice::vectors::Embedder;
use tokenizers::Tokenizer;

const DEPTH: usize = 20;
const BEKKO_DIM: usize = 512;
const ROUNDS: usize = 5;

type Rank = Vec<String>;
type Result<T> = lattice::Result<T>;

struct Bekko {
    tokenizer: Tokenizer,
    weights: Vec<f32>,
}

impl Bekko {
    fn load(dir: &Path) -> Result<Self> {
        let bytes = fs::read(dir.join("model.safetensors"))?;
        let table = candle_core::safetensors::load_buffer(&bytes, &Device::Cpu)?;
        let weight = &table["embedding.weight"];
        assert_eq!(weight.dims(), &[200_000, 1024]);
        let weights = weight
            .narrow(1, 0, BEKKO_DIM)?
            .to_dtype(DType::F32)?
            .flatten_all()?
            .to_vec1()?;
        Ok(Self {
            tokenizer: Tokenizer::from_file(dir.join("tokenizer.json"))
                .map_err(|e| format!("load Bekko tokenizer: {e}"))?,
            weights,
        })
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // StaticEmbedding does not insert special tokens. Accumulate in f32
        // and normalize: scaling by token count cancels under L2 normalization.
        let enc = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| format!("tokenize with Bekko: {e}"))?;
        let mut pooled = vec![0.0_f32; BEKKO_DIM];
        for &id in enc.get_ids() {
            let offset = id as usize * BEKKO_DIM;
            for (out, &v) in pooled
                .iter_mut()
                .zip(&self.weights[offset..offset + BEKKO_DIM])
            {
                *out += v;
            }
        }
        let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            pooled.iter_mut().for_each(|v| *v /= norm);
        }
        Ok(pooled)
    }
}

fn key(path: &str, name: &str, kind: &str) -> String {
    if name.is_empty() || matches!(kind, "file" | "module") {
        path.to_string()
    } else {
        format!("{path}::{name}")
    }
}

fn lexical(store: &Path, text: &str) -> Result<Rank> {
    let filters = Filters {
        limit: DEPTH,
        exclude_tests: false,
        kind: None,
    };
    Ok(fts::search(store, text, &filters)?
        .into_iter()
        .map(|h| key(&h.path, &h.name, &h.kind))
        .collect())
}

fn vector(db: &Connection, vector: &[f32], count: usize) -> Result<Rank> {
    let mut stmt = db.prepare_cached(
        "SELECT d.path, d.symbol, d.kind, d.start_line, v.distance
         FROM vec_documents v JOIN documents d ON d.id = v.rowid
         WHERE v.embedding MATCH ?1 AND k = ?2 ORDER BY v.distance ASC",
    )?;
    // Match the production cosine search and its oversampling before dedup.
    let rows = stmt.query_map(
        params![serde_json::to_string(vector)?, count.min(DEPTH * 4)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, usize>(3)?,
                r.get::<_, f64>(4)?,
            ))
        },
    )?;
    let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    hits.sort_by(|a, b| {
        let score = |distance: f64| 1.0_f32 - distance as f32;
        score(b.4)
            .total_cmp(&score(a.4))
            .then(a.0.cmp(&b.0))
            .then(a.3.cmp(&b.3))
    });
    let mut seen = BTreeSet::new();
    Ok(hits
        .into_iter()
        .filter_map(|(p, n, k, _, _)| seen.insert((p.clone(), n.clone())).then(|| key(&p, &n, &k)))
        .take(DEPTH)
        .collect())
}

fn hybrid(store: &Path, lexical: Rank, vector: Rank) -> Result<(Rank, Rank)> {
    let candidates = lexical.iter().chain(&vector).cloned().collect();
    let mut fused = rrf(&[lexical, vector], RRF_K);
    let names: Vec<_> = fused
        .iter()
        .filter_map(|h| h.key.rsplit_once("::"))
        .map(|(path, name)| (path.to_string(), name.to_string()))
        .collect();
    let degrees = fts::in_degrees(store, &names)?;
    let degree = |key: &str| degrees.get(key).copied().unwrap_or_default();
    let name = |key: &str| {
        key.rsplit_once("::")
            .map(|(_, n)| n)
            .unwrap_or(key)
            .to_string()
    };
    fused.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(degree(&b.key).cmp(&degree(&a.key)))
            .then_with(|| name(&a.key).cmp(&name(&b.key)))
            .then(a.key.cmp(&b.key))
    });
    Ok((
        fused.into_iter().take(DEPTH).map(|h| h.key).collect(),
        candidates,
    ))
}

/// Build the Bekko index from raw Rust files, without opening any MiniLM store.
fn index_bekko(root: &Path, model: &Bekko) -> Result<(Connection, BTreeSet<String>, usize)> {
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *const std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    }
    let mut db = Connection::open_in_memory()?;
    db.execute_batch(&format!(
        "CREATE TABLE documents (id INTEGER PRIMARY KEY, path TEXT, symbol TEXT, kind TEXT, start_line INTEGER);
         CREATE VIRTUAL TABLE vec_documents USING vec0(embedding float[{BEKKO_DIM}] distance_metric=cosine);"
    ))?;
    let mut files = Vec::new();
    lattice::index::collect_source_files(root, root, &[], &mut files)?;
    files.sort();
    let tx = db.transaction()?;
    let mut known = BTreeSet::new();
    let mut count = 0;
    for path in files {
        let relative = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        if !relative.ends_with(".rs") {
            return Err(
                format!("benchmark corpus must contain only Rust source: {relative}").into(),
            );
        }
        let source = fs::read_to_string(path)?;
        let facts = lattice::ingest::parse_file(&relative, &source)?;
        let lines: Vec<_> = source.lines().collect();
        let mut insert = |name: &str, kind: &str, start: usize, text: &str| -> Result<()> {
            count += 1;
            known.insert(key(&relative, name, kind));
            let embedding = serde_json::to_string(&model.embed(text)?)?;
            tx.execute(
                "INSERT INTO documents VALUES (?1, ?2, ?3, ?4, ?5)",
                params![count, relative, name, kind, start],
            )?;
            tx.execute(
                "INSERT INTO vec_documents VALUES (?1, ?2)",
                params![count, embedding],
            )?;
            Ok(())
        };
        if facts.functions.is_empty() {
            insert("", "file", 1, &source)?;
        } else {
            for decl in facts.functions {
                let kind = if decl.parent.is_some() {
                    "method"
                } else {
                    "function"
                };
                let text = lines[decl.start_line - 1..decl.end_line].join("\n");
                insert(&decl.name, kind, decl.start_line, &text)?;
            }
        }
    }
    tx.commit()?;
    assert!(count > 0, "benchmark corpus has no source spans");
    Ok((db, known, count))
}

#[derive(Default)]
struct Metrics {
    n: usize,
    recall: f64,
    union_recall: f64,
    ndcg: f64,
    top1: usize,
}

impl Metrics {
    fn record(&mut self, hits: &[String], candidates: &[String], labels: &[&str]) {
        let recall = |items: &[String]| {
            labels
                .iter()
                .filter(|&&l| items.iter().any(|h| h == l))
                .count() as f64
                / labels.len() as f64
        };
        let discount = |i: usize| 1.0 / ((i + 2) as f64).log2();
        let dcg: f64 = hits
            .iter()
            .take(5)
            .enumerate()
            .filter(|(_, h)| labels.contains(&h.as_str()))
            .map(|(i, _)| discount(i))
            .sum();
        let ideal: f64 = (0..labels.len().min(5)).map(discount).sum();
        self.n += 1;
        self.recall += recall(hits);
        self.union_recall += recall(candidates);
        self.ndcg += dcg / ideal;
        self.top1 += usize::from(hits.first().is_some_and(|h| labels.contains(&h.as_str())));
    }
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    assert!(
        matches!(args.len(), 2 | 3),
        "usage: retrieval_eval CORPUS_ROOT BEKKO_MODEL_DIR [LATTICE_BINARY]"
    );
    let (root, model) = (Path::new(&args[0]), Path::new(&args[1]));
    let cli = args.get(2).map(Path::new);
    let store = root.join(".lattice");
    assert!(
        store.join("meta.db").is_file(),
        "provide the shared FTS5 index"
    );
    let model_load = Instant::now();
    let bekko = Bekko::load(model)?;
    let bekko_load = model_load.elapsed();
    let build = Instant::now();
    let (bekko_db, known, count) = index_bekko(root, &bekko)?;
    eprintln!(
        "{count} documents parsed from source; Bekko model load {:?}; Bekko scratch index {:?}",
        bekko_load,
        build.elapsed()
    );
    let mini = if cli.is_some() {
        let db = Connection::open_with_flags(
            store.join("vectors.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let count = db.query_row("SELECT COUNT(*) FROM documents", [], |r| {
            r.get::<_, usize>(0)
        })?;
        Some((db, CandleEmbedder::from_env()?, count))
    } else {
        None
    };

    let mut groups: BTreeMap<&str, [Metrics; 3]> = BTreeMap::new();
    let mut times: [Vec<Duration>; 3] = Default::default();
    println!("category\tquery\tranks FTS5\tranks MiniLM\tranks Bekko");
    for row in include_str!("../eval/retrieval-queries.tsv")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let cols: Vec<_> = row.split('\t').collect();
        let [category, text, relevant] = cols[..] else {
            panic!("bad row: {row}")
        };
        let labels: Vec<_> = relevant.split('|').collect();
        assert!(
            labels.iter().all(|l| known.contains(*l)),
            "label not in corpus: {relevant}"
        );
        let mut output: [(Rank, Rank); 3] = Default::default();
        for round in 0..=ROUNDS {
            for arm in (0..3).map(|i| (i + round) % 3) {
                if arm == 1 && mini.is_none() {
                    continue;
                }
                let begin = Instant::now();
                let fts = lexical(&store, text)?;
                output[arm] = match arm {
                    0 => (fts.clone(), fts),
                    1 => {
                        let (db, embedder, count) = mini.as_ref().unwrap();
                        hybrid(&store, fts, vector(db, &embedder.embed(text), *count)?)?
                    }
                    _ => hybrid(&store, fts, vector(&bekko_db, &bekko.embed(text)?, count)?)?,
                };
                if round > 0 {
                    times[arm].push(begin.elapsed());
                }
            }
        }
        if let Some(cli) = cli {
            let check = Command::new(cli)
                .current_dir(root)
                .args(["query", text, "--limit", "20"])
                .output()?;
            assert!(
                check.status.success(),
                "{}",
                String::from_utf8_lossy(&check.stderr)
            );
            let env: serde_json::Value = serde_json::from_slice(&check.stdout)?;
            let expected: Vec<_> = env["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| {
                    key(
                        h["path"].as_str().unwrap(),
                        h["name"].as_str().unwrap(),
                        h["kind"].as_str().unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                output[1].0, expected,
                "MiniLM hybrid diverges from CLI: {text}"
            );
        }
        print!("{category}\t{text}");
        for (arm, (rank, candidates)) in output.iter().enumerate() {
            if arm == 1 && mini.is_none() {
                print!("\tnot run");
                continue;
            }
            for group in [category, "all"] {
                groups.entry(group).or_default()[arm].record(rank, candidates, &labels);
            }
            let positions: Vec<_> = labels
                .iter()
                .map(|&l| {
                    rank.iter()
                        .position(|h| h == l)
                        .map(|i| (i + 1).to_string())
                        .unwrap_or("-".into())
                })
                .collect();
            print!("\t{}", positions.join(","));
        }
        println!();
    }
    println!(
        "\ngroup\tarm\tqueries\trecall@20\tcandidate union recall\tnDCG@5\ttop1\tp95 ms (all only)"
    );
    for (group, arms) in groups {
        for (arm, (m, name)) in arms
            .into_iter()
            .zip(["FTS5", "MiniLM+RRF", "Bekko+RRF"])
            .enumerate()
        {
            if m.n == 0 {
                continue;
            }
            let n = m.n as f64;
            let p95 = if group == "all" {
                times[arm].sort();
                format!(
                    "{:.3}",
                    times[arm][(times[arm].len() * 95).div_ceil(100) - 1].as_secs_f64() * 1000.0
                )
            } else {
                "-".into()
            };
            println!(
                "{group}\t{name}\t{}\t{:.4}\t{:.4}\t{:.4}\t{}\t{p95}",
                m.n,
                m.recall / n,
                m.union_recall / n,
                m.ndcg / n,
                m.top1
            );
        }
    }
    Ok(())
}
