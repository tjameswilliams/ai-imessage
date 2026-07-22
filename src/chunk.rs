//! Conversation chunking: turn a chat's message stream into retrieval-sized
//! windows.
//!
//! A chunk boundary happens at a conversational lull (`gap_minutes` without
//! a message) or when the chunk reaches `target_tokens`. Size splits carry
//! the last `overlap_messages` messages into the next chunk so context is
//! not cut mid-exchange; gap splits do not, since the lull itself is the
//! context break.

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy)]
pub struct ChunkParams {
    pub gap_minutes: u32,
    pub target_tokens: u32,
    pub overlap_messages: u32,
}

/// One message as chunking sees it: who said it, what, and when.
#[derive(Debug, Clone)]
pub struct ChunkInput {
    pub sender: String,
    pub text: String,
    pub sent_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// One `sender: text` line per message.
    pub text: String,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub message_count: u32,
    /// Stable identity of the chunk's exact content; downstream layers
    /// (embeddings) key on this to survive re-chunking untouched.
    pub content_hash: String,
}

/// Rough token estimate; ~4 characters per token is close enough for
/// sizing windows.
fn est_tokens(s: &str) -> u32 {
    (s.len() / 4 + 1) as u32
}

fn line(m: &ChunkInput) -> String {
    format!("{}: {}", m.sender, m.text)
}

fn build_chunk(scope: &str, msgs: &[&ChunkInput]) -> Chunk {
    let lines: Vec<String> = msgs.iter().map(|m| line(m)).collect();
    let times: Vec<i64> = msgs.iter().filter_map(|m| m.sent_at_ms).collect();

    let mut hasher = Sha256::new();
    hasher.update(scope.as_bytes());
    hasher.update([0u8]);
    for (l, m) in lines.iter().zip(msgs) {
        hasher.update(l.as_bytes());
        hasher.update([0u8]);
        hasher.update(m.sent_at_ms.unwrap_or(0).to_le_bytes());
    }
    let content_hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    Chunk {
        text: lines.join("\n"),
        started_at_ms: times.iter().min().copied(),
        ended_at_ms: times.iter().max().copied(),
        message_count: msgs.len() as u32,
        content_hash,
    }
}

/// Chunk one chat's messages, given in chronological order. `scope` is any
/// stable identifier of the chat, mixed into each chunk's content hash so
/// identical exchanges in different chats never collide.
pub fn chunk_messages(scope: &str, msgs: &[ChunkInput], p: &ChunkParams) -> Vec<Chunk> {
    let gap_ms = i64::from(p.gap_minutes) * 60_000;
    let mut chunks = Vec::new();
    let mut cur: Vec<&ChunkInput> = Vec::new();
    let mut cur_tokens: u32 = 0;

    for m in msgs {
        let gap_split = match (cur.last().and_then(|l| l.sent_at_ms), m.sent_at_ms) {
            (Some(prev), Some(now)) => now - prev > gap_ms,
            _ => false,
        };
        let size_split = !cur.is_empty() && cur_tokens >= p.target_tokens;

        if gap_split || size_split {
            chunks.push(build_chunk(scope, &cur));
            let carry = if size_split && !gap_split {
                let keep = (p.overlap_messages as usize).min(cur.len().saturating_sub(1));
                cur.split_off(cur.len() - keep)
            } else {
                Vec::new()
            };
            cur = carry;
            cur_tokens = cur.iter().map(|m| est_tokens(&line(m))).sum();
        }

        cur_tokens += est_tokens(&line(m));
        cur.push(m);
    }
    if !cur.is_empty() {
        chunks.push(build_chunk(scope, &cur));
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: ChunkParams = ChunkParams {
        gap_minutes: 45,
        target_tokens: 750,
        overlap_messages: 3,
    };

    fn msg(sender: &str, text: &str, minute: i64) -> ChunkInput {
        ChunkInput {
            sender: sender.into(),
            text: text.into(),
            sent_at_ms: Some(minute * 60_000),
        }
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(chunk_messages("chat", &[], &PARAMS).is_empty());
    }

    #[test]
    fn one_conversation_becomes_one_chunk() {
        let msgs = vec![
            msg("Me", "want lunch?", 0),
            msg("Alice", "sure, noon?", 1),
            msg("Me", "see you there", 2),
        ];
        let chunks = chunk_messages("chat", &msgs, &PARAMS);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].message_count, 3);
        assert_eq!(
            chunks[0].text,
            "Me: want lunch?\nAlice: sure, noon?\nMe: see you there"
        );
        assert_eq!(chunks[0].started_at_ms, Some(0));
        assert_eq!(chunks[0].ended_at_ms, Some(2 * 60_000));
    }

    #[test]
    fn a_long_lull_starts_a_new_chunk_without_overlap() {
        let msgs = vec![
            msg("Me", "good night", 0),
            msg("Alice", "night!", 1),
            // Next morning.
            msg("Alice", "coffee?", 60 * 9),
        ];
        let chunks = chunk_messages("chat", &msgs, &PARAMS);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].message_count, 2);
        assert_eq!(chunks[1].message_count, 1);
        assert_eq!(chunks[1].text, "Alice: coffee?");
    }

    #[test]
    fn gap_exactly_at_threshold_does_not_split() {
        let msgs = vec![msg("Me", "a", 0), msg("Me", "b", 45)];
        assert_eq!(chunk_messages("chat", &msgs, &PARAMS).len(), 1);
    }

    #[test]
    fn size_split_carries_overlap_messages() {
        // Each message is ~100 tokens, so the 8th message crosses 750.
        let big = "x".repeat(400);
        let msgs: Vec<ChunkInput> = (0..10).map(|i| msg("Me", &big, i)).collect();
        let chunks = chunk_messages("chat", &msgs, &PARAMS);
        assert!(chunks.len() >= 2);
        // The second chunk repeats the last 3 messages of the first.
        let first_lines: Vec<&str> = chunks[0].text.lines().collect();
        let second_lines: Vec<&str> = chunks[1].text.lines().collect();
        assert_eq!(
            &first_lines[first_lines.len() - 3..],
            &second_lines[..3],
            "overlap messages must repeat"
        );
        // Every original message appears at least once.
        let total: u32 = chunks.iter().map(|c| c.message_count).sum();
        assert!(total >= 10);
    }

    #[test]
    fn messages_without_timestamps_never_gap_split() {
        let mut msgs = vec![msg("Me", "a", 0)];
        msgs.push(ChunkInput {
            sender: "Me".into(),
            text: "b".into(),
            sent_at_ms: None,
        });
        msgs.push(msg("Me", "c", 600));
        assert_eq!(chunk_messages("chat", &msgs, &PARAMS).len(), 1);
    }

    #[test]
    fn hashes_are_stable_and_scoped_to_the_chat() {
        let msgs = vec![msg("Me", "hello", 0)];
        let a = chunk_messages("chat-a", &msgs, &PARAMS);
        let b = chunk_messages("chat-a", &msgs, &PARAMS);
        let c = chunk_messages("chat-b", &msgs, &PARAMS);
        assert_eq!(a[0].content_hash, b[0].content_hash);
        assert_ne!(a[0].content_hash, c[0].content_hash);
    }

    #[test]
    fn hash_changes_when_a_message_changes() {
        let a = chunk_messages("chat", &[msg("Me", "hello", 0)], &PARAMS);
        let b = chunk_messages("chat", &[msg("Me", "hello!", 0)], &PARAMS);
        assert_ne!(a[0].content_hash, b[0].content_hash);
    }

    #[test]
    fn appending_messages_keeps_earlier_chunk_hashes() {
        let mut msgs = vec![msg("Me", "good night", 0), msg("Alice", "night!", 1)];
        let before = chunk_messages("chat", &msgs, &PARAMS);
        msgs.push(msg("Alice", "coffee?", 60 * 9));
        let after = chunk_messages("chat", &msgs, &PARAMS);
        // The lull isolates the old chunk, so its hash (and any embedding
        // keyed on it) survives.
        assert_eq!(before[0].content_hash, after[0].content_hash);
    }
}
