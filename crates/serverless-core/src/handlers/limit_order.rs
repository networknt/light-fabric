//! Traversal topology for the two insertion-only Java String maps used by the
//! qualified Lambda limiter. This stores keys, not quota/counter state. Source:
//! preserved Corretto 25 HashMap and ConcurrentHashMap; no deletion API.
use std::cmp::Ordering;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Quota,
    Counter,
}
#[derive(Clone)]
struct Node {
    key: String,
    hash: u32,
    parent: Option<usize>,
    left: Option<usize>,
    right: Option<usize>,
    red: bool,
}
#[derive(Clone, Default)]
struct Bin {
    nodes: Vec<Node>,
    order: Vec<usize>,
    tree: bool,
    root: Option<usize>,
}
impl Bin {
    fn append(&mut self, key: String, hash: u32) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node {
            key,
            hash,
            parent: None,
            left: None,
            right: None,
            red: true,
        });
        self.order.push(id);
        id
    }
    fn compare(&self, a: usize, b: usize) -> Ordering {
        (self.nodes[a].hash as i32)
            .cmp(&(self.nodes[b].hash as i32))
            .then_with(|| {
                self.nodes[a]
                    .key
                    .encode_utf16()
                    .cmp(self.nodes[b].key.encode_utf16())
            })
    }
    fn rotate_left(&mut self, p: usize) {
        let r = self.nodes[p].right.expect("rotation right child");
        let parent = self.nodes[p].parent;
        self.nodes[p].right = self.nodes[r].left;
        if let Some(child) = self.nodes[r].left {
            self.nodes[child].parent = Some(p);
        }
        self.nodes[r].parent = parent;
        if let Some(parent) = parent {
            if self.nodes[parent].left == Some(p) {
                self.nodes[parent].left = Some(r);
            } else {
                self.nodes[parent].right = Some(r);
            }
        } else {
            self.root = Some(r);
        }
        self.nodes[r].left = Some(p);
        self.nodes[p].parent = Some(r);
    }
    fn rotate_right(&mut self, p: usize) {
        let l = self.nodes[p].left.expect("rotation left child");
        let parent = self.nodes[p].parent;
        self.nodes[p].left = self.nodes[l].right;
        if let Some(child) = self.nodes[l].right {
            self.nodes[child].parent = Some(p);
        }
        self.nodes[l].parent = parent;
        if let Some(parent) = parent {
            if self.nodes[parent].right == Some(p) {
                self.nodes[parent].right = Some(l);
            } else {
                self.nodes[parent].left = Some(l);
            }
        } else {
            self.root = Some(l);
        }
        self.nodes[l].right = Some(p);
        self.nodes[p].parent = Some(l);
    }
    fn balance(&mut self, mut x: usize) {
        while let Some(parent) = self.nodes[x].parent {
            if !self.nodes[parent].red {
                break;
            }
            let Some(grand) = self.nodes[parent].parent else {
                break;
            };
            if self.nodes[grand].left == Some(parent) {
                if self.nodes[grand]
                    .right
                    .is_some_and(|uncle| self.nodes[uncle].red)
                {
                    let uncle = self.nodes[grand].right.unwrap();
                    self.nodes[uncle].red = false;
                    self.nodes[parent].red = false;
                    self.nodes[grand].red = true;
                    x = grand;
                } else {
                    if self.nodes[parent].right == Some(x) {
                        x = parent;
                        self.rotate_left(x);
                    }
                    let parent = self.nodes[x].parent.unwrap();
                    let grand = self.nodes[parent].parent.unwrap();
                    self.nodes[parent].red = false;
                    self.nodes[grand].red = true;
                    self.rotate_right(grand);
                }
            } else if self.nodes[grand]
                .left
                .is_some_and(|uncle| self.nodes[uncle].red)
            {
                let uncle = self.nodes[grand].left.unwrap();
                self.nodes[uncle].red = false;
                self.nodes[parent].red = false;
                self.nodes[grand].red = true;
                x = grand;
            } else {
                if self.nodes[parent].left == Some(x) {
                    x = parent;
                    self.rotate_right(x);
                }
                let parent = self.nodes[x].parent.unwrap();
                let grand = self.nodes[parent].parent.unwrap();
                self.nodes[parent].red = false;
                self.nodes[grand].red = true;
                self.rotate_left(grand);
            }
        }
        self.nodes[self.root.unwrap()].red = false;
    }
    fn attach(&mut self, id: usize) -> Option<usize> {
        let Some(mut p) = self.root else {
            self.root = Some(id);
            self.nodes[id].red = false;
            return None;
        };
        loop {
            let left = self.compare(id, p).is_lt();
            let next = if left {
                self.nodes[p].left
            } else {
                self.nodes[p].right
            };
            if let Some(next) = next {
                p = next;
            } else {
                if left {
                    self.nodes[p].left = Some(id);
                } else {
                    self.nodes[p].right = Some(id);
                }
                self.nodes[id].parent = Some(p);
                self.balance(id);
                return Some(p);
            }
        }
    }
    fn root_to_front(&mut self) {
        let root = self.root.unwrap();
        let at = self.order.iter().position(|id| *id == root).unwrap();
        self.order.remove(at);
        self.order.insert(0, root);
    }
    fn treeify(&mut self, kind: Kind) {
        self.tree = true;
        if kind == Kind::Quota {
            self.root = None;
            for id in self.order.clone() {
                self.nodes[id].parent = None;
                self.nodes[id].left = None;
                self.nodes[id].right = None;
                self.nodes[id].red = true;
                self.attach(id);
            }
            self.root_to_front();
        }
    }
    fn insert(&mut self, key: String, hash: u32, kind: Kind) -> bool {
        if self.nodes.iter().any(|node| node.key == key) {
            return false;
        }
        let id = self.append(key, hash);
        if self.tree {
            self.order.pop();
            if kind == Kind::Counter {
                self.order.insert(0, id);
            } else {
                let parent = self.attach(id).expect("nonempty quota tree");
                let at = self.order.iter().position(|node| *node == parent).unwrap();
                self.order.insert(at + 1, id);
                self.root_to_front();
            }
        }
        true
    }
    fn partition(&self, bit: usize, kind: Kind) -> (Self, Self) {
        let mut low = Self::default();
        let mut high = Self::default();
        // CHM list transfer reuses the final equal-bit run and prepends earlier
        // nodes. Its order differs from HashMap's stable low/high partition.
        let last_run = if kind == Kind::Counter && !self.tree {
            self.order
                .windows(2)
                .enumerate()
                .filter(|(_, pair)| {
                    (self.nodes[pair[0]].hash as usize & bit)
                        != (self.nodes[pair[1]].hash as usize & bit)
                })
                .map(|(i, _)| i + 1)
                .next_back()
                .unwrap_or(0)
        } else {
            0
        };
        for (at, id) in self.order.iter().enumerate() {
            let node = &self.nodes[*id];
            let bin = if node.hash as usize & bit == 0 {
                &mut low
            } else {
                &mut high
            };
            let id = bin.append(node.key.clone(), node.hash);
            if kind == Kind::Counter && !self.tree && at < last_run {
                bin.order.pop();
                bin.order.insert(0, id);
            }
        }
        if self.tree {
            // An unsplit tree retains its existing shape and iteration links.
            if low.nodes.is_empty() && high.nodes.len() > 6 {
                high = self.clone();
            } else if high.nodes.is_empty() && low.nodes.len() > 6 {
                low = self.clone();
            } else {
                if low.nodes.len() > 6 {
                    low.treeify(kind);
                }
                if high.nodes.len() > 6 {
                    high.treeify(kind);
                }
            }
        }
        (low, high)
    }
}

#[derive(Clone)]
struct Table {
    kind: Kind,
    bins: Vec<Bin>,
    size: usize,
}
impl Table {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            bins: vec![Bin::default(); 16],
            size: 0,
        }
    }
    fn resize(&mut self) {
        let bit = self.bins.len();
        let old = std::mem::replace(&mut self.bins, vec![Bin::default(); bit * 2]);
        for (i, bin) in old.into_iter().enumerate() {
            let (low, high) = bin.partition(bit, self.kind);
            self.bins[i] = low;
            self.bins[i + bit] = high;
        }
    }
    fn insert(&mut self, key: String) {
        let raw = key
            .encode_utf16()
            .fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(u32::from(c)));
        let hash = raw ^ (raw >> 16);
        let hash = if self.kind == Kind::Counter {
            hash & 0x7fff_ffff
        } else {
            hash
        };
        let index = hash as usize & (self.bins.len() - 1);
        let previous = self.bins[index].nodes.len();
        if !self.bins[index].insert(key, hash, self.kind) {
            return;
        }
        self.size += 1;
        if !self.bins[index].tree && previous >= 8 {
            if self.bins.len() < 64 {
                if self.kind == Kind::Quota {
                    self.resize();
                } else {
                    let target = (self.bins.len() * 3 + 1).next_power_of_two();
                    while self.bins.len() * 3 / 4 < target {
                        self.resize();
                    }
                }
            } else {
                self.bins[index].treeify(self.kind);
            }
        }
        let threshold = self.bins.len() * 3 / 4;
        if (self.kind == Kind::Quota && self.size > threshold)
            || (self.kind == Kind::Counter && self.size >= threshold)
        {
            self.resize();
        }
    }
    fn keys(&self) -> impl Iterator<Item = &str> {
        self.bins
            .iter()
            .flat_map(|bin| bin.order.iter().map(|id| bin.nodes[*id].key.as_str()))
    }
}

pub(super) fn quota_order(keys: &[String]) -> Vec<String> {
    let mut table = Table::new(Kind::Quota);
    for key in keys {
        table.insert(key.clone());
    }
    table.keys().map(str::to_owned).collect()
}
#[derive(Clone)]
pub(super) struct CounterOrder(Table);
impl CounterOrder {
    pub(super) fn new(quota_keys: &[String]) -> Self {
        let mut table = Table::new(Kind::Counter);
        for key in quota_keys {
            table.insert(key.clone());
        }
        Self(table)
    }
    pub(super) fn label(&mut self, path: &str) -> Option<String> {
        if path.len() > gateway_policy::rate_limit::MAX_RATE_LIMIT_KEY_BYTES {
            return None;
        }
        if let Some(key) = self.0.keys().find(|key| path.starts_with(key)) {
            return Some(key.to_owned());
        }
        if self.0.size >= gateway_policy::rate_limit::MAX_RATE_LIMIT_BUCKETS {
            return None;
        }
        self.0.insert(path.to_owned());
        Some(path.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_growth_is_bounded_without_forgetting_existing_labels() {
        let mut counters = CounterOrder::new(&[]);
        for i in 0..gateway_policy::rate_limit::MAX_RATE_LIMIT_BUCKETS {
            assert!(counters.label(&format!("/pressure/{i}/")).is_some());
        }
        assert!(counters.label("/new-identity/").is_none());
        assert_eq!(counters.label("/pressure/0/"), Some("/pressure/0/".into()));
        assert_eq!(
            counters.0.size,
            gateway_policy::rate_limit::MAX_RATE_LIMIT_BUCKETS
        );
    }
    #[test]
    fn quota_and_counter_traversal_match_preserved_java25_component_traces() {
        let observations: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/data/wp12_limit_java25_traversal.json"
        ))
        .unwrap();
        let strings = |values: &serde_json::Value| {
            values
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        for case in observations["cases"].as_array().unwrap() {
            let expected_quotas = strings(&case["quotaOrder"]);
            assert_eq!(
                quota_order(&strings(&case["declared"])),
                expected_quotas,
                "quota {}",
                case["name"]
            );
            let mut counters = CounterOrder::new(&expected_quotas);
            assert_eq!(
                counters.0.keys().collect::<Vec<_>>(),
                strings(&case["counterOrder"]),
                "initial {}",
                case["name"]
            );
            for step in case["steps"].as_array().unwrap() {
                assert_eq!(
                    counters.label(step["path"].as_str().unwrap()).unwrap(),
                    step["label"].as_str().unwrap(),
                    "label {}",
                    case["name"]
                );
                assert_eq!(
                    counters.0.keys().collect::<Vec<_>>(),
                    strings(&step["counterOrder"]),
                    "growth {} {}",
                    case["name"],
                    step["path"]
                );
            }
        }
    }
}
