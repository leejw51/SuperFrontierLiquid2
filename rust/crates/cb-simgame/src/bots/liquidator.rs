//! The liquidator: watches for underwater accounts and triggers liquidations.
//!
//! `LiquidationTrigger` is permissionless, so this bot is exactly what a
//! keeper on a real deployment would be: it sweeps every account the chain
//! would consider below maintenance margin and submits a trigger, collecting
//! the keeper share of the liquidation fee when the close succeeds.
//!
//! The bot is deliberately eager — it re-triggers an account that is still
//! liquidatable rather than assuming a previous trigger worked. That matters
//! for invariant 6: if a trigger can be accepted without actually closing the
//! position, an eager keeper makes the failure visible as "queued repeatedly,
//! never cleared" instead of hiding it behind a single unlucky attempt.

use cb_tx::TxBody;
use rand_chacha::ChaCha8Rng;

use super::{Bot, BotKind, Observation};

/// Parameters for one liquidator.
#[derive(Clone, Copy, Debug)]
pub struct LiquidatorParams {
    /// Maximum triggers submitted per block, so one keeper cannot fill a
    /// block by itself during a cascade.
    pub max_triggers_per_block: usize,
}

impl Default for LiquidatorParams {
    fn default() -> Self {
        Self {
            max_triggers_per_block: 12,
        }
    }
}

/// A keeper bot.
pub struct Liquidator {
    label: String,
    params: LiquidatorParams,
    /// Rotates the starting point of the sweep so several keepers do not all
    /// contend for the same victim every block.
    offset: usize,
    /// Count of triggers submitted, for the run report.
    pub triggers_submitted: u64,
}

impl Liquidator {
    /// Build a liquidator.
    pub fn new(label: String, offset: usize, params: LiquidatorParams) -> Self {
        Self {
            label,
            params,
            offset,
            triggers_submitted: 0,
        }
    }
}

impl Bot for Liquidator {
    fn label(&self) -> &str {
        &self.label
    }

    fn kind(&self) -> BotKind {
        BotKind::Liquidator
    }

    fn act(&mut self, obs: &Observation, _rng: &mut ChaCha8Rng) -> Vec<TxBody> {
        let targets = &obs.liquidatable;
        if targets.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let n = targets.len();
        let start = self.offset % n;
        for i in 0..n.min(self.params.max_triggers_per_block) {
            let (owner, market) = targets[(start + i) % n];
            out.push(TxBody::LiquidationTrigger { owner, market });
            self.triggers_submitted += 1;
        }
        self.offset = self.offset.wrapping_add(1);
        out
    }
}
