use crate::api::types::{CacheControl, ContentBlock, Message, MessageContent};

/// Add cache_control breakpoints to the last 2 eligible assistant turns.
/// Eligible = assistant role with Blocks content. Only text blocks get the tag.
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
            for block in blocks.iter_mut() {
                if let ContentBlock::Text { cache_control, .. } = block {
                    *cache_control = Some(CacheControl::ephemeral());
                }
            }
        }
    }
    messages
}
