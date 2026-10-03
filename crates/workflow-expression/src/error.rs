use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Invalid,
    Unsupported,
    Limit,
    Evaluation,
    JsonProfile,
    ResultType,
    ProfileUnsupported,
}
impl Category {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "EXPRESSION_INVALID",
            Self::Unsupported => "EXPRESSION_UNSUPPORTED",
            Self::Limit => "EXPRESSION_LIMIT",
            Self::Evaluation => "EXPRESSION_EVALUATION",
            Self::JsonProfile => "EXPRESSION_JSON_PROFILE",
            Self::ResultType => "EXPRESSION_RESULT_TYPE",
            Self::ProfileUnsupported => "EVALUATOR_PROFILE_UNSUPPORTED",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Admission,
    Runtime,
}
/// Contains coordinates only: never source, input, results, or CEL diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpressionError {
    pub category: Category,
    pub phase: Phase,
    pub span_index: Option<usize>,
    /// UTF-8 byte offset in the authored template, when available.
    pub offset: Option<usize>,
}
impl ExpressionError {
    pub(crate) fn new(category: Category, phase: Phase) -> Self {
        Self {
            category,
            phase,
            span_index: None,
            offset: None,
        }
    }
    pub(crate) fn at(mut self, index: usize, offset: usize) -> Self {
        self.span_index = Some(index);
        self.offset = Some(offset);
        self
    }
}
impl fmt::Display for ExpressionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({:?}, span {:?}, offset {:?})",
            self.category.code(),
            self.phase,
            self.span_index,
            self.offset
        )
    }
}
impl std::error::Error for ExpressionError {}
pub type ProfileError = ExpressionError;
pub type ScanError = ExpressionError;
pub(crate) fn admission(c: Category) -> ExpressionError {
    ExpressionError::new(c, Phase::Admission)
}
pub(crate) fn runtime(c: Category) -> ExpressionError {
    ExpressionError::new(c, Phase::Runtime)
}
