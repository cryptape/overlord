use derive_more::Display;

use crate::smr::smr_types::{Lock, Step};
use crate::types::{AggregatedVote, PoLC};
use crate::Codec;

#[derive(Clone, Debug, Display, Eq, PartialEq)]
#[rustfmt::skip]
#[display(
    fmt = "wal info height {}, round {}, step {:?}",
    height, round, step,
)]
pub struct WalInfo<T: Codec> {
    pub height: u64,
    pub round:  u64,
    pub step:   Step,
    pub lock:   Option<WalLock<T>>,
}

impl<T: Codec> WalInfo<T> {
    pub fn to_smr_base(&self) -> SMRBase {
        let lock = if let Some(polc) = &self.lock {
            Some(polc.to_lock())
        } else {
            None
        };

        SMRBase {
            height: self.height,
            round:  self.round,
            step:   self.step.clone(),
            polc:   lock,
        }
    }
}

#[derive(Clone, Debug, Display, PartialEq, Eq)]
#[display(fmt = "wal lock round {}, qc {:?}", lock_round, lock_votes)]
pub struct WalLock<T: Codec> {
    pub lock_round: u64,
    pub lock_votes: AggregatedVote,
    pub content:    T,
}

impl<T: Codec> WalLock<T> {
    pub fn to_polc(&self) -> PoLC {
        PoLC {
            lock_round: self.lock_round,
            lock_votes: self.lock_votes.clone(),
        }
    }

    pub fn to_lock(&self) -> Lock {
        Lock {
            round: self.lock_round,
            hash:  self.lock_votes.block_hash.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SMRBase {
    pub height: u64,
    pub round:  u64,
    pub step:   Step,
    pub polc:   Option<Lock>,
}

#[cfg(test)]
mod test {
    use std::error::Error;

    use bytes::Bytes;
    use rand::random;

    use super::*;
    use crate::types::{AggregatedSignature, VoteType};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Pill {
        inner: Vec<u8>,
    }

    impl Codec for Pill {
        fn encode(&self) -> Result<Bytes, Box<dyn Error + Send>> {
            Ok(Bytes::from(self.inner.clone()))
        }

        fn decode(data: Bytes) -> Result<Self, Box<dyn Error + Send>> {
            Ok(Pill {
                inner: data.as_ref().to_vec(),
            })
        }
    }

    impl Pill {
        fn new() -> Self {
            Pill {
                inner: (0..128).map(|_| random::<u8>()).collect::<Vec<_>>(),
            }
        }
    }

    fn mock_qc() -> AggregatedVote {
        let aggregated_signature = AggregatedSignature {
            signature:      Bytes::default(),
            address_bitmap: Bytes::default(),
        };

        AggregatedVote {
            signature:  aggregated_signature,
            vote_type:  VoteType::Precommit,
            height:     0u64,
            round:      0u64,
            block_hash: Bytes::default(),
            leader:     Bytes::default(),
        }
    }

    #[test]
    fn test_display() {
        let wal_lock = WalLock {
            lock_round: 0,
            lock_votes: mock_qc(),
            content:    Pill::new(),
        };

        let wal_info = WalInfo {
            height: 0,
            round:  0,
            step:   Step::Propose,
            lock:   Some(wal_lock),
        };

        println!("{}", wal_info);
    }
}
