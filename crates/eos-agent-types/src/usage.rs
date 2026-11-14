use serde::{Deserialize, Serialize};

/// Token usage as the API reports it. A `message_delta` carries a partial
/// usage whose present fields override the `message_start` values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUse>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerToolUse {
    #[serde(default)]
    pub web_search_requests: u64,
    #[serde(default)]
    pub web_fetch_requests: u64,
}

impl Usage {
    /// Applies a later partial report: every field it carries wins.
    pub fn merge(&mut self, later: &Usage) {
        fn take<T: Clone>(slot: &mut Option<T>, later: &Option<T>) {
            if later.is_some() {
                slot.clone_from(later);
            }
        }
        take(&mut self.input_tokens, &later.input_tokens);
        take(&mut self.output_tokens, &later.output_tokens);
        take(&mut self.cache_creation_input_tokens, &later.cache_creation_input_tokens);
        take(&mut self.cache_read_input_tokens, &later.cache_read_input_tokens);
        take(&mut self.cache_creation, &later.cache_creation);
        take(&mut self.server_tool_use, &later.server_tool_use);
    }

    /// Tokens the request occupied in the context window.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_fields_the_later_report_lacks() {
        let mut u = Usage { input_tokens: Some(10), cache_read_input_tokens: Some(90), ..Default::default() };
        u.merge(&Usage { output_tokens: Some(5), ..Default::default() });
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.output_tokens, Some(5));
        assert_eq!(u.context_tokens(), 100);
    }
}
