//! What the Snippets panel pays per repaint to find and copy out the snippets
//! it shows: short commands by list size, and a thousand long scripts
//! searched for text they lack, including text that shares a long prefix
//! with them.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use thinkterm_snippets::{SnippetRecord, SnippetStore};

fn store_of(count: usize, body: impl Fn(usize) -> String) -> SnippetStore {
    let mut store = SnippetStore::default();
    for index in 0..count {
        store.create(
            format!("snippet-{index}"),
            &format!("Deploy step {index}"),
            body(index),
            index as u64,
        );
    }
    store
}

fn command(index: usize) -> String {
    format!("ssh server-a 'cd /srv/app && git pull --rebase && make build-{index}'")
}

/// About 8 KiB of shell script.
fn script(index: usize) -> String {
    (0..128)
        .map(|line| format!("echo \"step {index}.{line}\" && make target-{line}\n"))
        .collect()
}

fn found(store: &SnippetStore, query: &str) -> Vec<SnippetRecord> {
    store.matching(black_box(query)).cloned().collect()
}

fn matching(c: &mut Criterion) {
    let mut group = c.benchmark_group("matching");
    for count in [100, 1_000, 10_000] {
        let store = store_of(count, command);
        for (label, query) in [("blank", ""), ("typed", "Make Build-9")] {
            group.bench_with_input(BenchmarkId::new(label, count), &store, |b, store| {
                b.iter(|| found(store, query))
            });
        }
    }

    let scripts = store_of(1_000, script);
    group.bench_function("long/miss", |b| b.iter(|| found(&scripts, "kubectl rollout")));
    let prefixes = store_of(1_000, |_| "a".repeat(8_192));
    let near_miss = format!("{}b", "a".repeat(63));
    group.bench_function("long/shared-prefix", |b| {
        b.iter(|| found(&prefixes, &near_miss))
    });
    group.finish();
}

criterion_group!(benches, matching);
criterion_main!(benches);
