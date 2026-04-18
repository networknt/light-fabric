const IMAGE_MARKER_PREFIX: &str = "[IMAGE:";

pub fn parse_image_markers(content: &str) -> (String, Vec<String>) {
    let mut refs = Vec::new();
    let mut cleaned = String::with_capacity(content.len());
    let mut cursor = 0usize;

    while let Some(rel_start) = content[cursor..].find(IMAGE_MARKER_PREFIX) {
        let start = cursor + rel_start;
        cleaned.push_str(&content[cursor..start]);

        let marker_start = start + IMAGE_MARKER_PREFIX.len();
        let Some(rel_end) = content[marker_start..].find(']') else {
            cleaned.push_str(&content[start..]);
            cursor = content.len();
            break;
        };

        let end = marker_start + rel_end;
        let candidate = content[marker_start..end].trim();

        if candidate.is_empty() {
            cleaned.push_str(&content[start..=end]);
        } else {
            refs.push(candidate.to_string());
        }

        cursor = end + 1;
    }

    if cursor < content.len() {
        cleaned.push_str(&content[cursor..]);
    }

    (cleaned.trim().to_string(), refs)
}

pub fn extract_ollama_image_payload(image_ref: &str) -> Option<String> {
    if image_ref.starts_with("data:") {
        let comma_idx = image_ref.find(',')?;
        let (_, payload) = image_ref.split_at(comma_idx + 1);
        let payload = payload.trim();
        if payload.is_empty() {
            None
        } else {
            Some(payload.to_string())
        }
    } else {
        Some(image_ref.trim().to_string()).filter(|value| !value.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_ollama_image_payload, parse_image_markers};

    #[test]
    fn parses_multiple_markers() {
        let input = "before [IMAGE: first.png] middle [IMAGE:data:image/png;base64,Zm9v] after";
        let (cleaned, refs) = parse_image_markers(input);
        assert_eq!(cleaned, "before  middle  after");
        assert_eq!(refs, vec!["first.png", "data:image/png;base64,Zm9v"]);
    }

    #[test]
    fn keeps_empty_marker_in_text() {
        let input = "hello [IMAGE:   ] world";
        let (cleaned, refs) = parse_image_markers(input);
        assert_eq!(cleaned, "hello [IMAGE:   ] world");
        assert!(refs.is_empty());
    }

    #[test]
    fn keeps_unclosed_marker_in_text() {
        let input = "hello [IMAGE:missing";
        let (cleaned, refs) = parse_image_markers(input);
        assert_eq!(cleaned, "hello [IMAGE:missing");
        assert!(refs.is_empty());
    }

    #[test]
    fn preserves_non_ascii_text() {
        let input = "你好 [IMAGE: 图像.png] мир";
        let (cleaned, refs) = parse_image_markers(input);
        assert_eq!(cleaned, "你好  мир");
        assert_eq!(refs, vec!["图像.png"]);
    }

    #[test]
    fn extracts_data_url_payload() {
        let payload = extract_ollama_image_payload("data:image/png;base64, Zm9vYmFy ");
        assert_eq!(payload.as_deref(), Some("Zm9vYmFy"));
    }
}
