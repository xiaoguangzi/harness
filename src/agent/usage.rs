//! Token counts added up over a conversation, for `/usage`.

use std::sync::Mutex;

use crate::llm::Usage;

/// The tokens a conversation and the subagents it started used, per
/// model. Shared between them, and kept across `/clear` and `/resume`.
#[derive(Default)]
pub struct UsageLog {
    models: Mutex<Vec<(String, Totals)>>,
}

/// What the requests to one model used.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Totals {
    /// Requests counted, including attempts that broke off partway.
    pub requests: u64,
    pub usage: Usage,
}

impl UsageLog {
    /// Adds one request to `model`'s totals.
    pub fn record(&self, model: &str, usage: &Usage) {
        let mut models = self.models.lock().unwrap();
        let index = match models.iter().position(|(m, _)| m == model) {
            Some(i) => i,
            None => {
                models.push((model.to_string(), Totals::default()));
                models.len() - 1
            }
        };
        let totals = &mut models[index].1;
        totals.requests += 1;
        totals.usage.input_tokens += usage.input_tokens;
        totals.usage.cache_read_tokens += usage.cache_read_tokens;
        totals.usage.cache_write_tokens += usage.cache_write_tokens;
        totals.usage.output_tokens += usage.output_tokens;
    }

    /// Each model's totals, in the order they were first used.
    pub fn models(&self) -> Vec<(String, Totals)> {
        self.models.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_up_per_model_in_order_of_first_use() {
        let log = UsageLog::default();
        let usage = |input, cached, output| Usage {
            input_tokens: input,
            cache_read_tokens: cached,
            cache_write_tokens: 0,
            output_tokens: output,
        };
        log.record("gw/b", &usage(100, 80, 10));
        log.record("gw/a", &usage(50, 0, 5));
        log.record("gw/b", &usage(200, 150, 20));
        assert_eq!(
            log.models(),
            vec![
                ("gw/b".to_string(), Totals { requests: 2, usage: usage(300, 230, 30) }),
                ("gw/a".to_string(), Totals { requests: 1, usage: usage(50, 0, 5) }),
            ]
        );
    }
}
