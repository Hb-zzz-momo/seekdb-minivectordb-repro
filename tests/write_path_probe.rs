//! Where does the write phase spend its time? Runs the benchmark's write
//! workload (10,000 rows of 128-d vectors, 100 per INSERT) in-process and
//! times each stage of the statement pipeline separately, best of five.
//!
//!   cargo test --release --test write_path_probe -- --ignored --nocapture

use std::path::PathBuf;
use std::time::Instant;

use minivectordb::engine::{Engine, EngineOptions, IndexOptions};
use minivectordb::protocol::Splitter;
use minivectordb::sql::ast::{Expr, Literal, Statement};
use minivectordb::sql::{self, parser::is_blank};
use minivectordb::storage::log::LogOptions;
use minivectordb::types::parse_vector;

/// Input lines in the bench's format (`f"{x:.6f}"` of uniform [-1, 1)).
fn workload() -> Vec<String> {
    let mut state = 1u64;
    let mut unit = || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    };
    let mut lines = vec!["CREATE TABLE items (id BIGINT PRIMARY KEY, category VARCHAR(32), embedding VECTOR(128));\n".to_string()];
    for b in 0..100 {
        let rows: Vec<String> = (0..100)
            .map(|r| {
                let id = b * 100 + r + 1;
                let v: Vec<String> = (0..128).map(|_| format!("{:.6}", unit())).collect();
                format!("({id}, 'c{}', '[{}]')", id % 10, v.join(","))
            })
            .collect();
        lines.push(format!("INSERT INTO items (id, category, embedding) VALUES {};\n", rows.join(", ")));
    }
    lines
}

fn best_ms(mut f: impl FnMut()) -> f64 {
    (0..5)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .fold(f64::INFINITY, f64::min)
}

fn split(lines: &[String]) -> Vec<String> {
    let mut sp = Splitter::default();
    let mut out = Vec::new();
    for l in lines {
        sp.push(l, &mut out);
    }
    out
}

fn fresh_dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn execute_all(stmts: &[Statement], sync: bool) {
    let opts = EngineOptions { log: LogOptions { sync, ..LogOptions::default() }, cache_budget: 256 << 20, index: IndexOptions { min_rows: usize::MAX, lists: false, codes: false } };
    let mut engine = Engine::open(&fresh_dir("write_path_probe"), opts).unwrap();
    for s in stmts {
        engine.execute(s).unwrap();
    }
}

#[test]
#[ignore]
fn write_path_stages() {
    let lines = workload();
    let bytes: usize = lines.iter().map(|l| l.len()).sum();
    let stmts = split(&lines);
    let parsed: Vec<Statement> = stmts.iter().map(|s| sql::parse(s).unwrap()).collect();
    let vectors: Vec<&str> = parsed
        .iter()
        .filter_map(|s| match s {
            Statement::Insert(ins) => Some(ins),
            _ => None,
        })
        .flat_map(|ins| ins.rows.iter())
        .filter_map(|row| match row.last() {
            Some(Expr::Lit(Literal::Str(s))) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(vectors.len(), 10_000);

    println!("{} statements, {:.1} MB of SQL", stmts.len(), bytes as f64 / 1e6);
    let stages: [(&str, f64); 6] = [
        ("split into statements", best_ms(|| drop(split(&lines)))),
        ("is_blank check", best_ms(|| assert!(!stmts.iter().any(|s| is_blank(s))))),
        ("parse (tokenize + AST)", best_ms(|| drop(stmts.iter().map(|s| sql::parse(s).unwrap()).collect::<Vec<_>>()))),
        ("parse_vector on all literals", best_ms(|| drop(vectors.iter().map(|v| parse_vector(v).unwrap()).collect::<Vec<_>>()))),
        ("execute, no fdatasync", best_ms(|| execute_all(&parsed, false))),
        ("execute, fdatasync per commit", best_ms(|| execute_all(&parsed, true))),
    ];
    for (name, ms) in stages {
        println!("  {name:<32} {ms:7.1} ms");
    }
}
