use crate::api::types::{CacheControl, ContentBlock, Message, MessageContent};

/// Add cache_control breakpoints to the last 2 eligible assistant turns.
/// Eligible = assistant role with Blocks content. Only the *last* text block of
/// each turn gets the tag: the API allows at most 4 breakpoints per request
/// (the system prompt uses one), so tagging every text block of a turn that
/// interleaves text and tool calls would exceed it and fail with a 400.
pub fn attach_cache_breakpoints(mut messages: Vec<Message>) -> Vec<Message> {
    let eligible_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.role == "assistant" && matches!(m.content, MessageContent::Blocks(_))
        })
        .map(|(i, _)| i)
        .collect();

    let last_two: Vec<usize> = eligible_idxs
        .iter()
        .rev()
        .take(2)
        .copied()
        .collect();

    for idx in last_two {
        if let MessageContent::Blocks(blocks) = &mut messages[idx].content {
            if let Some(ContentBlock::Text { cache_control, .. }) = blocks
                .iter_mut()
                .rev()
                .find(|b| matches!(b, ContentBlock::Text { .. }))
            {
                *cache_control = Some(CacheControl::ephemeral());
            }
        }
    }
    messages
}
