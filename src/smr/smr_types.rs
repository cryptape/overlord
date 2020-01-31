use derive_more::Display;
use serde::{Deserialize, Serialize};

use crate::types::Hash;
use crate::wal::SMRBase;

/// SMR steps. The default step is commit step because SMR needs rich status to start a new epoch.
#[derive(Serialize, Deserialize, Clone, Debug, Display, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Prepose step, in this step:
    /// Firstly, each node calculate the new proposer, then:
    /// Leader:
    ///     proposer an epoch,
    /// Replica:
    ///     wait for a proposal and check it.
    /// Then goto prevote step.
    #[display(fmt = "Prepose step")]
    Propose,
    /// Prevote step, in this step:
    /// Leader:
    ///     1. wait for others signed prevote votes,
    ///     2. aggregate them to an aggregated vote,
    ///     3. broadcast the aggregated vote to others.
    /// Replica:
    ///     1. transmit prevote vote,
    ///     2. wait for aggregated vote,
    ///     3. check the aggregated vote.
    /// Then goto precommit step.
    #[display(fmt = "Prevote step")]
    Prevote,
    /// Precommit step, in this step:
    /// Leader:
    ///     1. wait for others signed precommit votes,
    ///     2. aggregate them to an aggregated vote,
    ///     3. broadcast the aggregated vote to others.
    /// Replica:
    ///     1. transmit precommit vote,
    ///     2. wait for aggregated vote,
    ///     3. check the aggregated vote.
    /// If there is no consensus in the precommit step, goto propose step and start a new round
    /// cycle. Otherwise, goto commit step.
    #[display(fmt = "Precommit step")]
    Precommit,
    /// Commit step, in this step each node commit the epoch and wait for the rich status. After
    /// receiving the it, all nodes will goto propose step and start a new epoch consensus.
    #[display(fmt = "Commit step")]
    Commit,
}

impl Default for Step {
    fn default() -> Self {
        Step::Commit
    }
}

impl Into<u8> for Step {
    fn into(self) -> u8 {
        match self {
            Step::Propose => 0,
            Step::Prevote => 1,
            Step::Precommit => 2,
            Step::Commit => 3,
        }
    }
}

impl From<u8> for Step {
    fn from(s: u8) -> Self {
        match s {
            0 => Step::Propose,
            1 => Step::Prevote,
            2 => Step::Precommit,
            3 => Step::Commit,
            _ => panic!("Invalid vote type!"),
        }
    }
}

/// SMR event that state and timer monitor this.
/// **NOTICE**: The `height` field is just for the timer. Timer will take this to signal the timer
/// epoch ID. State will ignore this field on handling event.
#[derive(Clone, Debug, Display, PartialEq, Eq)]
pub enum SMREvent {
    /// New round event,
    /// for state: update round,
    /// for timer: set a propose step timer. If `round == 0`, set an extra total epoch timer.
    #[display(fmt = "New round {} event", round)]
    NewRoundInfo {
        height:        u64,
        round:         u64,
        lock_round:    Option<u64>,
        lock_proposal: Option<Hash>,
    },
    /// Prevote event,
    /// for state: transmit a prevote vote,
    /// for timer: set a prevote step timer.
    #[display(fmt = "Prevote event")]
    PrevoteVote {
        height:     u64,
        round:      u64,
        block_hash: Hash,
        lock_round: Option<u64>,
    },

    /// Precommit event,
    /// for state: transmit a precommit vote,
    /// for timer: set a precommit step timer.
    #[display(fmt = "Precommit event")]
    PrecommitVote {
        height:     u64,
        round:      u64,
        block_hash: Hash,
        lock_round: Option<u64>,
    },
    /// Commit event,
    /// for state: do commit,
    /// for timer: do nothing.
    #[display(fmt = "Commit event")]
    Commit(Hash),
    /// Stop event,
    /// for state: stop process,
    /// for timer: stop process.
    #[display(fmt = "Stop event")]
    Stop,
}

/// SMR trigger types.
#[derive(Clone, Debug, Display, PartialEq, Eq)]
pub enum TriggerType {
    /// Proposal trigger.
    #[display(fmt = "Proposal")]
    Proposal,
    /// Prevote quorum certificate trigger.
    #[display(fmt = "PrevoteQC")]
    PrevoteQC,
    /// Precommit quorum certificate trigger.
    #[display(fmt = "PrecommitQC")]
    PrecommitQC,
    /// New Epoch trigger.
    #[display(fmt = "New epoch {}", _0)]
    NewEpoch(u64),
    ///
    WalInfo,
}

/// SMR trigger sources.
#[derive(Serialize, Deserialize, Clone, Debug, Display, PartialEq, Eq)]
pub enum TriggerSource {
    /// SMR triggered by state.
    #[display(fmt = "State")]
    State = 0,
    /// SMR triggered by timer.
    #[display(fmt = "Timer")]
    Timer = 1,
}

impl Into<u8> for TriggerType {
    /// It should not occur that call `TriggerType::NewEpoch(*).into()`.
    fn into(self) -> u8 {
        match self {
            TriggerType::Proposal => 0u8,
            TriggerType::PrevoteQC => 1u8,
            TriggerType::PrecommitQC => 2u8,
            _ => unreachable!(),
        }
    }
}

impl From<u8> for TriggerType {
    /// It should not occur that call `from(3u8)`.
    fn from(s: u8) -> Self {
        match s {
            0 => TriggerType::Proposal,
            1 => TriggerType::PrevoteQC,
            2 => TriggerType::PrecommitQC,
            3 => unreachable!(),
            _ => panic!("Invalid trigger type!"),
        }
    }
}

/// A SMR trigger to touch off SMR process. For different trigger type,
/// the field `hash` and `round` have different restrictions and meaning.
/// While trigger type is `Proposal`:
///     * `hash`: Proposal epoch hash,
///     * `round`: Optional lock round.
/// While trigger type is `PrevoteQC` or `PrecommitQC`:
///     * `hash`: QC epoch hash,
///     * `round`: QC round, this must be `Some`.
/// While trigger type is `NewEpoch`:
///     * `hash`: A empty hash,
///     * `round`: This must be `None`.
/// For each sources, while filling the `SMRTrigger`, the `height` field take the current epoch ID
/// directly.
#[derive(Clone, Debug, Display, PartialEq, Eq)]
#[display(
    fmt = "{:?} trigger from {:?}, epoch ID {}",
    trigger_type,
    source,
    height
)]
pub struct SMRTrigger {
    /// SMR trigger type.
    pub trigger_type: TriggerType,
    /// SMR trigger source.
    pub source: TriggerSource,
    /// SMR trigger hash, the meaning shown above.
    pub hash: Hash,
    /// SMR trigger round, the meaning shown above.
    pub round: Option<u64>,
    /// **NOTICE**: This field is only for timer to signed timer's epoch ID. Therefore, the SMR can
    /// filter out the outdated timers.
    pub height: u64,
    ///
    pub wal_info: Option<SMRBase>,
}

/// An inner lock struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    /// Lock round.
    pub round: u64,
    /// Lock hash.
    pub hash: Hash,
}
