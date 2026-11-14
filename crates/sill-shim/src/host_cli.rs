//! Deliberately separate from SFP: no administrative capability and no session selector.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

pub fn run(args: &[String]) -> Result<()> {
    let operation = args.first().context("usage: sill-host state | OP GENERATION JSON_ARGS; launch/commands: {\"argv\":[...],\"cwd\":\"/absolute/host/path\",\"env\":{...}}; stop: {\"name\":\"host:ID\"}; stop_existing: {\"pid\":PID,\"start_ticks\":TICKS}")?;
    let (generation, parameters) = if operation == "state" {
        anyhow::ensure!(args.len() == 1, "state takes no arguments");
        (None, Value::Null)
    } else {
        anyhow::ensure!(
            args.len() == 3,
            "expected OP GENERATION JSON_ARGS (use state to read the generation)"
        );
        (
            Some(args[1].parse::<u64>()?),
            serde_json::from_str::<Value>(&args[2])?,
        )
    };
    let mut socket = UnixStream::connect("/.sill/bridge/host.sock").context(
        "Host control is inactive, or this session needs a restart to install the bridge",
    )?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(15)))?;
    socket.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    writeln!(
        socket,
        "{}",
        json!({"operation":operation,"generation":generation,"args":parameters})
    )?;
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line).context(
        "host bridge disconnected; action outcome may be unknown; do not replay automatically",
    )?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("{error}");
    }
    println!("{}", serde_json::to_string_pretty(&response["result"])?);
    Ok(())
}
