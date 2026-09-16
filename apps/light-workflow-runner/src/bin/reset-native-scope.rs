#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 3 || args[0] != "--confirm-fence" {
        return Err(
            "usage: reset-native-scope --confirm-fence <Claude runner config> <execution UUID>"
                .into(),
        );
    }
    let execution = args[2].parse()?;
    let reference =
        light_workflow_runner::operator_fence::reset(std::path::Path::new(&args[1]), execution)
            .map_err(std::io::Error::other)?;
    println!("{reference}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Native scope fencing requires Linux systemd");
    std::process::exit(1);
}
