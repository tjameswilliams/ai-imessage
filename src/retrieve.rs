//! Hybrid retrieval: fuse FTS5 keyword ranking and vector similarity
//! ranking with reciprocal rank fusion (RRF).
//!
//! RRF only looks at ranks, never raw scores, so bm25 and cosine — which
//! live on incomparable scales — need no calibration: each list
//! contributes `1 / (K + rank)` and chunks found by both lists rise.

use std::collections::HashMap;

use crate::index::{IndexDb, IndexError, SearchHit};

/// Standard RRF dampening constant from the original paper; large enough
/// that a single #1 rank cannot drown out agreement between lists.
const RRF_K: f32 = 60.0;

#[derive(Debug, Clone, Copy)]
pub struct RetrievalParams {
    pub fts_candidates: u32,
    pub vector_candidates: u32,
    pub limit: u32,
}

/// Keyword + vector search fused by RRF. `query_vec` is `None` when the
/// index has no embeddings (e.g. `etl --no-embed`); retrieval then
/// degrades to pure keyword ranking rather than failing.
pub fn hybrid_search(
    index: &IndexDb,
    query: &str,
    query_vec: Option<&[f32]>,
    params: &RetrievalParams,
) -> Result<Vec<SearchHit>, IndexError> {
    let keyword = index.search(query, params.fts_candidates)?;
    let vector = match query_vec {
        Some(v) => index.vector_search(v, params.vector_candidates)?,
        None => Vec::new(),
    };
    Ok(fuse(keyword, vector, params.limit as usize))
}

/// The keyword list wins ties on presentation: its snippets carry
/// «highlight» markers, which beat a truncated chunk head.
fn fuse(keyword: Vec<SearchHit>, vector: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    let mut scores: HashMap<i64, f32> = HashMap::new();
    let mut hits: HashMap<i64, SearchHit> = HashMap::new();

    for list in [keyword, vector] {
        for (rank, hit) in list.into_iter().enumerate() {
            *scores.entry(hit.chunk_id).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
            hits.entry(hit.chunk_id).or_insert(hit);
        }
    }

    let mut fused: Vec<(f32, SearchHit)> = hits
        .into_values()
        .map(|mut h| {
            let score = scores[&h.chunk_id];
            // Cosine similarity from one branch is meaningless on a fused
            // list; the ordering itself is the result.
            h.score = None;
            (score, h)
        })
        .collect();
    fused.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.chunk_id.cmp(&b.1.chunk_id)));
    fused.truncate(limit);
    fused.into_iter().map(|(_, h)| h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(chunk_id: i64, snippet: &str) -> SearchHit {
        SearchHit {
            chunk_id,
            chat_label: "chat".into(),
            started_at_ms: None,
            ended_at_ms: None,
            message_count: 1,
            snippet: snippet.into(),
            score: None,
        }
    }

    #[test]
    fn agreement_between_lists_outranks_a_single_first_place() {
        // Chunk 1 is #2 in both lists; chunks 2 and 3 are #1 in one list
        // each and absent from the other.
        let keyword = vec![hit(2, "kw-top"), hit(1, "both")];
        let vector = vec![hit(3, "vec-top"), hit(1, "both")];
        let fused = fuse(keyword, vector, 10);
        assert_eq!(fused[0].chunk_id, 1);
    }

    #[test]
    fn keyword_snippet_is_kept_for_chunks_found_by_both() {
        let keyword = vec![hit(1, "with «highlights»")];
        let vector = vec![hit(1, "plain truncation")];
        let fused = fuse(keyword, vector, 10);
        assert_eq!(fused[0].snippet, "with «highlights»");
    }

    #[test]
    fn empty_vector_list_degrades_to_keyword_order() {
        let keyword = vec![hit(5, "a"), hit(6, "b"), hit(7, "c")];
        let fused = fuse(keyword, Vec::new(), 10);
        let ids: Vec<i64> = fused.iter().map(|h| h.chunk_id).collect();
        assert_eq!(ids, vec![5, 6, 7]);
    }

    #[test]
    fn limit_truncates_the_fused_list() {
        let keyword = (0..20).map(|i| hit(i, "k")).collect();
        let fused = fuse(keyword, Vec::new(), 3);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn fused_hits_carry_no_similarity_score() {
        let mut v = hit(1, "v");
        v.score = Some(0.9);
        let fused = fuse(Vec::new(), vec![v], 10);
        assert_eq!(fused[0].score, None);
    }

    #[test]
    fn ties_break_deterministically_by_chunk_id() {
        let keyword = vec![hit(9, "a"), hit(4, "b")];
        let vector = vec![hit(4, "b"), hit(9, "a")];
        // Both chunks have identical fused scores (ranks 1+2 vs 2+1).
        let fused = fuse(keyword, vector, 10);
        assert_eq!(fused[0].chunk_id, 4);
        assert_eq!(fused[1].chunk_id, 9);
    }
}
