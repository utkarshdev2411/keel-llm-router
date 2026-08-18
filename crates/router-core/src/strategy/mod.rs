mod least_requests;
mod p2c;
mod round_robin;

pub use least_requests::LeastRequests;
pub use p2c::P2c;
pub use round_robin::RoundRobin;

use rand::rngs::SmallRng;

use crate::backend::{BackendId, Snapshot};
use crate::features::RequestFeatures;
use crate::trace::DecisionTrace;

pub trait RoutingStrategy: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Pure with respect to (snap, req, rng): same inputs and seed produce
    /// the same output. Must not block, do I/O, or allocate on the happy
    /// path. Returns None only when no healthy backend exists.
    fn pick(
        &self,
        snap: &Snapshot,
        req: &RequestFeatures,
        rng: &mut SmallRng,
        trace: Option<&mut DecisionTrace>,
    ) -> Option<BackendId>;
}
