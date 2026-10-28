use std::{
    fmt,
    sync::{Arc, LazyLock},
};

use ahash::AHashMap;
use fluent_uri::Uri;
use serde_json::Value;

use crate::{
    cache::{SharedUriCache, UriCache},
    meta, uri,
    vocabularies::{self, VocabularySet},
    Anchor, DefaultRetriever, Draft, Error, Resolver, ResourceRef, Retrieve,
};

mod build;
use build::{KnownResources, ResourceStore, StoredResource};

pub(crate) mod index;
use index::{Index, IndexedAnchor, IndexedResource};

mod input;
#[cfg(feature = "retrieve-async")]
pub(crate) use input::IntoAsyncRetriever;
pub use input::IntoRegistryResource;
pub(crate) use input::{IntoRetriever, PendingResource};

/// Pre-loaded registry containing all JSON Schema meta-schemas and their vocabularies
pub static SPECIFICATIONS: LazyLock<Registry<'static>> =
    LazyLock::new(|| Registry::from_meta_schemas(meta::META_SCHEMAS_ALL.as_slice()));

#[derive(Clone)]
pub struct RegistryBuilder<'a> {
    baseline: Option<&'a Registry<'a>>,
    pending: AHashMap<Uri<String>, PendingResource<'a>>,
    schema_entries: Vec<build::PendingSchemaEntry>,
    entry_bytes: usize,
    pending_identity_conflict: bool,
    retriever: Arc<dyn Retrieve>,
    #[cfg(feature = "retrieve-async")]
    async_retriever: Option<Arc<dyn crate::AsyncRetrieve>>,
    draft: Option<Draft>,
}

impl fmt::Debug for RegistryBuilder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryBuilder")
            .field("has_baseline", &self.baseline.is_some())
            .field("pending_len", &self.pending.len())
            .field("draft", &self.draft)
            .finish()
    }
}

/// A registry of JSON Schema resources, each identified by their canonical URIs.
///
/// `Registry` is a prepared registry: add resources with [`Registry::new`] and
/// [`RegistryBuilder::add`], then call [`RegistryBuilder::prepare`] to build the
/// reusable registry. To resolve `$ref` references directly, create a [`Resolver`]
/// from the prepared registry:
///
/// ```rust
/// use referencing::Registry;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let schema = serde_json::json!({
///     "$schema": "https://json-schema.org/draft/2020-12/schema",
///     "$id": "https://example.com/root",
///     "$defs": { "item": { "type": "string" } },
///     "items": { "$ref": "#/$defs/item" }
/// });
///
/// let registry = Registry::new()
///     .add("https://example.com/root", schema)?
///     .prepare()?;
///
/// let resolver = registry.resolver(referencing::uri::from_str("https://example.com/root")?);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Registry<'a> {
    baseline: Option<&'a Registry<'a>>,
    resolution_cache: SharedUriCache,
    known_resources: KnownResources,
    index: Index<'a>,
}

/// Borrowed opt-in schema-entry metadata. Values remain owned by the registry.
/// Construction is private: caller-controlled equal values cannot assert identity.
pub struct SchemaEntryBoundaries<'r, 'a> {
    registry: &'r Registry<'a>,
}

/// Actual captured schema context, borrowing its containing resource owner.
pub struct SchemaEntryBoundary<'r> {
    registry: &'r Registry<'r>,
    context: &'r index::SchemaEntryContext<'r>,
}
impl SchemaEntryBoundary<'_> {
    #[must_use]
    /// # Panics
    /// Panics only if private registered owner/pointer invariants are broken.
    pub fn contents(&self) -> &Value {
        self.context
            .owner
            .contents()
            .pointer(&self.context.pointer)
            .expect("registered schema entry retains its owner")
    }
    #[must_use]
    pub fn pointer(&self) -> &str {
        &self.context.pointer
    }
    #[must_use]
    pub fn base_uri(&self) -> &str {
        self.context.effective_base.as_str()
    }
    #[must_use]
    pub fn draft(&self) -> Draft {
        self.context.draft
    }
    #[must_use]
    pub fn metaschema(&self) -> Option<&str> {
        self.context.metaschema.as_deref()
    }
    /// # Errors
    /// Returns an error for an uncaptured or unsupported required metaschema.
    pub fn prepared_draft(&self) -> Result<Draft, Error> {
        self.registry
            .entry_profile(
                self.context.draft,
                self.context.effective_base.clone(),
                self.metaschema(),
            )
            .map(|(draft, _)| draft)
    }
    /// # Errors
    /// Returns an error if the captured metaschema URI cannot resolve.
    pub fn metaschema_uri(&self) -> Result<Option<Arc<Uri<String>>>, Error> {
        self.metaschema()
            .map(|meta| {
                self.registry
                    .resolve_uri(&self.context.effective_base.borrow(), meta)
            })
            .transpose()
    }
    #[must_use]
    pub fn is_resource_root(&self) -> bool {
        self.context.draft.id_of(self.contents()).is_some()
    }
}
impl<'r, 'a: 'r> SchemaEntryBoundaries<'r, 'a> {
    /// Look up a live captured value by identity, never by equality.
    #[must_use]
    pub fn entry_for(&self, contents: &Value) -> Option<SchemaEntryBoundary<'_>> {
        self.registry
            .schema_entry_context(contents)
            .map(|context| SchemaEntryBoundary {
                registry: self.registry,
                context,
            })
    }
    pub fn entries(&self) -> impl Iterator<Item = SchemaEntryBoundary<'_>> {
        std::iter::successors(Some(self.registry), |registry| registry.baseline)
            .flat_map(|registry| registry.index.schema_entries.values())
            .map(|context| SchemaEntryBoundary {
                registry: self.registry,
                context,
            })
    }
}

impl<'a> RegistryBuilder<'a> {
    fn new() -> Self {
        Self {
            baseline: None,
            pending: AHashMap::new(),
            schema_entries: Vec::new(),
            entry_bytes: 0,
            pending_identity_conflict: false,
            retriever: Arc::new(DefaultRetriever),
            #[cfg(feature = "retrieve-async")]
            async_retriever: None,
            draft: None,
        }
    }

    fn from_registry(registry: &'a Registry<'a>) -> Self {
        Self {
            baseline: Some(registry),
            pending: AHashMap::new(),
            schema_entries: Vec::new(),
            entry_bytes: 0,
            pending_identity_conflict: false,
            retriever: Arc::new(DefaultRetriever),
            #[cfg(feature = "retrieve-async")]
            async_retriever: None,
            draft: None,
        }
    }

    #[must_use]
    pub fn draft(mut self, draft: Draft) -> Self {
        self.draft = Some(draft);
        self
    }

    #[must_use]
    pub fn retriever(mut self, retriever: impl IntoRetriever) -> Self {
        self.retriever = retriever.into_retriever();
        self
    }

    #[cfg(feature = "retrieve-async")]
    #[must_use]
    pub fn async_retriever(mut self, retriever: impl IntoAsyncRetriever) -> Self {
        self.async_retriever = Some(retriever.into_retriever());
        self
    }

    /// Add a resource to the registry builder.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid.
    /// # Panics
    /// Panics only if sealed resource insertion violates its private invariant.
    pub fn add<'b>(
        self,
        uri: impl AsRef<str>,
        resource: impl IntoRegistryResource<'b>,
    ) -> Result<RegistryBuilder<'b>, Error>
    where
        'a: 'b,
    {
        fn view<'b>(pending: &'b PendingResource<'_>) -> (&'b Value, Draft) {
            match pending {
                PendingResource::Value(v) => (v, Draft::default().detect(v)),
                PendingResource::ValueRef(v) => (v, Draft::default().detect(v)),
                PendingResource::SharedValue(v) => (v, Draft::default().detect(v)),
                PendingResource::Resource(v) => (v.contents(), v.draft()),
                PendingResource::ResourceRef(v) => (v.contents(), v.draft()),
            }
        }
        let parsed = uri::from_str(uri.as_ref().trim_end_matches('#'))?;
        let mut pending: AHashMap<Uri<String>, PendingResource<'b>> =
            self.pending.into_iter().collect();
        let previous = pending.remove_entry(&parsed);
        input::private::Sealed::insert_into(resource, &mut pending, parsed);
        let conflict = previous.is_some_and(|(key, previous)| {
            let (a, ad) = view(&previous);
            let (b, bd) = view(pending.get(&key).expect("inserted resource"));
            ad != bd || !Index::equivalent(a, b)
        });
        Ok(RegistryBuilder {
            baseline: self.baseline,
            pending,
            schema_entries: self.schema_entries,
            entry_bytes: self.entry_bytes,
            pending_identity_conflict: self.pending_identity_conflict || conflict,
            retriever: self.retriever,
            #[cfg(feature = "retrieve-async")]
            async_retriever: self.async_retriever,
            draft: self.draft,
        })
    }

    /// Experimental opt-in `OpenAPI` schema entry; default resource traversal is unchanged.
    /// Budgets are prototype values, not production admission.
    /// # Errors
    /// Returns an error for invalid pointers or exceeded experimental budgets.
    /// # Panics
    /// Panics only if the checked private budget invariant is broken.
    pub fn add_schema_entry(mut self, document_uri: &str, pointer: &str) -> Result<Self, Error> {
        const BYTES: usize = 4 << 20;
        const ENTRIES: usize = 65536;
        let bytes = document_uri
            .len()
            .checked_add(pointer.len())
            .and_then(|n| self.entry_bytes.checked_add(n));
        if self.schema_entries.len() >= ENTRIES || bytes.is_none_or(|n| n > BYTES) {
            return Err(Error::unretrievable(
                "schema-entry",
                "experimental entry budget".into(),
            ));
        }
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(Error::unretrievable(
                "schema-entry",
                "JSON pointer required".into(),
            ));
        }
        // Check before URI parsing or pointer ownership allocation.
        let document_uri = uri::from_str(document_uri)?;
        self.schema_entries.push(build::PendingSchemaEntry {
            document_uri,
            pointer: pointer.to_owned(),
        });
        self.entry_bytes = bytes.unwrap();
        Ok(self)
    }

    /// Add multiple resources to the registry builder.
    ///
    /// # Errors
    ///
    /// Returns an error if any URI is invalid.
    pub fn extend<'b, I, U, T>(self, pairs: I) -> Result<RegistryBuilder<'b>, Error>
    where
        'a: 'b,
        I: IntoIterator<Item = (U, T)>,
        U: AsRef<str>,
        T: IntoRegistryResource<'b>,
    {
        let mut builder = RegistryBuilder {
            baseline: self.baseline,
            pending: self.pending.into_iter().collect(),
            schema_entries: self.schema_entries,
            entry_bytes: self.entry_bytes,
            pending_identity_conflict: self.pending_identity_conflict,
            retriever: self.retriever,
            #[cfg(feature = "retrieve-async")]
            async_retriever: self.async_retriever,
            draft: self.draft,
        };
        for (uri, resource) in pairs {
            builder = builder.add(uri, resource)?;
        }
        Ok(builder)
    }

    /// Prepare the registry for reuse.
    ///
    /// # Errors
    ///
    /// Returns an error if URI processing, retrieval, or custom meta-schema validation fails.
    pub fn prepare(self) -> Result<Registry<'a>, Error> {
        if !self.schema_entries.is_empty() && self.pending_identity_conflict {
            return Err(Error::unretrievable(
                "schema-entry",
                "validator: conflicting captured schema identity".into(),
            ));
        }
        // When extending an existing registry, seed known resources from the baseline so the
        // retriever skips URIs already owned by the parent.
        let mut known_resources = self
            .baseline
            .map(|b| b.known_resources.clone())
            .unwrap_or_default();
        let mut documents = ResourceStore::new();
        let mut resolution_cache = UriCache::new();
        let (custom_metaschemas, mut index_data) = build::index_resources(
            self.pending,
            self.schema_entries,
            &*self.retriever,
            &mut documents,
            &mut known_resources,
            &mut resolution_cache,
            self.draft,
        )?;
        if let Some(baseline) = self.baseline {
            index_data.check_baseline(baseline);
        }
        index_data.validate_identities()?;
        build::validate_custom_metaschemas(&custom_metaschemas, &known_resources)?;
        Ok(Registry {
            baseline: self.baseline,
            resolution_cache: resolution_cache.into_shared(),
            known_resources,
            index: index_data,
        })
    }

    #[cfg(feature = "retrieve-async")]
    /// Prepare the registry for reuse with async retrieval.
    ///
    /// # Errors
    ///
    /// Returns an error if URI processing, retrieval, or custom meta-schema validation fails.
    pub async fn async_prepare(self) -> Result<Registry<'a>, Error> {
        if !self.schema_entries.is_empty() && self.pending_identity_conflict {
            return Err(Error::unretrievable(
                "schema-entry",
                "validator: conflicting captured schema identity".into(),
            ));
        }
        let retriever = self
            .async_retriever
            .unwrap_or_else(|| Arc::new(DefaultRetriever));
        let mut known_resources = self
            .baseline
            .map(|b| b.known_resources.clone())
            .unwrap_or_default();
        let mut documents = ResourceStore::new();
        let mut resolution_cache = UriCache::new();
        let (custom_metaschemas, mut index_data) = build::index_resources_async(
            self.pending,
            self.schema_entries,
            &*retriever,
            &mut documents,
            &mut known_resources,
            &mut resolution_cache,
            self.draft,
        )
        .await?;
        if let Some(baseline) = self.baseline {
            index_data.check_baseline(baseline);
        }
        index_data.validate_identities()?;
        build::validate_custom_metaschemas(&custom_metaschemas, &known_resources)?;
        Ok(Registry {
            baseline: self.baseline,
            resolution_cache: resolution_cache.into_shared(),
            known_resources,
            index: index_data,
        })
    }
}

impl<'a> Registry<'a> {
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new<'b>() -> RegistryBuilder<'b> {
        RegistryBuilder::new()
    }
    /// Add a resource to a prepared registry, returning a builder that must be prepared again.
    ///
    /// # Errors
    ///
    /// Returns an error if the URI is invalid.
    pub fn add<'b>(
        &'b self,
        uri: impl AsRef<str>,
        resource: impl IntoRegistryResource<'b>,
    ) -> Result<RegistryBuilder<'b>, Error>
    where
        'a: 'b,
    {
        RegistryBuilder::from_registry(self).add(uri, resource)
    }

    /// Add multiple resources to a prepared registry, returning a builder that
    /// must be prepared again.
    ///
    /// # Errors
    ///
    /// Returns an error if any URI is invalid.
    pub fn extend<'b, I, U, T>(&'b self, pairs: I) -> Result<RegistryBuilder<'b>, Error>
    where
        'a: 'b,
        I: IntoIterator<Item = (U, T)>,
        U: AsRef<str>,
        T: IntoRegistryResource<'b>,
    {
        RegistryBuilder::from_registry(self).extend(pairs)
    }

    /// Build a registry with all the given meta-schemas from specs.
    pub(crate) fn from_meta_schemas(schemas: &[(&'static str, &'static Value)]) -> Self {
        let mut documents = ResourceStore::with_capacity(schemas.len());
        let mut known_resources = KnownResources::with_capacity(schemas.len());

        for (uri, schema) in schemas {
            let parsed =
                uri::from_str(uri.trim_end_matches('#')).expect("meta-schema URI must be valid");
            let key = Arc::new(parsed);
            let draft = Draft::default().detect(schema);
            known_resources.insert((*key).clone());
            documents.insert(key, Arc::new(StoredResource::borrowed(schema, draft)));
        }

        let mut resolution_cache = UriCache::with_capacity(35);
        let index_data = build::build_index_from_stored(&documents, &mut resolution_cache)
            .expect("meta-schema index data must build");

        Self {
            baseline: None,
            resolution_cache: resolution_cache.into_shared(),
            known_resources,
            index: index_data,
        }
    }
    /// Borrow actual opt-in captured contexts without copying schema trees.
    #[must_use]
    pub fn schema_entry_boundaries(&self) -> SchemaEntryBoundaries<'_, 'a> {
        SchemaEntryBoundaries { registry: self }
    }

    pub(crate) fn schema_entry_context(
        &self,
        contents: &Value,
    ) -> Option<&index::SchemaEntryContext<'a>> {
        let key = std::ptr::from_ref(contents) as usize;
        if let Some(context) = self.index.schema_entries.get(&key) {
            // Check stored pointer against its owner without dereferencing keys.
            debug_assert!(context
                .owner
                .contents()
                .pointer(&context.pointer)
                .is_some_and(|v| std::ptr::eq(v, contents)));
            Some(context)
        } else {
            self.baseline
                .and_then(|baseline| baseline.schema_entry_context(contents))
        }
    }

    // Opt-in entry profile. Only actual captured/pinned metaschemas participate.
    pub(crate) fn entry_profile(
        &self,
        mut draft: Draft,
        mut base: Arc<Uri<String>>,
        specification: Option<&str>,
    ) -> Result<(Draft, VocabularySet), Error> {
        let mut next = specification.map(str::to_owned);
        let mut seen = std::collections::BTreeSet::new();
        let mut vocabularies = None;
        while let Some(specification) = next {
            let mut uri = (*self.resolve_uri(&base.borrow(), &specification)?).clone();
            uri.set_fragment(None);
            if !seen.insert(uri.as_str().to_owned()) {
                break;
            }
            let detected = Draft::from_schema_uri(uri.as_str());
            if detected != Draft::Unknown {
                draft = detected;
            }
            let resource = self.resource_by_uri(&uri).ok_or_else(|| {
                Error::unretrievable("schema-entry", "uncaptured metaschema".into())
            })?;
            if vocabularies.is_none() {
                if let Some(object) = resource
                    .contents()
                    .get("$vocabulary")
                    .and_then(Value::as_object)
                {
                    let mut active = VocabularySet::default();
                    for (name, required) in object {
                        let vocabulary: crate::Vocabulary = name.parse()?;
                        if matches!(vocabulary, crate::Vocabulary::Custom(_)) {
                            if required.as_bool() == Some(true) {
                                return Err(Error::unretrievable(
                                    "schema-entry",
                                    "validator: unsupported required vocabulary".into(),
                                ));
                            }
                        } else {
                            active.add(vocabulary);
                        }
                    }
                    vocabularies = Some(active);
                }
            }
            next = resource
                .contents()
                .get("$schema")
                .and_then(Value::as_str)
                .map(str::to_owned);
            base = Arc::new(uri);
        }
        Ok((
            draft,
            vocabularies.unwrap_or_else(|| draft.default_vocabularies()),
        ))
    }
    /// Validate all opt-in schema-entry metaschema contexts before readiness.
    /// Ordinary registries have no schema entries and retain their default behavior.
    /// # Errors
    /// Returns an error for conflicting identities or invalid captured profiles.
    pub fn validate_schema_entry_contexts(&self) -> Result<(), Error> {
        for context in self.index.schema_entries.values() {
            self.entry_profile(
                context.draft,
                Arc::clone(&context.effective_base),
                context.metaschema.as_deref(),
            )?;
        }
        Ok(())
    }

    /// Returns `true` if the registry contains a resource at the given URI.
    ///
    /// Returns `false` if the URI is malformed.
    #[must_use]
    pub fn contains_resource(&self, uri: &str) -> bool {
        let Ok(uri) = uri::from_str(uri) else {
            return false;
        };
        self.resource_by_uri(&uri).is_some()
    }

    /// Creates a [`Resolver`] rooted at `base_uri`.
    ///
    /// The returned resolver borrows from this registry and cannot outlive it.
    #[must_use]
    pub fn resolver(&self, base_uri: Uri<String>) -> Resolver<'_> {
        Resolver::new(self, Arc::new(base_uri))
    }

    /// Returns the vocabulary set active for a schema with the given `contents`.
    ///
    /// Detects the draft from the `$schema` field. If no draft is detected or
    /// the draft has no registered vocabularies, returns the default vocabulary
    /// set — never errors.
    #[must_use]
    pub fn find_vocabularies(&self, draft: Draft, contents: &Value) -> VocabularySet {
        match draft.detect(contents) {
            Draft::Unknown => {
                if let Some(specification) = contents
                    .as_object()
                    .and_then(|obj| obj.get("$schema"))
                    .and_then(|s| s.as_str())
                {
                    if let Ok(mut uri) = uri::from_str(specification) {
                        uri.set_fragment(None);
                        if let Some(resource) = self.resource_by_uri(&uri) {
                            if let Ok(Some(vocabularies)) = vocabularies::find(resource.contents())
                            {
                                return vocabularies;
                            }
                        }
                    }
                }
                Draft::Unknown.default_vocabularies()
            }
            draft => draft.default_vocabularies(),
        }
    }

    /// Resolves `uri` against `base` and returns the resulting absolute URI.
    ///
    /// Results are cached. Returns an error if `base` has no scheme or if
    /// resolution fails.
    ///
    /// # Errors
    ///
    /// Returns an error if base has no schema or there is a fragment.
    pub fn resolve_uri(&self, base: &Uri<&str>, uri: &str) -> Result<Arc<Uri<String>>, Error> {
        self.resolution_cache.resolve_against(base, uri)
    }

    #[inline]
    pub(crate) fn resource_by_uri(&self, uri: &Uri<String>) -> Option<ResourceRef<'_>> {
        self.index
            .resources
            .get(uri)
            .and_then(IndexedResource::resolve)
            .or_else(|| {
                self.baseline
                    .and_then(|baseline| baseline.resource_by_uri(uri))
            })
    }

    pub(crate) fn anchor(&self, uri: &Uri<String>, name: &str) -> Result<Anchor<'_>, Error> {
        if let Some(anchor) = self.anchor_exact(uri, name) {
            return Ok(anchor);
        }

        if let Some(resource) = self.resource_by_uri(uri) {
            if let Some(id) = resource.id() {
                let canonical = uri::from_str(id)?;
                if let Some(anchor) = self.anchor_exact(&canonical, name) {
                    return Ok(anchor);
                }
            }
        }

        if name.contains('/') {
            Err(Error::invalid_anchor(name.to_string()))
        } else {
            Err(Error::no_such_anchor(name.to_string()))
        }
    }

    fn local_anchor_by_uri(&self, uri: &Uri<String>, name: &str) -> Option<Anchor<'_>> {
        self.index
            .anchors
            .get(uri)
            .and_then(|entries| entries.get(name))
            .and_then(IndexedAnchor::resolve)
    }

    fn anchor_exact(&self, uri: &Uri<String>, name: &str) -> Option<Anchor<'_>> {
        self.local_anchor_by_uri(uri, name).or_else(|| {
            self.baseline
                .and_then(|baseline| baseline.anchor_exact(uri, name))
        })
    }
}
