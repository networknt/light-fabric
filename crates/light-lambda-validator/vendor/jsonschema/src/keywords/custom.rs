use crate::{
    error::ErrorIterator,
    paths::{LazyLocation, Location, RefTracker},
    validator::{Validate, ValidationContext},
    ValidationError,
};
use serde_json::{Map, Value};

pub(crate) struct CustomKeyword {
    inner: Box<dyn Keyword>,
    location: Location,
    keyword: String,
}

impl CustomKeyword {
    pub(crate) fn new(inner: Box<dyn Keyword>, location: Location, keyword: String) -> Self {
        Self {
            inner,
            location,
            keyword,
        }
    }
}

impl Validate for CustomKeyword {
    fn validate<'i>(
        &self,
        instance: &'i Value,
        instance_path: &LazyLocation,
        _tracker: Option<&RefTracker>,
        _ctx: &mut ValidationContext,
    ) -> Result<(), ValidationError<'i>> {
        self.inner
            .validate(instance)
            .map_err(|err| err.with_context(instance, instance_path, &self.location, &self.keyword))
    }

    fn is_valid(&self, instance: &Value, _ctx: &mut ValidationContext) -> bool {
        self.inner.is_valid(instance)
    }

    fn iter_errors<'i>(
        &self,
        instance: &'i Value,
        instance_path: &LazyLocation,
        _tracker: Option<&RefTracker>,
        _ctx: &mut ValidationContext,
    ) -> ErrorIterator<'i> {
        let errors: Vec<_> = self
            .inner
            .iter_errors(instance)
            .map(|err| err.with_context(instance, instance_path, &self.location, &self.keyword))
            .collect();
        ErrorIterator::from_iterator(errors.into_iter())
    }
}

/// Trait for implementing custom keyword validators.
///
/// Custom keywords extend JSON Schema validation with domain-specific rules.
///
/// # Example
///
/// ```rust
/// use jsonschema::{Keyword, ValidationError};
/// use serde_json::Value;
///
/// struct EvenNumberValidator;
///
/// impl Keyword for EvenNumberValidator {
///     fn validate<'i>(&self, instance: &'i Value) -> Result<(), ValidationError<'i>> {
///         if let Some(n) = instance.as_u64() {
///             if n % 2 != 0 {
///                 return Err(ValidationError::custom("number must be even"));
///             }
///         }
///         Ok(())
///     }
///
///     fn is_valid(&self, instance: &Value) -> bool {
///         instance.as_u64().map_or(true, |n| n % 2 == 0)
///     }
/// }
/// ```
pub trait Keyword: Send + Sync {
    /// Validate an instance against this custom keyword.
    ///
    /// Use [`ValidationError::custom`] for error messages. Path information
    /// (`instance_path` and `schema_path`) is filled in automatically.
    ///
    /// # Errors
    ///
    /// Returns a [`ValidationError`] if the instance is invalid.
    fn validate<'i>(&self, instance: &'i Value) -> Result<(), ValidationError<'i>>;

    /// Check validity without collecting error details.
    fn is_valid(&self, instance: &Value) -> bool;

    /// Validate an instance, yielding every error at once.
    ///
    /// Override this to report multiple problems from a single keyword. The
    /// default yields at most one error, from [`validate`](Keyword::validate).
    fn iter_errors<'i>(
        &self,
        instance: &'i Value,
    ) -> Box<dyn Iterator<Item = ValidationError<'i>> + 'i> {
        Box::new(self.validate(instance).err().into_iter())
    }
}

pub(crate) trait KeywordFactory: Send + Sync {
    fn init<'a>(
        &self,
        parent: &'a Map<String, Value>,
        schema: &'a Value,
        schema_path: Location,
        keyword: &str,
    ) -> Result<Box<dyn Keyword>, ValidationError<'a>>;
}

impl<F> KeywordFactory for F
where
    F: for<'a> Fn(
            &'a Map<String, Value>,
            &'a Value,
            Location,
        ) -> Result<Box<dyn Keyword>, ValidationError<'a>>
        + Send
        + Sync,
{
    fn init<'a>(
        &self,
        parent: &'a Map<String, Value>,
        schema: &'a Value,
        schema_path: Location,
        keyword: &str,
    ) -> Result<Box<dyn Keyword>, ValidationError<'a>> {
        self(parent, schema, schema_path.clone())
            .map_err(|err| err.with_schema_context(schema, schema_path, keyword))
    }
}

/// Effective dialect and active vocabularies at one keyword compilation site.
/// This is immutable compilation metadata, not request state.
pub struct KeywordCompilationContext<'a> {
    pub(crate) draft: referencing::Draft,
    pub(crate) vocabularies: &'a referencing::VocabularySet,
}
impl KeywordCompilationContext<'_> {
    /// Effective dialect for the actual resource being compiled.
    #[must_use]
    pub fn draft(&self) -> referencing::Draft {
        self.draft
    }
    /// Whether the resource's active vocabulary includes this vocabulary.
    #[must_use]
    pub fn has_vocabulary(&self, vocabulary: &referencing::Vocabulary) -> bool {
        self.vocabularies.contains(vocabulary)
    }
}
pub(crate) trait ContextualKeywordFactory: Send + Sync {
    fn init(
        &self,
        context: &KeywordCompilationContext<'_>,
        parent: &Map<String, Value>,
        schema: &Value,
        path: Location,
    ) -> Result<Option<Box<dyn Keyword>>, ValidationError<'static>>;
}
impl<F> ContextualKeywordFactory for F
where
    F: Fn(
            &KeywordCompilationContext<'_>,
            &Map<String, Value>,
            &Value,
            Location,
        ) -> Result<Option<Box<dyn Keyword>>, ValidationError<'static>>
        + Send
        + Sync,
{
    fn init(
        &self,
        context: &KeywordCompilationContext<'_>,
        parent: &Map<String, Value>,
        schema: &Value,
        path: Location,
    ) -> Result<Option<Box<dyn Keyword>>, ValidationError<'static>> {
        self(context, parent, schema, path)
    }
}
#[derive(Clone)]
pub(crate) enum RegisteredKeywordFactory {
    Legacy(std::sync::Arc<dyn KeywordFactory>),
    Contextual(std::sync::Arc<dyn ContextualKeywordFactory>),
}
