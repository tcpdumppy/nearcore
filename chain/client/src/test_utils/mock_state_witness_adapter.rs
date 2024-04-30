use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use near_async::messaging::CanSend;

use crate::stateless_validation::state_witness_actor::DistributeStateWitnessRequest;

#[derive(Clone, Default)]
pub struct MockStateWitnessAdapter {
    distribution_request: Arc<RwLock<VecDeque<DistributeStateWitnessRequest>>>,
}

impl CanSend<DistributeStateWitnessRequest> for MockStateWitnessAdapter {
    fn send(&self, msg: DistributeStateWitnessRequest) {
        self.distribution_request.write().unwrap().push_back(msg);
    }
}

impl MockStateWitnessAdapter {
    pub fn pop_distribution_request(&self) -> Option<DistributeStateWitnessRequest> {
        self.distribution_request.write().unwrap().pop_front()
    }
}
