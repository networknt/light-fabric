//! Narrow admission for the native deferred manager, not a general service catalogue.
use crate::init_error::InitError;
use serde_json::Value;
pub(crate) fn reject(value: &Value) -> Result<(), InitError> {
    for singleton in value
        .get("singletons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(managers) = singleton.get("com.networknt.cache.CacheManager") {
            let managers = managers.as_array().ok_or(InitError::InvalidConfiguration(
                "service.yml: invalid CacheManager declaration",
            ))?;
            if managers
                .iter()
                .any(|name| name == "com.networknt.aws.lambda.cache.DynamoDbCacheManager")
            {
                return Err(InitError::InvalidConfiguration(
                    "service.yml: DynamoDbCacheManager unavailable",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_selected_dynamo_is_rejected() {
        for value in [
            Value::Null,
            json!({}),
            json!({"singletons":[]}),
            json!({"singletons":[{"other.Service":["com.networknt.aws.lambda.cache.DynamoDbCacheManager"]}]}),
            json!({"singletons":[{"com.networknt.cache.CacheManager":[]}]}),
            json!({"singletons":[{"com.networknt.cache.CacheManager":["unrelated.Manager"]}]}),
        ] {
            assert!(reject(&value).is_ok());
        }
        for managers in [
            json!(["com.networknt.aws.lambda.cache.DynamoDbCacheManager"]),
            json!([
                "unrelated.Manager",
                "com.networknt.aws.lambda.cache.DynamoDbCacheManager"
            ]),
        ] {
            let error =
                reject(&json!({"singletons":[{"com.networknt.cache.CacheManager":managers}]}))
                    .unwrap_err();
            assert_eq!(
                error.message(),
                "service.yml: DynamoDbCacheManager unavailable"
            );
        }
    }
}
