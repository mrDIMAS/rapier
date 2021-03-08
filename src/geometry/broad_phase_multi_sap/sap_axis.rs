use super::{BroadPhaseProxies, SAPEndpoint, NUM_SENTINELS};
use crate::math::Real;
use bit_vec::BitVec;
use parry::bounding_volume::BoundingVolume;
use parry::utils::hashmap::HashMap;
use std::cmp::Ordering;

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Clone)]
pub(crate) struct SAPAxis {
    pub min_bound: Real,
    pub max_bound: Real,
    pub endpoints: Vec<SAPEndpoint>,
    #[cfg_attr(feature = "serde-serialize", serde(skip))]
    pub new_endpoints: Vec<(SAPEndpoint, usize)>, // Workspace
}

impl SAPAxis {
    pub fn new(min_bound: Real, max_bound: Real) -> Self {
        assert!(min_bound <= max_bound);

        Self {
            min_bound,
            max_bound,
            endpoints: vec![SAPEndpoint::start_sentinel(), SAPEndpoint::end_sentinel()],
            new_endpoints: Vec::new(),
        }
    }

    pub fn batch_insert(
        &mut self,
        dim: usize,
        new_proxies: &[usize],
        proxies: &BroadPhaseProxies,
        reporting: Option<&mut HashMap<(u32, u32), bool>>,
    ) {
        if new_proxies.is_empty() {
            return;
        }

        self.new_endpoints.clear();

        for proxy_id in new_proxies {
            let proxy = &proxies[*proxy_id];
            assert!(proxy.aabb.mins[dim] <= self.max_bound);
            assert!(proxy.aabb.maxs[dim] >= self.min_bound);
            let start_endpoint =
                SAPEndpoint::start_endpoint(proxy.aabb.mins[dim], *proxy_id as u32);
            let end_endpoint = SAPEndpoint::end_endpoint(proxy.aabb.maxs[dim], *proxy_id as u32);

            self.new_endpoints.push((start_endpoint, 0));
            self.new_endpoints.push((end_endpoint, 0));
        }

        self.new_endpoints
            .sort_by(|a, b| a.0.value.partial_cmp(&b.0.value).unwrap_or(Ordering::Equal));

        let mut curr_existing_index = self.endpoints.len() - NUM_SENTINELS - 1;
        let new_num_endpoints = self.endpoints.len() + self.new_endpoints.len();
        self.endpoints
            .resize(new_num_endpoints, SAPEndpoint::end_sentinel());
        let mut curr_shift_index = new_num_endpoints - NUM_SENTINELS - 1;

        // Sort the endpoints.
        // TODO: specialize for the case where this is the
        // first time we insert endpoints to this axis?
        for new_endpoint in self.new_endpoints.iter_mut().rev() {
            loop {
                let existing_endpoint = self.endpoints[curr_existing_index];
                if existing_endpoint.value <= new_endpoint.0.value {
                    break;
                }

                self.endpoints[curr_shift_index] = existing_endpoint;

                curr_shift_index -= 1;
                curr_existing_index -= 1;
            }

            self.endpoints[curr_shift_index] = new_endpoint.0;
            new_endpoint.1 = curr_shift_index;
            curr_shift_index -= 1;
        }

        // Report pairs using a single mbp pass on each new endpoint.
        let endpoints_wo_last_sentinel = &self.endpoints[..self.endpoints.len() - 1];
        if let Some(reporting) = reporting {
            for (endpoint, endpoint_id) in self.new_endpoints.drain(..).filter(|e| e.0.is_start()) {
                let proxy1 = &proxies[endpoint.proxy() as usize];
                let min = endpoint.value;
                let max = proxy1.aabb.maxs[dim];

                for endpoint2 in &endpoints_wo_last_sentinel[endpoint_id + 1..] {
                    if endpoint2.proxy() == endpoint.proxy() {
                        continue;
                    }

                    let proxy2 = &proxies[endpoint2.proxy() as usize];

                    // NOTE: some pairs with equal aabb.mins[dim] may end up being reported twice.
                    if (endpoint2.is_start() && endpoint2.value < max)
                        || (endpoint2.is_end() && proxy2.aabb.mins[dim] <= min)
                    {
                        // Report pair.
                        if proxy1.aabb.intersects(&proxy2.aabb) {
                            // Report pair.
                            let pair = super::sort2(endpoint.proxy(), endpoint2.proxy());
                            reporting.insert(pair, true);
                        }
                    }
                }
            }
        }
    }

    pub fn delete_out_of_bounds_proxies(&self, existing_proxies: &mut BitVec) -> usize {
        let mut deleted = 0;
        for endpoint in &self.endpoints {
            if endpoint.value < self.min_bound {
                let proxy_idx = endpoint.proxy() as usize;
                if endpoint.is_end() && existing_proxies[proxy_idx] {
                    existing_proxies.set(proxy_idx, false);
                    deleted += 1;
                }
            } else {
                break;
            }
        }

        for endpoint in self.endpoints.iter().rev() {
            if endpoint.value > self.max_bound {
                let proxy_idx = endpoint.proxy() as usize;
                if endpoint.is_start() && existing_proxies[proxy_idx] {
                    existing_proxies.set(endpoint.proxy() as usize, false);
                    deleted += 1;
                }
            } else {
                break;
            }
        }

        deleted
    }

    pub fn delete_out_of_bounds_endpoints(&mut self, existing_proxies: &BitVec) {
        self.endpoints
            .retain(|endpt| endpt.is_sentinel() || existing_proxies[endpt.proxy() as usize])
    }

    pub fn update_endpoints(
        &mut self,
        dim: usize,
        proxies: &BroadPhaseProxies,
        reporting: &mut HashMap<(u32, u32), bool>,
    ) {
        let last_endpoint = self.endpoints.len() - NUM_SENTINELS;
        for i in NUM_SENTINELS..last_endpoint {
            let mut endpoint_i = self.endpoints[i];
            let aabb_i = proxies[endpoint_i.proxy() as usize].aabb;

            if endpoint_i.is_start() {
                endpoint_i.value = aabb_i.mins[dim];
            } else {
                endpoint_i.value = aabb_i.maxs[dim];
            }

            let mut j = i;

            if endpoint_i.is_start() {
                while endpoint_i.value < self.endpoints[j - 1].value {
                    let endpoint_j = self.endpoints[j - 1];
                    self.endpoints[j] = endpoint_j;

                    if endpoint_j.is_end() {
                        // Report start collision.
                        if aabb_i.intersects(&proxies[endpoint_j.proxy() as usize].aabb) {
                            let pair = super::sort2(endpoint_i.proxy(), endpoint_j.proxy());
                            reporting.insert(pair, true);
                        }
                    }

                    j -= 1;
                }
            } else {
                while endpoint_i.value < self.endpoints[j - 1].value {
                    let endpoint_j = self.endpoints[j - 1];
                    self.endpoints[j] = endpoint_j;

                    if endpoint_j.is_start() {
                        // Report end collision.
                        if !aabb_i.intersects(&proxies[endpoint_j.proxy() as usize].aabb) {
                            let pair = super::sort2(endpoint_i.proxy(), endpoint_j.proxy());
                            reporting.insert(pair, false);
                        }
                    }

                    j -= 1;
                }
            }

            self.endpoints[j] = endpoint_i;
        }

        // println!(
        //     "Num start swaps: {}, end swaps: {}, dim: {}",
        //     num_start_swaps, num_end_swaps, dim
        // );
    }
}