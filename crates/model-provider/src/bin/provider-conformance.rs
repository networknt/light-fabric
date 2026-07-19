use chrono::{DateTime, Utc};
use model_provider::conformance::ConformanceRunner;
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut corpus = None;
    let mut output = None;
    let mut expected = None;
    let mut as_of = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--corpus" => corpus = args.next().map(PathBuf::from),
            "--output" => output = args.next().map(PathBuf::from),
            "--expected" => expected = args.next().map(PathBuf::from),
            "--as-of" => as_of = args.next(),
            _ => return Err(format!("unknown argument `{argument}`").into()),
        }
    }
    let corpus = corpus.ok_or("--corpus is required")?;
    let output = output.ok_or("--output is required")?;
    let tested_at =
        DateTime::parse_from_rfc3339(&as_of.ok_or("--as-of is required")?)?.with_timezone(&Utc);
    fs::create_dir_all(&output)?;
    let results = ConformanceRunner::default().run(&corpus, tested_at)?;
    for result in results {
        if !result.is_current_and_passing(tested_at) {
            return Err(format!(
                "{} conformance failed",
                serde_json::to_value(result.provider)?
            )
            .into());
        }
        let name = match result.provider {
            model_provider::inference::ProviderFormat::OpenAi => "openai.json",
            model_provider::inference::ProviderFormat::Anthropic => "anthropic.json",
        };
        if let Some(expected) = &expected {
            let expected_result: model_provider::conformance::ConformanceResult =
                serde_json::from_slice(&fs::read(expected.join(name))?)?;
            if !expected_result.verify_digest() || expected_result != result {
                return Err(format!(
                    "checked-in {name} does not match generated conformance result"
                )
                .into());
            }
        }
        fs::write(output.join(name), serde_json::to_vec_pretty(&result)?)?;
    }
    Ok(())
}
