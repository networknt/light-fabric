//! Requested-capacity accounting, not an allocator/RSS claim. Preflight JSON
//! without building a Value, then reserve before metadata parsing/catalogues.
use super::controller::budget::{MetadataReservation, Reservation};
use super::controller::{Budget, Failure};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::{mem::size_of, sync::Arc};
const MAP_NODE: usize =
    11 * (size_of::<String>() + size_of::<serde_json::Value>()) + 15 * size_of::<usize>();
const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 64;
#[derive(Default)]
struct Tree {
    nodes: usize,
    owned: usize,
}
impl Tree {
    fn add(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.owned = self
            .owned
            .checked_add(bytes)
            .ok_or("metadata accounting overflow")?;
        Ok(())
    }
}
struct Walk<'a> {
    tree: &'a mut Tree,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Walk<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        if self.depth > MAX_DEPTH || self.tree.nodes == MAX_NODES {
            return Err(serde::de::Error::custom("metadata profile"));
        }
        self.tree.nodes += 1;
        self.tree
            .add(size_of::<serde_json::Value>())
            .map_err(serde::de::Error::custom)?;
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Walk<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded metadata JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<(), E> {
        self.tree
            .add(
                s.len()
                    .checked_mul(2)
                    .ok_or_else(|| E::custom("overflow"))?,
            )
            .map_err(E::custom)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut n = 0usize;
        while a
            .next_element_seed(Walk {
                tree: self.tree,
                depth: self.depth + 1,
            })?
            .is_some()
        {
            n += 1;
        }
        self.tree
            .add(
                n.checked_mul(3 * size_of::<serde_json::Value>())
                    .ok_or_else(|| serde::de::Error::custom("overflow"))?,
            )
            .map_err(serde::de::Error::custom)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut n = 0usize;
        while a.next_key_seed(Key(self.tree))?.is_some() {
            n += 1;
            a.next_value_seed(Walk {
                tree: self.tree,
                depth: self.depth + 1,
            })?;
        }
        self.tree
            .add(
                (n + 1)
                    .checked_mul(MAP_NODE)
                    .ok_or_else(|| serde::de::Error::custom("overflow"))?,
            )
            .map_err(serde::de::Error::custom)
    }
}
struct Key<'a>(&'a mut Tree);
impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_str(self)
    }
}
impl<'de> Visitor<'de> for Key<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("metadata key")
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<(), E> {
        self.0
            .add(
                s.len()
                    .checked_mul(2)
                    .ok_or_else(|| E::custom("overflow"))?,
            )
            .map_err(E::custom)
    }
}
/// Explicit review ceilings for the existing requested-capacity estimates.
/// No Default/Deserialize and no RSS, allocator or worker-memory guarantee.
#[derive(Clone, Copy, Debug)]
pub struct MetadataLimits {
    pub max_wire_bytes: usize,
    pub max_peak_bytes: usize,
    pub max_retained_bytes: usize,
}
impl MetadataLimits {
    pub fn validate(&self, parent_limit: usize) -> Result<(), Failure> {
        if self.max_wire_bytes == 0
            || self.max_wire_bytes > 4 << 20
            || self.max_retained_bytes == 0
            || self.max_retained_bytes > self.max_peak_bytes
            || self.max_peak_bytes > parent_limit
        {
            return Err(Failure::AdapterInvariant);
        }
        Ok(())
    }
    fn admit(&self, first: usize, transient: usize, retained: usize) -> Result<(), Failure> {
        let peak = first
            .checked_add(transient)
            .and_then(|n| n.checked_add(retained))
            .ok_or(Failure::ParentBudget)?;
        if peak > self.max_peak_bytes || retained > self.max_retained_bytes {
            return Err(Failure::ParentBudget);
        }
        Ok(())
    }
}

pub fn metadata(budget: &Arc<Budget>, bytes: &[u8]) -> Result<MetadataReservation, Failure> {
    metadata_in(budget, bytes, None)
}

pub fn metadata_limited(
    budget: &Arc<Budget>,
    bytes: &[u8],
    limits: MetadataLimits,
) -> Result<MetadataReservation, Failure> {
    limits.validate(budget.limit())?;
    metadata_in(budget, bytes, Some(limits))
}

fn metadata_in(
    budget: &Arc<Budget>,
    bytes: &[u8],
    limits: Option<MetadataLimits>,
) -> Result<MetadataReservation, Failure> {
    if bytes.len() > 4 << 20 {
        return Err(Failure::Frame);
    }
    if let Some(limits) = limits
        && bytes.len() > limits.max_wire_bytes
    {
        return Err(Failure::ParentBudget);
    }
    // serde's escaped-string scratch can realloc old+new. Reserve it before
    // even the non-owning pass, and retain it through ordinary parsing.
    let scratch_bytes = bytes
        .len()
        .checked_mul(3)
        .and_then(|n| n.checked_add(4096))
        .ok_or(Failure::ParentBudget)?;
    if let Some(limits) = limits
        && scratch_bytes > limits.max_peak_bytes
    {
        return Err(Failure::ParentBudget);
    }
    let first = budget.reservation(scratch_bytes)?;
    let mut tree = Tree::default();
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    Walk {
        tree: &mut tree,
        depth: 0,
    }
    .deserialize(&mut parser)
    .map_err(|_| Failure::Protocol)?;
    parser.end().map_err(|_| Failure::Protocol)?;
    // Metadata parse/profile/catalogue retain cloned document/path/operation
    // trees; path tables/descriptor/normalization control gets a byte allowance.
    let retained = tree
        .owned
        .checked_mul(6)
        .and_then(|n| n.checked_add(bytes.len().checked_mul(32)?))
        .and_then(|n| n.checked_add(4096))
        .ok_or(Failure::ParentBudget)?;
    let transient = tree
        .owned
        .checked_mul(2)
        .and_then(|n| n.checked_add(scratch_bytes))
        .ok_or(Failure::ParentBudget)?;
    if let Some(limits) = limits {
        limits.admit(scratch_bytes, transient, retained)?;
    }
    let scratch = budget.reservation(transient)?;
    let retained = budget.reservation(retained)?;
    drop(first);
    Ok(MetadataReservation { scratch, retained })
}
pub fn request_bound(bytes: usize, entries: usize) -> Result<usize, Failure> {
    entries
        .checked_add(16)
        .and_then(|n| n.checked_mul(MAP_NODE))
        .and_then(|n| n.checked_add(bytes))
        .and_then(|n| n.checked_mul(16))
        .and_then(|n| n.checked_add(4096))
        .ok_or(Failure::ParentBudget)
}
pub fn settings_bound(settings: &super::Settings) -> Result<usize, Failure> {
    let mut n = size_of::<super::Settings>() + 4 * size_of::<usize>();
    if let Some(v) = &settings.skip_path_prefixes {
        n = n
            .checked_add(
                v.capacity()
                    .checked_mul(size_of::<String>())
                    .ok_or(Failure::ParentBudget)?,
            )
            .ok_or(Failure::ParentBudget)?;
        for s in v {
            n = n.checked_add(s.capacity()).ok_or(Failure::ParentBudget)?;
        }
    }
    Ok(n)
}
pub fn retained_metadata(
    budget: &Arc<Budget>,
    bytes: &[u8],
) -> Result<(Reservation, Reservation), Failure> {
    let m = metadata(budget, bytes)?;
    Ok((m.scratch, m.retained))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controller_accounts_for_capture_and_managed_source_copy() {
        use super::super::controller::{Controller, SourceFormat};
        let small = Controller::prepare(
            std::path::Path::new("not-executed"),
            b"{}",
            SourceFormat::Json,
            Arc::new(Default::default()),
            Default::default(),
        )
        .unwrap();
        let larger = Controller::prepare(
            std::path::Path::new("not-executed"),
            b"{  }",
            SourceFormat::Json,
            Arc::new(Default::default()),
            Default::default(),
        )
        .unwrap();
        assert_eq!(larger.budget().used() - small.budget().used(), 4);
    }
    #[test]
    fn metadata_credits_cover_parse_and_snapshot_lifetimes_and_release_on_error() {
        let bytes = br#"{"openapi":"3.1.0","paths":{"/x":{"get":{}}}}"#;
        let b = Budget::new(1 << 20);
        let m = metadata(&b, bytes).unwrap();
        let total = b.used();
        assert_eq!(total, m.scratch.bytes() + m.retained.bytes());
        drop(m.scratch);
        assert_eq!(b.used(), m.retained.bytes());
        drop(m.retained);
        assert_eq!(b.used(), 0);
        let short = Budget::new(total - 1);
        assert!(metadata(&short, bytes).is_err());
        assert_eq!(short.used(), 0);
        assert!(metadata(&b, b"{broken").is_err());
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn prepared_controller_rejects_before_copy_and_retains_shared_owners() {
        let settings = super::super::Settings {
            skip_path_prefixes: Some(vec!["/test".into()]),
            ..Default::default()
        };
        let profile = super::super::controller::ResourceProfile {
            parent_bytes: 1,
            ..Default::default()
        };
        assert!(
            super::super::controller::Controller::prepare(
                std::path::Path::new("not-executed"),
                b"{}",
                super::super::controller::SourceFormat::Json,
                Arc::new(settings.clone()),
                profile
            )
            .is_err()
        );
        let c = super::super::controller::Controller::prepare(
            std::path::Path::new("not-executed"),
            b"{}",
            super::super::controller::SourceFormat::Json,
            Arc::new(settings),
            Default::default(),
        )
        .unwrap();
        let b = c.budget();
        let stats = c.stats.clone();
        assert!(b.used() > 0);
        drop(c);
        assert!(b.used() > 0);
        drop(stats);
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn response_bytes_retain_reply_credit_until_last_clone_drops() {
        let b = Budget::new(1 << 20);
        let reply = super::super::Reply::decode(br#"{"status":200,"body":"ok"}"#).unwrap();
        let r = super::super::controller::Retained {
            value: reply,
            _credit: b.reserve(4096).unwrap(),
        };
        let bytes = super::super::Response::Retained(r).into_body();
        let clone = bytes.clone();
        drop(bytes);
        assert_eq!(b.used(), 4096);
        assert_eq!(&clone[..], b"ok");
        drop(clone);
        assert_eq!(b.used(), 0);
    }
    #[test]
    fn request_arithmetic_fails_closed_without_allocating_large_inputs() {
        assert!(request_bound(usize::MAX, 1).is_err());
        assert!(request_bound(1, usize::MAX).is_err());
        let b = Budget::new(4096);
        assert!(b.reservation(request_bound(32, 2).unwrap()).is_err());
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn explicit_metadata_ceilings_reuse_existing_charges_without_double_accounting() {
        let bytes = br#"{"paths":{"/pets":{"post":{}}},"name":"Tucker"}"#;
        let budget = Budget::new(1 << 24);
        let baseline = metadata(&budget, bytes).unwrap();
        let peak = budget.peak();
        let retained = baseline.retained.bytes();
        let held = budget.used();
        drop(baseline);
        assert_eq!(budget.used(), 0);
        let limits = MetadataLimits {
            max_wire_bytes: bytes.len(),
            max_peak_bytes: peak,
            max_retained_bytes: retained,
        };
        let guarded = metadata_limited(&budget, bytes, limits).unwrap();
        assert_eq!(budget.used(), held);
        drop(guarded.scratch);
        assert_eq!(budget.used(), retained);
        drop(guarded.retained);
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn invalid_metadata_policy_rejects_before_reservation() {
        let budget = Budget::new(10_000);
        for limits in [
            MetadataLimits {
                max_wire_bytes: 0,
                max_peak_bytes: 100,
                max_retained_bytes: 10,
            },
            MetadataLimits {
                max_wire_bytes: (4 << 20) + 1,
                max_peak_bytes: 100,
                max_retained_bytes: 10,
            },
            MetadataLimits {
                max_wire_bytes: 1,
                max_peak_bytes: 100,
                max_retained_bytes: 0,
            },
            MetadataLimits {
                max_wire_bytes: 1,
                max_peak_bytes: 100,
                max_retained_bytes: 101,
            },
            MetadataLimits {
                max_wire_bytes: 1,
                max_peak_bytes: 10_001,
                max_retained_bytes: 100,
            },
        ] {
            assert!(matches!(
                metadata_limited(&budget, b"{}", limits),
                Err(Failure::AdapterInvariant)
            ));
            assert_eq!(budget.peak(), 0);
        }
    }
    #[test]
    fn metadata_wire_and_initial_scratch_refuse_before_json_parse_or_charge() {
        let budget = Budget::new(10_000);
        for limits in [
            MetadataLimits {
                max_wire_bytes: 1,
                max_peak_bytes: 9_000,
                max_retained_bytes: 1,
            },
            MetadataLimits {
                max_wire_bytes: 10,
                max_peak_bytes: 2,
                max_retained_bytes: 1,
            },
        ] {
            assert!(matches!(
                metadata_limited(&budget, b"bad", limits),
                Err(Failure::ParentBudget)
            ));
            assert_eq!(budget.peak(), 0);
        }
    }
    #[test]
    fn retained_and_peak_policy_rejection_refunds_preflight_without_extra_charge() {
        let bytes = br#"{"path":"/pets"}"#;
        let baseline = Budget::new(1 << 24);
        let reservations = metadata(&baseline, bytes).unwrap();
        let peak = baseline.peak();
        let retained = reservations.retained.bytes();
        for limits in [
            MetadataLimits {
                max_wire_bytes: bytes.len(),
                max_peak_bytes: peak - 1,
                max_retained_bytes: retained,
            },
            MetadataLimits {
                max_wire_bytes: bytes.len(),
                max_peak_bytes: peak,
                max_retained_bytes: retained - 1,
            },
        ] {
            let budget = Budget::new(1 << 24);
            assert!(matches!(
                metadata_limited(&budget, bytes, limits),
                Err(Failure::ParentBudget)
            ));
            assert_eq!(budget.used(), 0);
            assert_eq!(budget.peak(), bytes.len() * 3 + 4096);
        }
    }
    #[test]
    fn checked_metadata_policy_peak_never_wraps() {
        let limits = MetadataLimits {
            max_wire_bytes: 1,
            max_peak_bytes: usize::MAX,
            max_retained_bytes: usize::MAX,
        };
        assert!(matches!(
            limits.admit(usize::MAX, 1, 0),
            Err(Failure::ParentBudget)
        ));
        assert!(matches!(
            limits.admit(1, usize::MAX, 1),
            Err(Failure::ParentBudget)
        ));
    }
}
