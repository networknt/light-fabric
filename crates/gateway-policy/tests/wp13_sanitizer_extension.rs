use gateway_policy::sanitizer_encoding::{encode, encode_utf16};
use sha2::{Digest, Sha256};
#[test]
fn json_and_xml11_match_pinned_java_for_all_scalars_context_and_invalid_utf16() {
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("data/wp13-encoder-extension-java.json")).unwrap();
    for (name, expected) in golden.as_object().unwrap() {
        let mut hash = Sha256::new();
        for code in 0..=0x10ffff {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            let output = encode(name, c.encode_utf8(&mut [0; 4])).unwrap();
            hash.update((output.len() as u32).to_be_bytes());
            hash.update(output.as_bytes());
        }
        assert_eq!(
            format!("{:x}", hash.finalize()),
            expected["allUnicodeSha256"].as_str().unwrap(),
            "{name}"
        );
        for (input, output) in expected["sequences"].as_object().unwrap() {
            assert_eq!(
                encode(name, input).as_deref(),
                output.as_str(),
                "{name} contextual input"
            );
        }
        for case in expected["invalidUtf16"].as_array().unwrap() {
            let units = |key: &str| {
                case[key]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n.as_u64().unwrap() as u16)
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                encode_utf16(name, &units("input")).unwrap(),
                units("output"),
                "{name} isolated UTF-16 units"
            );
        }
    }
}
