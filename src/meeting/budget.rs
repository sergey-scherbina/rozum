/// Simple budget tracker: counts chars, warns at per-turn threshold, hard-stops at total.
#[derive(Debug, Clone)]
pub struct BudgetGuard {
    pub max_tokens_per_turn: usize,
    pub max_total_chars: usize,
    total_chars: usize,
}

impl Default for BudgetGuard {
    fn default() -> Self {
        // Unlimited by default. The CLI flags `--budget` and
        // `--per-turn-budget` opt into a hard total cap or a per-turn
        // warning threshold respectively.
        Self {
            max_tokens_per_turn: usize::MAX,
            max_total_chars: usize::MAX,
            total_chars: 0,
        }
    }
}

impl BudgetGuard {
    pub fn new(max_tokens_per_turn: usize, max_total_chars: usize) -> Self {
        Self {
            max_tokens_per_turn,
            max_total_chars,
            total_chars: 0,
        }
    }

    /// Returns (per_turn_warning, total_exceeded).
    pub fn record_turn(&mut self, content: &str) -> (bool, bool) {
        let chars = content.len();
        let per_turn_warn = chars > self.max_tokens_per_turn.saturating_mul(4);
        self.total_chars = self.total_chars.saturating_add(chars);
        let total_exceeded = self.total_chars >= self.max_total_chars;
        (per_turn_warn, total_exceeded)
    }

    pub fn total_chars(&self) -> usize {
        self.total_chars
    }

    pub fn update_max_total(&mut self, max: usize) {
        self.max_total_chars = max;
    }
}
