use crate::model::{ResourceAction, ResourceIdentity, ResourceSummary};
use std::collections::BTreeSet;

pub fn calculate_pruned(
    current: &[ResourceSummary],
    target: &[ResourceSummary],
) -> Vec<ResourceSummary> {
    let target_set: BTreeSet<ResourceIdentity> =
        target.iter().map(ResourceSummary::identity).collect();

    current
        .iter()
        .filter(|resource| !target_set.contains(&resource.identity()))
        .map(|resource| ResourceSummary {
            api_version: resource.api_version.clone(),
            kind: resource.kind.clone(),
            namespace: resource.namespace.clone(),
            name: resource.name.clone(),
            action: ResourceAction::Pruned,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_removed_resources() {
        let current = vec![resource("ConfigMap", "old"), resource("Deployment", "app")];
        let target = vec![resource("Deployment", "app")];

        let pruned = calculate_pruned(&current, &target);
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].name, "old");
    }

    fn resource(kind: &str, name: &str) -> ResourceSummary {
        ResourceSummary {
            api_version: "v1".into(),
            kind: kind.into(),
            namespace: "default".into(),
            name: name.into(),
            action: ResourceAction::Unchanged,
        }
    }
}
