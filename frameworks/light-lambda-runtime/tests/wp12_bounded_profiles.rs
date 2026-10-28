#[path = "profile_support/response_pipeline.rs"]
mod response_pipeline;
#[path = "profile_support/transformer.rs"]
mod transformer;
#[path = "profile_support/transformer_pipeline.rs"]
mod transformer_pipeline;
#[path = "profile_support/validator.rs"]
mod validator;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant as WallInstant},
};
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
static WORKER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn transformer_structure_intermediates_and_repeated_phase_aggregate_are_bounded() {
    let _worker_tests = WORKER_TEST_LOCK.lock().await;
    let raw = json!({"endpointRules":{"/x@post":{"req-tra":["r"]}},"ruleBodies":{"r":{"ruleId":"r","ruleType":"req-tra","common":"Y","ruleName":"owned","actions":[{"actionId":"a","actionRef":"request-body-literal","actionValues":{"body":"x".repeat(4096-11)}}],"conditionLanguage":"cel","conditionSecurityProfile":"standard","expression":"true"}}});
    let candidate = Arc::new(transformer::Candidate::compile(&raw).unwrap());
    assert!(
        transformer::aggregate(&vec![(&*candidate, "req-tra"); transformer::OCCURRENCES]).is_ok()
    );
    assert!(
        transformer::aggregate(&vec![
            (&*candidate, "req-tra");
            transformer::OCCURRENCES + 1
        ])
        .is_err()
    );
    let mut repeated = raw.clone();
    repeated["endpointRules"]["/x@post"]["req-tra"] = json!(["r", "r"]);
    let repeated = transformer::Candidate::compile(&repeated).unwrap();
    assert!(transformer::aggregate(&vec![(&repeated, "req-tra"); 17]).is_err());
    let mut phases = vec![(&*candidate, "req-tra"); 16];
    phases.extend(vec![(&*candidate, "res-tra"); 16]);
    assert!(transformer::aggregate(&phases).is_ok());
    let mut four = raw.clone();
    four["ruleBodies"]["r"]["actions"] =
        json!(vec![raw["ruleBodies"]["r"]["actions"][0].clone(); 4]);
    let four = transformer::Candidate::compile(&four).unwrap();
    assert!(
        transformer::aggregate(&vec![(&four, "req-tra"); 32]).is_ok(),
        "128 actions across all occurrences"
    );
    let mut large = raw.clone();
    large["ruleBodies"]["r"]["ruleDesc"] = json!("x".repeat(40000));
    let large_a = transformer::Candidate::compile(&large).unwrap();
    let large_b = transformer::Candidate::compile(&large).unwrap();
    assert!(
        transformer::aggregate(&[(&large_a, "req-tra"), (&large_b, "res-tra")]).is_err(),
        "aggregate compile bytes"
    );
    let mut exact = raw.clone();
    exact["ruleBodies"]["r"]["ruleDesc"] = json!("");
    let fill = transformer::FILE_BYTES - exact.to_string().len();
    exact["ruleBodies"]["r"]["ruleDesc"] = json!("x".repeat(fill));
    assert!(transformer::Candidate::compile(&exact).is_ok());
    exact["ruleBodies"]["r"]["ruleDesc"] = json!("x".repeat(fill + 1));
    assert!(transformer::Candidate::compile(&exact).is_err());
    let nodes = json!({"v":[vec![0;256],vec![0;256],vec![0;256],vec![0;250]]});
    assert!(transformer::context(&nodes).is_ok());
    let nodes = json!({"v":[vec![0;256],vec![0;256],vec![0;256],vec![0;251]]});
    assert!(transformer::context(&nodes).is_err());
    let members = (0..64)
        .map(|i| (format!("k{i}"), json!(0)))
        .collect::<serde_json::Map<_, _>>();
    assert!(transformer::context(&Value::Object(members.clone())).is_ok());
    let mut members = members;
    members.insert("extra".into(), json!(0));
    assert!(transformer::context(&Value::Object(members)).is_err());
    let mut key = serde_json::Map::new();
    key.insert("k".repeat(256), json!(0));
    assert!(transformer::context(&Value::Object(key)).is_ok());
    let mut key = serde_json::Map::new();
    key.insert("k".repeat(257), json!(0));
    assert!(transformer::context(&Value::Object(key)).is_err());
    let mut deep = json!("leaf");
    for _ in 0..transformer::DEPTH - 1 {
        deep = json!([deep]);
    }
    assert!(transformer::context(&json!({"v":deep.clone()})).is_ok());
    deep = json!([deep]);
    assert!(transformer::context(&json!({"v":deep})).is_err());
    assert!(transformer::context(&json!({"v":vec![0;257]})).is_err());
    assert!(transformer::context(&json!({"v":vec![0;256]})).is_ok());
    let boundary = json!({"v":"x".repeat(transformer::CONTEXT_BYTES-8)});
    assert!(transformer::context(&boundary).is_ok());
    assert!(transformer::context(&json!({"v":"x".repeat(transformer::CONTEXT_BYTES-7)})).is_err());
    let c = candidate.clone();
    assert!(
        transformer::worker(
            boundary,
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
            move |input| c.execute(input, "/x@post", "req-tra")
        )
        .await
        .is_err()
    );
}

#[test]
fn schema_candidate_java_component_parity_and_explicit_dialect_differences() {
    let cases: Value = serde_json::from_str(include_str!("data/wp12-schema-java.json")).unwrap();
    let mut rows = Vec::new();
    let mut same = 0;
    let mut differences = 0;
    let mut excluded = 0;
    for case in cases.as_array().unwrap() {
        match validator::Candidate::compile(&case["schema"], &json!({}), case["oas30"] == true) {
            Ok(candidate) => {
                let actual = candidate.validate(&case["value"]).unwrap();
                let java = case["java"]["valid"].as_bool().unwrap();
                if actual == java {
                    same += 1;
                } else {
                    differences += 1;
                }
                rows.push(
                    json!({"id":case["id"],"java":java,"candidate":actual,"same":actual==java}),
                );
                assert_eq!(candidate.retrievals.load(Ordering::SeqCst), 0);
            }
            Err(_) => {
                excluded += 1;
                rows.push(json!({"id":case["id"],"excluded":true}));
            }
        }
    }
    assert!(same > 30);
    assert_eq!(
        same + differences + excluded,
        cases.as_array().unwrap().len()
    );
    println!(
        "SCHEMA_PARITY {}",
        json!({"same":same,"differences":differences,"excluded":excluded,"rows":rows})
    );
    for oas30 in [true, false] {
        let schema = if oas30 {
            json!({"type":"integer","minimum":5,"exclusiveMinimum":true})
        } else {
            json!({"type":"integer","exclusiveMinimum":5})
        };
        let candidate = validator::Candidate::compile(&schema, &json!({}), oas30).unwrap();
        assert!(!candidate.validate(&json!(5)).unwrap());
        assert!(candidate.validate(&json!(6)).unwrap());
    }
}
#[test]
fn schema_candidate_denies_retrieval_cycles_unsupported_features_and_enforces_all_borders() {
    for reference in [
        "https://never-contact.invalid/schema",
        "file:///never-open/schema",
        "other.json#/x",
        "#/missing",
    ] {
        assert!(
            validator::Candidate::compile(&json!({"$ref":reference}), &json!({}), false).is_err()
        );
    }
    let resources = json!({"components":{"schemas":{"a":{"$ref":"#/components/schemas/a"},"b":{"type":"string"}}}});
    assert!(
        validator::Candidate::compile(&json!({"$ref":"#/components/schemas/a"}), &resources, false)
            .is_err()
    );
    let local =
        validator::Candidate::compile(&json!({"$ref":"#/components/schemas/b"}), &resources, false)
            .unwrap();
    assert!(local.validate(&json!("x")).unwrap());
    assert_eq!(local.retrievals.load(Ordering::SeqCst), 0);
    for schema in [
        json!({"allOf":[{}]}),
        json!({"uniqueItems":true}),
        json!({"type":"number"}),
        json!({"pattern":"(?=a)"}),
        json!({"pattern":"a$"}),
        json!({"pattern":"."}),
        json!({"pattern":"a{33,}"}),
        json!({"pattern":"x".repeat(validator::REGEX_BYTES+1)}),
    ] {
        assert!(validator::Candidate::compile(&schema, &json!({}), false).is_err());
    }
    let string =
        validator::Candidate::compile(&json!({"type":"string"}), &json!({}), false).unwrap();
    assert!(
        string
            .validate(&json!("x".repeat(validator::STRING_BYTES)))
            .unwrap()
    );
    assert!(
        string
            .validate(&json!("x".repeat(validator::STRING_BYTES + 1)))
            .is_err()
    );
    assert!(
        string
            .validate_bytes(&vec![b' '; validator::INSTANCE_BYTES + 1])
            .is_err()
    );
    let array = validator::Candidate::compile(&json!({"type":"array"}), &json!({}), false).unwrap();
    assert!(array.validate(&json!(vec![0; 256])).unwrap());
    assert!(array.validate(&json!(vec![0; 257])).is_err());
    let mut deep = json!(0);
    for _ in 0..validator::DEPTH + 1 {
        deep = json!([deep]);
    }
    assert!(array.validate(&deep).is_err());
}
#[test]
fn schema_candidate_measures_adversarial_linear_patterns_and_maximum_object_work() {
    let mut observations = Vec::new();
    for pattern in ["(a+)+b", "(a|aa)+b", "[A-Za-z0-9_-]{1,32}", "a{1,32}b"] {
        let schema = json!({"type":"string","pattern":pattern});
        let candidate = validator::Candidate::compile(&schema, &json!({}), false).unwrap();
        let value = json!("a".repeat(validator::STRING_BYTES));
        let start = WallInstant::now();
        for _ in 0..32 {
            candidate.validate(&value).unwrap();
        }
        observations.push(json!({"pattern":pattern,"iterations":32,"stringBytes":validator::STRING_BYTES,"elapsedMicros":start.elapsed().as_micros()}));
    }
    let properties = (0..64)
        .map(|i| {
            (
                format!("k{i}"),
                json!({"type":"string","pattern":"(a|aa)+b"}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let value = (0..64)
        .map(|i| (format!("k{i}"), json!(format!("{}b", "a".repeat(399)))))
        .collect::<serde_json::Map<_, _>>();
    let candidate = validator::Candidate::compile(
        &json!({"type":"object","properties":properties}),
        &json!({}),
        false,
    )
    .unwrap();
    let start = WallInstant::now();
    assert!(candidate.validate(&Value::Object(value.clone())).unwrap());
    observations.push(
        json!({"properties":64,"perStringBytes":400,"outcome":"all-pass","elapsedMicros":start.elapsed().as_micros()}),
    );
    let mut value = value;
    let last = value.keys().next_back().unwrap().clone();
    value.insert(last, json!("a".repeat(400)));
    let start = WallInstant::now();
    assert!(!candidate.validate(&Value::Object(value)).unwrap());
    observations.push(json!({"properties":64,"perStringBytes":400,"outcome":"last-fails","elapsedMicros":start.elapsed().as_micros()}));
    println!("SCHEMA_MEASUREMENTS {}", json!(observations));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_native_cel_pairs_execute_real_actions_and_worker_leases_survive_timeout_panic_and_late_results()
 {
    let _worker_tests = WORKER_TEST_LOCK.lock().await;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/v1/t");
    let mut action_names = std::collections::BTreeSet::new();
    let mut observations = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let fixture: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let Some(raw) = fixture.pointer("/ruleConfigurationPair/rust/rule") else {
            continue;
        };
        let candidate = Arc::new(
            transformer::Candidate::compile(raw)
                .unwrap_or_else(|error| panic!("{}: {error}", fixture["id"])),
        );
        let endpoint = raw["endpointRules"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        if fixture["id"]
            .as_str()
            .unwrap()
            .contains("caught-native-cel-rule-error")
        {
            let input = json!({"requestBody":"original","responseBody":"backend","requestHeaders":{"test-rule":"throw"}});
            for phase in ["req-tra", "res-tra"] {
                let c = candidate.clone();
                let endpoint = endpoint.clone();
                let result = transformer::worker(
                    input.clone(),
                    Instant::now() + Duration::from_secs(1),
                    CancellationToken::new(),
                    move |input| c.execute(input, &endpoint, phase),
                )
                .await
                .unwrap();
                assert_eq!(
                    result, input,
                    "caught condition failure must not mutate the response/request"
                );
            }
            observations.push(json!({"fixture":fixture["id"],"caughtConditionError":true,"contextUnchanged":true}));
            continue;
        }
        let request_value = raw["ruleBodies"]
            .as_object()
            .unwrap()
            .values()
            .find_map(|rule| {
                rule["expression"]
                    .as_str()
                    .and_then(|s| s.split('\'').nth(1))
            })
            .unwrap_or("mutate");
        for rule in raw["ruleBodies"].as_object().unwrap().values() {
            for action in rule["actions"].as_array().unwrap() {
                action_names.insert(action["actionRef"].as_str().unwrap().to_owned());
            }
        }
        let start = WallInstant::now();
        let c = candidate.clone();
        let request_endpoint = endpoint.clone();
        let request=transformer::worker(json!({"requestBody":request_value,"requestPath":"/x","requestHeaders":{"x-input":"request-value"}}),Instant::now()+Duration::from_secs(2),CancellationToken::new(),move|input|c.execute(input,&request_endpoint,"req-tra")).await.unwrap();
        let c = candidate.clone();
        let response=transformer::worker(json!({"requestBody":"mutate","responseBody":"backend","responseHeaders":{"x-input":"request-value"}}),Instant::now()+Duration::from_secs(2),CancellationToken::new(),move|input|c.execute(input,&endpoint,"res-tra")).await.unwrap();
        // Check actual named-action outputs, not just engine success.
        for rule in raw["ruleBodies"].as_object().unwrap().values() {
            for action in rule["actions"].as_array().unwrap() {
                let v = &action["actionValues"];
                match action["actionRef"].as_str().unwrap() {
                    "request-body-literal" => assert_eq!(request["requestBody"], v["body"]),
                    "response-body-literal" => assert_eq!(response["responseBody"], v["body"]),
                    "request-metadata-literal" => {
                        assert_eq!(request["requestPath"], v["requestPath"]);
                        assert_eq!(request["requestHeaders"], v["requestHeaders"]);
                    }
                    "request-local-response-literal" => {
                        assert_eq!(request["responseBody"], v["responseBody"])
                    }
                    "request-validation-error-literal" => {
                        assert_eq!(request["validationError"], true)
                    }
                    "response-headers-literal" => assert_eq!(response["responseHeaders"], *v),
                    _ => unreachable!(),
                }
            }
        }
        observations
            .push(json!({"fixture":fixture["id"],"elapsedMicros":start.elapsed().as_micros()}));
    }
    assert_eq!(action_names.len(), 6);
    println!("RULE_MEASUREMENTS {}", json!(observations));
    let lease = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let started = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut waits = Vec::new();
    for _ in 0..transformer::WORKER_COUNT {
        let l = lease.clone();
        let s = started.clone();
        let done = completed.clone();
        waits.push(tokio::spawn(transformer::worker(
            json!({}),
            Instant::now() + Duration::from_millis(80),
            CancellationToken::new(),
            move |v| {
                s.fetch_add(1, Ordering::SeqCst);
                let (lock, cv) = &*l;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
                done.fetch_add(1, Ordering::SeqCst);
                Ok(v)
            },
        )));
    }
    timeout(Duration::from_secs(2), async {
        while started.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for wait in waits {
        assert!(wait.await.unwrap().is_err());
    }
    assert_eq!(transformer::permits(), 0);
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    let submitted = Arc::new(AtomicBool::new(false));
    let flag = submitted.clone();
    assert!(
        transformer::worker(
            json!({}),
            Instant::now() + Duration::from_millis(10),
            CancellationToken::new(),
            move |v| {
                flag.store(true, Ordering::SeqCst);
                Ok(v)
            }
        )
        .await
        .is_err()
    );
    assert!(!submitted.load(Ordering::SeqCst));
    {
        let (lock, cv) = &*lease;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    timeout(Duration::from_secs(2), async {
        while transformer::permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), 2);
    assert!(
        transformer::worker(
            json!({}),
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
            |_| panic!("fixed test-only panic")
        )
        .await
        .is_err()
    );
    assert_eq!(transformer::permits(), 2);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        transformer::worker(
            json!({}),
            Instant::now() + Duration::from_secs(1),
            cancel,
            Ok
        )
        .await
        .is_err()
    );
    assert!(
        transformer::worker(
            json!({"warm":"clean"}),
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
            Ok
        )
        .await
        .unwrap()["warm"]
            == "clean"
    );
    response_pipeline::timeout_discards_late_response_and_finalizes_once().await;
    transformer_pipeline::actual_body_pair_uses_current_context_and_one_finalized_dispatch().await;
    // Cancellation while running and while waiting must retain leases until
    // the closures exit, and never submit a cancelled waiter.
    let mut releases = Vec::new();
    let mut running = Vec::new();
    let mut cancels = Vec::new();
    for _ in 0..transformer::WORKER_COUNT {
        let (send, receive) = std::sync::mpsc::channel::<()>();
        releases.push(send);
        let cancel = CancellationToken::new();
        cancels.push(cancel.clone());
        running.push(tokio::spawn(transformer::worker(
            json!({}),
            Instant::now() + Duration::from_secs(2),
            cancel,
            move |value| {
                receive.recv().unwrap();
                Ok(value)
            },
        )));
    }
    timeout(Duration::from_secs(1), async {
        while transformer::permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let waiting_cancel = CancellationToken::new();
    let cancel = waiting_cancel.clone();
    let submitted = Arc::new(AtomicBool::new(false));
    let flag = submitted.clone();
    let waiter = tokio::spawn(transformer::worker(
        json!({}),
        Instant::now() + Duration::from_secs(2),
        cancel,
        move |value| {
            flag.store(true, Ordering::SeqCst);
            Ok(value)
        },
    ));
    waiting_cancel.cancel();
    assert!(waiter.await.unwrap().is_err());
    assert!(!submitted.load(Ordering::SeqCst));
    for cancel in cancels {
        cancel.cancel();
    }
    for task in running {
        assert!(task.await.unwrap().is_err());
    }
    assert_eq!(transformer::permits(), 0);
    for send in releases {
        send.send(()).unwrap();
    }
    timeout(Duration::from_secs(1), async {
        while transformer::permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
