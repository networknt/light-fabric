use core::fmt;
use std::sync::Arc;

use fluent_uri::Uri;
use serde_json::Value;

use crate::{list::List, Anchor, Draft, Error, Registry, ResourceRef, VocabularySet};

type EntryProfile = (Draft, VocabularySet, Option<Arc<str>>);

/// A reference resolver.
///
/// Resolves references against the base URI and looks up the result in the registry.
#[derive(Clone)]
pub struct Resolver<'r> {
    pub(crate) registry: &'r Registry<'r>,
    base_uri: Arc<Uri<String>>,
    scopes: List<Uri<String>>,
    entry: Option<&'r crate::registry::index::SchemaEntryContext<'r>>,
    profile: Option<Arc<EntryProfile>>,
}

impl PartialEq for Resolver<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.base_uri == other.base_uri
            && self.profile == other.profile
            && match (self.entry, other.entry) {
                (None, None) => true,
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                _ => false,
            }
    }
}
impl Eq for Resolver<'_> {}

impl fmt::Debug for Resolver<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("base_uri", &self.base_uri.as_str())
            .field("scopes", &{
                let mut buf = String::from("[");
                let mut values = self.scopes.iter();
                if let Some(value) = values.next() {
                    buf.push_str(value.as_str());
                }
                for value in values {
                    buf.push_str(", ");
                    buf.push_str(value.as_str());
                }
                buf.push(']');
                buf
            })
            .finish()
    }
}

impl<'r> Resolver<'r> {
    /// Create a new `Resolver` with the given registry and base URI.
    #[inline]
    pub(crate) fn new(registry: &'r Registry<'r>, base_uri: Arc<Uri<String>>) -> Self {
        Self {
            registry,
            base_uri,
            scopes: List::new(),
            entry: None,
            profile: None,
        }
    }
    /// Returns the current base URI of this resolver.
    #[must_use]
    #[inline]
    pub fn base_uri(&self) -> Arc<Uri<String>> {
        self.base_uri.clone()
    }
    /// Resolve a reference to the resource it points to.
    ///
    /// # Errors
    ///
    /// If the reference cannot be resolved or is invalid.
    pub fn lookup(&self, reference: &str) -> Result<Resolved<'r>, Error> {
        let (uri, fragment) = if let Some(reference) = reference.strip_prefix('#') {
            (self.base_uri.clone(), reference)
        } else {
            let (uri, fragment) = if let Some((uri, fragment)) = reference.rsplit_once('#') {
                (uri, fragment)
            } else {
                (reference, "")
            };
            let uri = self.registry.resolve_uri(&self.base_uri.borrow(), uri)?;
            (uri, fragment)
        };

        let Some(retrieved) = self.registry.resource_by_uri(&uri) else {
            return Err(Error::unretrievable(
                uri.as_str(),
                "Retrieving external resources is not supported once the registry is populated"
                    .into(),
            ));
        };

        if fragment.starts_with('/') {
            let resolver = self.evolve(uri);
            return retrieved.pointer(fragment, resolver);
        }

        if !fragment.is_empty() {
            let retrieved = self.lookup_anchor(&uri, fragment)?;
            let resolver = self.evolve(uri);
            let (contents, resolver, draft) = retrieved.resolve(resolver)?.into_inner();
            return Ok(Resolved::new(
                contents,
                resolver.with_entry(contents),
                draft,
            ));
        }

        let resolver = self.evolve(uri).with_entry(retrieved.contents());
        Ok(Resolved::new(
            retrieved.contents(),
            resolver,
            retrieved.draft(),
        ))
    }
    /// Resolve a recursive reference.
    ///
    /// This method implements the recursive reference resolution algorithm
    /// as specified in JSON Schema Draft 2019-09.
    ///
    /// It starts by resolving "#" and then follows the dynamic scope,
    /// looking for resources with `$recursiveAnchor: true`.
    ///
    /// # Errors
    ///
    /// This method can return any error that [`Resolver::lookup`] can return.
    pub fn lookup_recursive_ref(&self) -> Result<Resolved<'r>, Error> {
        let mut resolved = self.lookup("#")?;

        if let Value::Object(obj) = resolved.contents {
            if obj
                .get("$recursiveAnchor")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                for uri in &self.dynamic_scope() {
                    let next_resolved = self.lookup(uri.as_str())?;

                    match next_resolved.contents {
                        Value::Object(next_obj) => {
                            if !next_obj
                                .get("$recursiveAnchor")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                            {
                                break;
                            }
                        }
                        _ => break,
                    }

                    resolved = next_resolved;
                }
            }
        }

        Ok(resolved)
    }

    #[inline]
    pub(crate) fn lookup_anchor<'a>(
        &self,
        uri: &'a Uri<String>,
        name: &'a str,
    ) -> Result<Anchor<'r>, Error> {
        self.registry.anchor(uri, name)
    }

    /// Create a resolver for a subresource.
    ///
    /// # Errors
    ///
    /// Returns an error if the resource id cannot be resolved against the base URI of this resolver.
    #[inline]
    pub fn in_subresource(&self, subresource: ResourceRef<'_>) -> Result<Self, Error> {
        let mut parent = self.clone();
        if self.entry.is_some() || self.profile.is_some() {
            if let Some(specification) = subresource
                .contents()
                .get("$schema")
                .and_then(Value::as_str)
            {
                let (draft, vocabularies) = self.registry.entry_profile(
                    subresource.draft(),
                    self.base_uri.clone(),
                    Some(specification),
                )?;
                parent.profile = Some(Arc::new((
                    draft,
                    vocabularies,
                    Some(Arc::from(specification)),
                )));
            }
        }
        if let Some(id) = subresource.id() {
            let base_uri = self.registry.resolve_uri(&self.base_uri.borrow(), id)?;
            Ok(Resolver {
                registry: self.registry,
                base_uri,
                scopes: self.scopes.clone(),
                entry: self.entry,
                profile: parent.profile,
            })
        } else {
            Ok(parent)
        }
    }
    #[must_use]
    #[inline]
    pub fn dynamic_scope(&self) -> List<Uri<String>> {
        self.scopes.clone()
    }
    #[inline]
    fn evolve(&self, base_uri: Arc<Uri<String>>) -> Resolver<'r> {
        if !self.base_uri.as_str().is_empty()
            && (self.scopes.is_empty() || base_uri != self.base_uri)
        {
            Resolver {
                registry: self.registry,
                base_uri: base_uri.clone(),
                scopes: self.scopes.push_front(self.base_uri.clone()),
                entry: if base_uri == self.base_uri {
                    self.entry
                } else {
                    None
                },
                profile: if base_uri == self.base_uri {
                    self.profile.clone()
                } else {
                    None
                },
            }
        } else {
            Resolver {
                registry: self.registry,
                base_uri,
                scopes: self.scopes.clone(),
                entry: self.entry,
                profile: self.profile.clone(),
            }
        }
    }
    /// Resolve a reference against a base.
    ///
    /// # Errors
    ///
    /// If the reference is invalid.
    #[inline]
    pub fn resolve_uri(&self, base: &Uri<&str>, uri: &str) -> Result<Arc<Uri<String>>, Error> {
        self.registry.resolve_uri(base, uri)
    }

    /// Effective supported draft of the active captured entry or subschema.
    #[must_use]
    pub fn schema_entry_draft(&self) -> Option<Draft> {
        self.profile.as_ref().map(|profile| profile.0)
    }
    /// Captured dialect identity for opt-in compiler cache separation.
    /// Ordinary resolvers have no entry context and return `None`.
    #[must_use]
    pub fn schema_entry_dialect(&self) -> Option<Arc<str>> {
        self.profile.as_ref().and_then(|profile| profile.2.clone())
    }
    pub(crate) fn with_entry(mut self, contents: &'r Value) -> Self {
        self.entry = self.registry.schema_entry_context(contents);
        self.profile = self.entry.and_then(|entry| {
            self.registry
                .entry_profile(
                    entry.draft,
                    entry.effective_base.clone(),
                    entry.metaschema.as_deref(),
                )
                .ok()
                .map(|(draft, vocabularies)| {
                    Arc::new((draft, vocabularies, entry.metaschema.clone()))
                })
        });
        self
    }
    pub(crate) fn entry_draft(&self, fallback: Draft) -> Draft {
        self.profile.as_ref().map_or(fallback, |profile| profile.0)
    }
    /// Vocabulary of the containing resource root (the resolver's base URI), so a
    /// `$schema`-less subschema inherits it; falls back to `contents`.
    #[must_use]
    pub fn find_vocabularies(&self, draft: Draft, contents: &Value) -> VocabularySet {
        if let Some(profile) = &self.profile {
            return profile.1.clone();
        }
        if let Some(resource) = self.registry.resource_by_uri(self.base_uri.as_ref()) {
            return self.registry.find_vocabularies(draft, resource.contents());
        }
        self.registry.find_vocabularies(draft, contents)
    }
}

/// A reference resolved to its contents by a [`Resolver`].
#[derive(Debug)]
pub struct Resolved<'r> {
    /// The contents of the resolved reference.
    contents: &'r Value,
    /// The resolver that resolved this reference, which can be used for further resolutions.
    resolver: Resolver<'r>,
    draft: Draft,
}

impl<'r> Resolved<'r> {
    pub(crate) fn new(contents: &'r Value, resolver: Resolver<'r>, draft: Draft) -> Self {
        let draft = resolver.entry_draft(draft);
        Self {
            contents,
            resolver,
            draft,
        }
    }
    /// Resolved contents.
    #[must_use]
    pub fn contents(&self) -> &'r Value {
        self.contents
    }
    /// Resolver used to resolve this contents.
    #[must_use]
    pub fn resolver(&self) -> &Resolver<'r> {
        &self.resolver
    }
    #[must_use]
    pub fn into_inner(self) -> (&'r Value, Resolver<'r>, Draft) {
        (self.contents, self.resolver, self.draft)
    }
}
