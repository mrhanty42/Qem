use super::*;

const DEFAULT_COMPACTION_MIN_TOTAL_BYTES: usize = 1024 * 1024;
const DEFAULT_COMPACTION_MIN_PIECES: usize = 1024;
const DEFAULT_COMPACTION_SMALL_PIECE_BYTES: usize = 1024;
const DEFAULT_COMPACTION_MAX_AVERAGE_PIECE_BYTES: usize = 4096;
const DEFAULT_COMPACTION_MIN_RATIO: f64 = 0.35;
const DEFAULT_COMPACTION_FORCED_PIECES: usize = 8192;
const DEFAULT_COMPACTION_FORCED_RATIO: f64 = 0.50;

/// Policy thresholds used to decide when a piece-table document should be compacted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionPolicy {
    /// Minimum document size before compaction is considered worthwhile.
    pub min_total_bytes: usize,
    /// Minimum piece count before broader compaction is considered.
    pub min_piece_count: usize,
    /// Pieces at or below this size contribute to fragmentation ratio.
    pub small_piece_threshold_bytes: usize,
    /// Maximum average piece size allowed for deferred compaction recommendations.
    pub max_average_piece_bytes: usize,
    /// Minimum ratio of small pieces required before deferred compaction is recommended.
    pub min_fragmentation_ratio: f64,
    /// Hard piece-count threshold for forced compaction recommendations.
    pub forced_piece_count: usize,
    /// Hard fragmentation ratio threshold for forced compaction recommendations.
    pub forced_fragmentation_ratio: f64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            min_total_bytes: DEFAULT_COMPACTION_MIN_TOTAL_BYTES,
            min_piece_count: DEFAULT_COMPACTION_MIN_PIECES,
            small_piece_threshold_bytes: DEFAULT_COMPACTION_SMALL_PIECE_BYTES,
            max_average_piece_bytes: DEFAULT_COMPACTION_MAX_AVERAGE_PIECE_BYTES,
            min_fragmentation_ratio: DEFAULT_COMPACTION_MIN_RATIO,
            forced_piece_count: DEFAULT_COMPACTION_FORCED_PIECES,
            forced_fragmentation_ratio: DEFAULT_COMPACTION_FORCED_RATIO,
        }
    }
}

/// How urgently the engine recommends running a broader compaction pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionUrgency {
    /// Compaction is worth scheduling in idle/background time.
    Deferred,
    /// Compaction should happen before persistence-sensitive boundaries like save.
    Forced,
}

/// A policy decision derived from current piece-table fragmentation metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionRecommendation {
    urgency: CompactionUrgency,
    stats: FragmentationStats,
}

impl CompactionRecommendation {
    /// Returns the urgency class for the recommendation.
    pub fn urgency(self) -> CompactionUrgency {
        self.urgency
    }

    /// Returns the fragmentation metrics that triggered the recommendation.
    pub fn stats(self) -> FragmentationStats {
        self.stats
    }
}

/// Outcome of an idle compaction pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IdleCompactionOutcome {
    /// No compaction work was needed for the current document state.
    NotNeeded,
    /// Deferred compaction ran and rewrote the active piece-table snapshot.
    Compacted(CompactionRecommendation),
    /// A hard threshold is pending and should be handled at an explicit save or
    /// operator-controlled maintenance boundary.
    ForcedPending(CompactionRecommendation),
}

impl IdleCompactionOutcome {
    /// Returns the recommendation associated with this outcome, if any.
    pub const fn recommendation(self) -> Option<CompactionRecommendation> {
        match self {
            Self::NotNeeded => None,
            Self::Compacted(recommendation) | Self::ForcedPending(recommendation) => {
                Some(recommendation)
            }
        }
    }

    /// Returns `true` when this idle pass actually compacted the document.
    pub const fn is_compacted(self) -> bool {
        matches!(self, Self::Compacted(_))
    }

    /// Returns `true` when a forced compaction threshold is pending.
    pub const fn is_forced_pending(self) -> bool {
        matches!(self, Self::ForcedPending(_))
    }
}

impl Document {
    /// Returns a compaction recommendation for piece-table backed documents.
    ///
    /// Local adjacent coalescing already runs on the hot edit path. This API is
    /// for broader deferred/forced compaction decisions only.
    pub fn compaction_recommendation(&self) -> Option<CompactionRecommendation> {
        self.compaction_recommendation_with_policy(CompactionPolicy::default())
    }

    /// Returns a compaction recommendation using a caller-provided policy.
    pub fn compaction_recommendation_with_policy(
        &self,
        policy: CompactionPolicy,
    ) -> Option<CompactionRecommendation> {
        self.piece_table
            .as_ref()
            .and_then(|piece_table| piece_table.compaction_recommendation(policy))
    }

    /// Compacts the current piece-table backed document state into a dense snapshot.
    ///
    /// This does not create a new undo step. Older undo history remains intact,
    /// while the current history entry is rewritten into a compact representation.
    /// Returns `Ok(false)` when the document is not piece-table backed, is
    /// already compact enough that no rewrite is needed, or exceeds the
    /// full-materialization size budget (compaction copies the whole
    /// document into the edit buffer, so it is capped like the rope path).
    pub fn compact_piece_table(&mut self) -> io::Result<bool> {
        let Some(piece_table) = self.piece_table.as_mut() else {
            return Ok(false);
        };
        piece_table.compact_current_state()
    }

    /// Compacts the current piece-table state when the provided policy recommends it.
    ///
    /// Returns the triggering recommendation when compaction ran.
    pub fn compact_piece_table_if_recommended(
        &mut self,
        policy: CompactionPolicy,
    ) -> io::Result<Option<CompactionRecommendation>> {
        let Some(recommendation) = self.compaction_recommendation_with_policy(policy) else {
            return Ok(None);
        };
        if self.compact_piece_table()? {
            Ok(Some(recommendation))
        } else {
            Ok(None)
        }
    }

    /// Runs an idle-time compaction pass with the default policy.
    ///
    /// This performs only `Deferred` maintenance work. `Forced`
    /// recommendations are surfaced as [`IdleCompactionOutcome::ForcedPending`]
    /// so callers can keep heavier compaction tied to explicit maintenance or
    /// save boundaries.
    pub fn run_idle_compaction(&mut self) -> io::Result<IdleCompactionOutcome> {
        self.run_idle_compaction_with_policy(CompactionPolicy::default())
    }

    /// Runs an idle-time compaction pass using a caller-provided policy.
    pub fn run_idle_compaction_with_policy(
        &mut self,
        policy: CompactionPolicy,
    ) -> io::Result<IdleCompactionOutcome> {
        let Some(recommendation) = self.compaction_recommendation_with_policy(policy) else {
            return Ok(IdleCompactionOutcome::NotNeeded);
        };
        if recommendation.urgency() == CompactionUrgency::Forced {
            return Ok(IdleCompactionOutcome::ForcedPending(recommendation));
        }
        if self.compact_piece_table()? {
            Ok(IdleCompactionOutcome::Compacted(recommendation))
        } else {
            Ok(IdleCompactionOutcome::NotNeeded)
        }
    }
}

impl PieceTable {
    pub(super) fn compact_current_state(&mut self) -> io::Result<bool> {
        let stats = self.fragmentation_stats();
        if stats.piece_count <= 1 && self.full_index {
            return Ok(false);
        }

        // Compaction materializes the whole document into the add buffer
        // (this is where `full_index` becomes true), so it carries the same
        // size budget as every other full-materialization path. Refusing
        // above the cap keeps `run_idle_compaction` - which the crate docs
        // advertise as an idle-time helper - from aborting the process on
        // a partial-window huge document.
        if self.total_len > super::MAX_ROPE_EDIT_FILE_BYTES {
            return Ok(false);
        }

        let mut compacted = self.read_range(0, self.total_len);
        let compacted_len = compacted.len();
        let line_breaks = count_line_breaks_in_bytes(&compacted);
        let add_start = self.add.len();
        self.add.append(&mut compacted);

        let replacement = if compacted_len == 0 {
            Vec::new()
        } else {
            vec![Piece {
                src: PieceSource::Add,
                start: add_start,
                len: compacted_len,
                line_breaks,
            }]
        };
        self.pieces.replace_current_root_with_pieces(replacement);
        self.total_len = compacted_len;
        self.known_byte_len = compacted_len;
        self.full_index = true;
        self.pieces
            .set_known_boundary(self.known_byte_len, self.full_index);
        self.refresh_known_line_count();
        self.schedule_session_flush()?;
        Ok(true)
    }

    pub(super) fn compaction_recommendation(
        &self,
        policy: CompactionPolicy,
    ) -> Option<CompactionRecommendation> {
        if self.total_len < policy.min_total_bytes {
            return None;
        }
        // Never recommend a rewrite the executor would refuse: compaction
        // materializes the whole document and is capped like every other
        // full-materialization path.
        if self.total_len > super::MAX_ROPE_EDIT_FILE_BYTES {
            return None;
        }

        let stats = self.fragmentation_stats_with_threshold(policy.small_piece_threshold_bytes);
        if stats.piece_count < policy.min_piece_count {
            return None;
        }

        let ratio = stats.fragmentation_ratio();
        if stats.piece_count >= policy.forced_piece_count
            && ratio >= policy.forced_fragmentation_ratio
        {
            return Some(CompactionRecommendation {
                urgency: CompactionUrgency::Forced,
                stats,
            });
        }

        if ratio >= policy.min_fragmentation_ratio
            && stats.average_piece_bytes() <= policy.max_average_piece_bytes as f64
        {
            return Some(CompactionRecommendation {
                urgency: CompactionUrgency::Deferred,
                stats,
            });
        }

        None
    }
}
