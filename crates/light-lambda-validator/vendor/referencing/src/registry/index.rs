//! Lookup structures produced by the build pass.
use std::sync::Arc;

use fluent_uri::Uri;

use serde_json::Value;

use crate::{
    anchor::Anchor, draft::Draft, pointer::ParsedPointer, small_map::SmallMap, ResourceRef,
};

use super::build::StoredResource;

/// Lookup tables mapping canonical URIs to resources and anchors.
#[derive(Clone, Default)]
pub(super) struct Index<'a> {
    pub(super) strict_identities: bool,
    pub(super) conflicting_identity: bool,
    pub(super) schema_entries: std::collections::BTreeMap<usize, SchemaEntryContext<'a>>,
    pub(super) resources: SmallMap<Arc<Uri<String>>, IndexedResource<'a>>,
    pub(super) anchors: SmallMap<Arc<Uri<String>>, SmallMap<Box<str>, IndexedAnchor<'a>>>,
}
impl std::fmt::Debug for Index<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Index")
            .field("resources", &self.resources)
            .field("anchors", &self.anchors)
            .finish()
    }
}

/// Opt-in context for a captured schema entry. Address keys are identity only;
/// they are never dereferenced. The stored owner retains the containing value.
#[derive(Debug, Clone)]
pub(crate) struct SchemaEntryContext<'a> {
    pub(super) owner: Arc<StoredResource<'a>>,
    pub(super) pointer: String,
    pub(crate) effective_base: Arc<Uri<String>>,
    pub(crate) draft: Draft,
    pub(crate) metaschema: Option<Arc<str>>,
}

fn entries<K, V, const N: usize>(map: &SmallMap<K, V, N>) -> impl Iterator<Item = (&K, &V)> {
    let (small, large) = match map {
        SmallMap::Small(m) => (Some(m), None),
        SmallMap::Large(m) => (None, Some(m)),
    };
    small
        .into_iter()
        .flat_map(|m| m.iter())
        .chain(large.into_iter().flat_map(|m| m.iter()))
}

impl<'a> Index<'a> {
    pub(super) fn equivalent(a: &Value, b: &Value) -> bool {
        // Whitespace normalization only; retain member order and numeric lexemes.
        match (a, b) {
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|((ak, av), (bk, bv))| ak == bk && Self::equivalent(av, bv))
            }
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| Self::equivalent(a, b))
            }
            (Value::Number(a), Value::Number(b)) => a.to_string() == b.to_string(),
            _ => a == b,
        }
    }
    fn entry_context_equivalent(&self, a: &Value, b: &Value) -> bool {
        let a = self.schema_entries.get(&(std::ptr::from_ref(a) as usize));
        let b = self.schema_entries.get(&(std::ptr::from_ref(b) as usize));
        match (a, b) {
            (Some(a), Some(b)) => {
                a.draft == b.draft
                    && a.effective_base == b.effective_base
                    && a.metaschema == b.metaschema
            }
            (None, None) => true,
            // A non-entry root and captured entry may share the same schema
            // only when no inherited dialect/base information is missing.
            (Some(a), None) | (None, Some(a)) => a.metaschema.is_none(),
        }
    }
    fn check_identity(
        &mut self,
        key: &Arc<Uri<String>>,
        draft: Draft,
        has_id: bool,
        contents: &Value,
    ) {
        if !self.strict_identities {
            return;
        }
        if has_id {
            if let Some(previous) = self.resources.get(key).and_then(IndexedResource::resolve) {
                if previous.draft() != draft
                    || !Self::equivalent(previous.contents(), contents)
                    || !self.entry_context_equivalent(previous.contents(), contents)
                {
                    self.conflicting_identity = true;
                }
            }
        }
        if let Some(anchors) = self.anchors.get(key) {
            for anchor in draft.anchors(contents) {
                if let Some(previous) = anchors.get(anchor.name()).and_then(IndexedAnchor::resolve)
                {
                    let previous = match previous {
                        Anchor::Default { resource, .. } | Anchor::Dynamic { resource, .. } => {
                            resource
                        }
                    };
                    if previous.draft() != draft
                        || !Self::equivalent(previous.contents(), contents)
                        || !self.entry_context_equivalent(previous.contents(), contents)
                    {
                        self.conflicting_identity = true;
                    }
                }
            }
        }
    }
    pub(super) fn check_baseline(&mut self, baseline: &super::Registry<'_>) {
        if !self.strict_identities {
            return;
        }
        for (uri, entry) in entries(&self.resources) {
            if let (Some(current), Some(previous)) =
                (entry.resolve(), baseline.resource_by_uri(uri))
            {
                if current.draft() != previous.draft()
                    || !Self::equivalent(current.contents(), previous.contents())
                {
                    self.conflicting_identity = true;
                }
            }
        }
        for (uri, anchors) in entries(&self.anchors) {
            for (name, entry) in entries(anchors) {
                if let (Some(current), Ok(previous)) = (entry.resolve(), baseline.anchor(uri, name))
                {
                    let current = match current {
                        Anchor::Default { resource, .. } | Anchor::Dynamic { resource, .. } => {
                            resource
                        }
                    };
                    let previous = match previous {
                        Anchor::Default { resource, .. } | Anchor::Dynamic { resource, .. } => {
                            resource
                        }
                    };
                    if current.draft() != previous.draft()
                        || !Self::equivalent(current.contents(), previous.contents())
                    {
                        self.conflicting_identity = true;
                    }
                }
            }
        }
    }
    pub(super) fn validate_identities(&self) -> Result<(), crate::Error> {
        if self.conflicting_identity {
            return Err(crate::Error::unretrievable(
                "schema-entry",
                "validator: conflicting captured schema identity".into(),
            ));
        }
        Ok(())
    }

    /// Register a document: insert its resource entry and all its anchors.
    pub(super) fn register_document(
        &mut self,
        key: &Arc<Uri<String>>,
        document: &Arc<StoredResource<'a>>,
    ) {
        if let Some(contents) = document.borrowed_contents() {
            self.register_borrowed_subresource(key, document.draft(), true, contents);
        } else {
            let pointer = ParsedPointer::default();
            self.register_owned_subresource(
                key,
                document,
                &pointer,
                document.draft(),
                true,
                document.contents(),
            );
        }
    }

    /// Register a subresource discovered during BFS traversal of a borrowed document.
    /// If `has_id` is true, the subresource is also registered as a resource entry.
    pub(super) fn register_borrowed_subresource(
        &mut self,
        key: &Arc<Uri<String>>,
        draft: Draft,
        has_id: bool,
        contents: &'a Value,
    ) {
        self.check_identity(key, draft, has_id, contents);
        if has_id {
            self.resources.insert(
                Arc::clone(key),
                IndexedResource::Borrowed(ResourceRef::new(contents, draft)),
            );
        }
        let anchors = self.anchors.get_or_insert_default(Arc::clone(key));
        for anchor in draft.anchors(contents) {
            anchors.insert(
                anchor.name().to_string().into_boxed_str(),
                IndexedAnchor::Borrowed(anchor),
            );
        }
    }

    /// Register a subresource discovered during BFS traversal of an owned document.
    /// If `has_id` is true, the subresource is also registered as a resource entry.
    pub(super) fn register_owned_subresource(
        &mut self,
        key: &Arc<Uri<String>>,
        document: &Arc<StoredResource<'a>>,
        pointer: &ParsedPointer,
        draft: Draft,
        has_id: bool,
        contents: &Value,
    ) {
        self.check_identity(key, draft, has_id, contents);
        if has_id {
            self.resources.insert(
                Arc::clone(key),
                IndexedResource::Owned {
                    document: Arc::clone(document),
                    pointer: pointer.clone(),
                    draft,
                },
            );
        }
        let anchors = self.anchors.get_or_insert_default(Arc::clone(key));
        for anchor in draft.anchors(contents) {
            let (name, kind) = match anchor {
                Anchor::Default { name, .. } => (name, AnchorKind::Default),
                Anchor::Dynamic { name, .. } => (name, AnchorKind::Dynamic),
            };
            let name = name.to_string().into_boxed_str();
            anchors.insert(
                name.clone(),
                IndexedAnchor::Owned {
                    document: Arc::clone(document),
                    pointer: pointer.clone(),
                    draft,
                    kind,
                    name,
                },
            );
        }
    }
}

/// A schema resource in the index: either borrowed from the caller or owned by the registry.
#[derive(Debug, Clone)]
pub(super) enum IndexedResource<'a> {
    Borrowed(ResourceRef<'a>),
    Owned {
        document: Arc<StoredResource<'a>>,
        pointer: ParsedPointer,
        draft: Draft,
    },
}

impl IndexedResource<'_> {
    #[inline]
    pub(super) fn resolve(&self) -> Option<ResourceRef<'_>> {
        match self {
            IndexedResource::Borrowed(resource) => {
                Some(ResourceRef::new(resource.contents(), resource.draft()))
            }
            IndexedResource::Owned {
                document,
                pointer,
                draft,
            } => {
                let contents = pointer.lookup(document.contents())?;
                Some(ResourceRef::new(contents, *draft))
            }
        }
    }
}

/// An anchor in the index: either borrowed from the caller or owned by the registry.
#[derive(Debug, Clone)]
pub(super) enum IndexedAnchor<'a> {
    Borrowed(Anchor<'a>),
    Owned {
        document: Arc<StoredResource<'a>>,
        pointer: ParsedPointer,
        draft: Draft,
        kind: AnchorKind,
        name: Box<str>,
    },
}

impl IndexedAnchor<'_> {
    #[inline]
    pub(super) fn resolve(&self) -> Option<Anchor<'_>> {
        match self {
            IndexedAnchor::Borrowed(anchor) => Some(match anchor {
                Anchor::Default { name, resource } => Anchor::Default {
                    name,
                    resource: ResourceRef::new(resource.contents(), resource.draft()),
                },
                Anchor::Dynamic { name, resource } => Anchor::Dynamic {
                    name,
                    resource: ResourceRef::new(resource.contents(), resource.draft()),
                },
            }),
            IndexedAnchor::Owned {
                document,
                pointer,
                draft,
                kind,
                name,
            } => {
                let contents = pointer.lookup(document.contents())?;
                let resource = ResourceRef::new(contents, *draft);
                Some(match kind {
                    AnchorKind::Default => Anchor::Default { name, resource },
                    AnchorKind::Dynamic => Anchor::Dynamic { name, resource },
                })
            }
        }
    }
}

/// Whether an anchor is a plain anchor (`$anchor`) or a dynamic anchor (`$dynamicAnchor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnchorKind {
    Default,
    Dynamic,
}
