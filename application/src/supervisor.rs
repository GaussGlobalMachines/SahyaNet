use std::collections::HashMap;

use commonware_consensus::{Supervisor as Su, simplex::types::View};
use seismicbft_types::PublicKey;

#[derive(Clone)]
pub struct Supervisor {
    participants: Vec<PublicKey>,
    participants_map: HashMap<PublicKey, u32>,
}

impl Supervisor {
    pub fn new(mut participants: Vec<PublicKey>) -> Self {
        // Setup participants
        participants.sort();
        let mut participants_map = HashMap::with_capacity(participants.len());
        for (index, validator) in participants.iter().enumerate() {
            participants_map.insert(validator.clone(), index as u32);
        }
        // Return supervisor
        Self {
            participants,
            participants_map,
        }
    }
}

impl Su for Supervisor {
    type Index = View;

    type PublicKey = PublicKey;

    fn leader(&self, index: Self::Index) -> Option<Self::PublicKey> {
        let index = index as usize % self.participants.len();

        // todo: No copy on pub key for some reason but that should be possible
        Some(self.participants[index].clone())
    }

    fn participants(&self, _index: Self::Index) -> Option<&Vec<Self::PublicKey>> {
        Some(&self.participants)
    }

    fn is_participant(&self, _index: Self::Index, candidate: &Self::PublicKey) -> Option<u32> {
        self.participants_map.get(candidate).cloned()
    }
}
