fn main() -> anyhow::Result<()> {
    serverless_conformance::command(&std::env::args().skip(1).collect::<Vec<_>>())
}
