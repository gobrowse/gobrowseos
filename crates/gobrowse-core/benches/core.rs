use std::collections::HashMap;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use gobrowse_core::{
    context::{ContextCandidate, ContextSource, build_context},
    library::{RankingWeights, chunk_text, rank_fusion},
};
use uuid::Uuid;

fn library_rank_fusion(criterion: &mut Criterion) {
    let lexical: Vec<_> = (0..200).map(|_| Uuid::new_v4()).collect();
    let mut semantic: Vec<_> = lexical.iter().copied().skip(100).collect();
    semantic.extend((0..100).map(|_| Uuid::new_v4()));
    let boosts: HashMap<_, _> = lexical
        .iter()
        .take(50)
        .copied()
        .map(|id| (id, (0.8, 1.0, 1.0)))
        .collect();
    criterion.bench_function("library_rank_fusion_200x200", |bencher| {
        bencher.iter(|| {
            rank_fusion(
                std::hint::black_box(&lexical),
                std::hint::black_box(&semantic),
                std::hint::black_box(&boosts),
                RankingWeights::default(),
            )
        });
    });
}

fn book_chunking(criterion: &mut Criterion) {
    let body = "Gobrowse context 🦀 provenance and trust. ".repeat(25_000);
    criterion.bench_function("chunk_book_1m_chars", |bencher| {
        bencher.iter(|| chunk_text(std::hint::black_box(&body), 4_000, 400));
    });
}

fn context_construction(criterion: &mut Criterion) {
    criterion.bench_function("context_select_500_candidates", |bencher| {
        bencher.iter_batched(
            || {
                (0..500)
                    .map(|index| ContextCandidate {
                        source: ContextSource::LibraryRetrieval,
                        stable_id: format!("L-{index:05}"),
                        content: "bounded context".repeat(20),
                        token_estimate: 100,
                        priority: u16::try_from(index % 100).unwrap(),
                        required: index < 3,
                        trust_label: "user_provided".into(),
                    })
                    .collect()
            },
            |candidates| build_context(candidates, 20_000),
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    library_rank_fusion,
    book_chunking,
    context_construction
);
criterion_main!(benches);
