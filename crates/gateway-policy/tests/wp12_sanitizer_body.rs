use base64::{Engine, engine::general_purpose::STANDARD};
use gateway_policy::sanitizer_body::sanitize;
#[test]
fn actual_java_ordered_body_and_selector_bytes_are_preserved() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("data/wp12-body-java-golden.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let list = |key: &str| {
            case[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let result = sanitize(
            case["input"].as_str().unwrap().as_bytes(),
            case["encoder"].as_str().unwrap(),
            &list("ignore"),
            &list("appreciate"),
        );
        if case["java"].get("error").is_some() {
            assert!(result.is_err(), "{}", case["id"]);
        } else {
            assert_eq!(
                result.unwrap(),
                STANDARD
                    .decode(case["java"]["outputBase64"].as_str().unwrap())
                    .unwrap(),
                "{}",
                case["id"]
            );
        }
    }
}
