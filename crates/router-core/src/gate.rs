use std::sync::atomic::Ordering::Relaxed;

use smallvec::SmallVec;

use crate::backend::{BackendId, Snapshot};

#[inline]
pub fn admits(b: &crate::backend::Backend, need_kv: i64, sigma: f64) -> bool {
    let projected = b.live.kv_projected_tokens.load(Relaxed) + need_kv;
    let kv_ok = projected <= (sigma * b.caps.kv_capacity_tokens as f64) as i64;
    let slot_ok = b.live.inflight.load(Relaxed) < b.caps.max_num_seqs;
    kv_ok && slot_ok
}

pub fn eligible(
    snap: &Snapshot,
    need_kv: i64,
    sigma: f64,
    out: &mut SmallVec<[BackendId; 16]>,
) {
    out.clear();
    for &id in snap.healthy.iter() {
        let b = &snap.backends[id.0 as usize];
        if admits(b, need_kv, sigma) {
            out.push(id);
        }
    }
}
