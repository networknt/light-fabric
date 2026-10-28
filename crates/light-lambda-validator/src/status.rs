use serde_json::Value;
pub struct StatusError {
    pub status_code: u16,
    pub code: String,
    pub message: String,
    pub description: String,
    pub severity: String,
    pub metadata: Option<Vec<(String, Value)>>,
}
impl StatusError {
    pub fn new(status_code: u16, code: &str, message: &str, description: &str) -> Self {
        Self {
            status_code,
            code: code.into(),
            message: message.into(),
            description: description.into(),
            severity: "ERROR".into(),
            metadata: None,
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = format!(
            "{{\"statusCode\":{},\"code\":\"{}\",\"message\":\"{}\",\"description\":{}",
            self.status_code,
            self.code,
            self.message,
            serde_json::to_string(&self.description).expect("string serialization is infallible")
        );
        if let Some(metadata) = &self.metadata {
            s.push_str(",\"metadata\":{");
            for (i, (key, value)) in metadata.iter().enumerate() {
                if i != 0 {
                    s.push(',');
                }
                s.push_str(
                    &serde_json::to_string(key).expect("string serialization is infallible"),
                );
                s.push(':');
                s.push_str(&value.to_string());
            }
            s.push('}');
        }
        s.push_str(&format!(",\"severity\":\"{}\"}}", self.severity));
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn description_escaping_preserves_text_and_field_order() {
        for text in [
            "plain",
            "quotes \" and slash \\",
            "line\nreturn\rtab\t\u{0}\u{1f}",
            "1e+400 / 1.00000000000000000000",
        ] {
            let status = StatusError::new(400, "ERR11004", "VALIDATOR_REQUEST_BODY", text);
            let bytes = status.to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["description"], text);
            let expected = format!(
                "{{\"statusCode\":400,\"code\":\"ERR11004\",\"message\":\"VALIDATOR_REQUEST_BODY\",\"description\":{},\"severity\":\"ERROR\"}}",
                serde_json::to_string(text).unwrap()
            );
            assert_eq!(bytes, expected.as_bytes());
        }
    }
}
