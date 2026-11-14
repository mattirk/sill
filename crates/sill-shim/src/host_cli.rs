//! Deliberately separate from SFP: no administrative capability and no session selector.
//! `sill-host` (the daemon's host) and `sill-mac` (the Mac of Sill.app's VM, docs/sill-mac.md)
//! both talk to the session's Host control bridge.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

pub fn run(args: &[String]) -> Result<()> {
    let operation = args.first().context("usage: sill-host state | OP GENERATION JSON_ARGS; launch/commands: {\"argv\":[...],\"cwd\":\"/absolute/host/path\",\"env\":{...}}; stop: {\"name\":\"host:ID\"}; stop_existing: {\"pid\":PID,\"start_ticks\":TICKS}; view: {} (the desktop, a PNG under /.sill/bridge/view/), {\"list\":true} (its windows) or {\"window\":ID}; input: {\"actions\":[{\"move\":[x,y]},{\"click\":1},{\"key\":\"ctrl+l\"},{\"type\":\"text\"},{\"wait\":ms}]}")?;
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
    let result = call(operation, generation, parameters)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

const MAC_USAGE: &str = "usage: sill-mac state | displays GENERATION | windows GENERATION | screenshot GENERATION (--display ID | --window ID) [--output PATH]
  state       Host control's generation and grants, and whether a Mac is attached
  displays    the Mac's displays: id, position and size in points, pixels, scale
  windows     the Mac's windows on screen: id, app, title, position and size in points
  screenshot  a PNG of one display or window, under /.sill/bridge/mac/ (and copied to PATH)
Needs the mac_view grant; macOS must allow Sill to record the screen.";

/// `sill-mac`: the Mac's screen, through the bridge's `mac_view`.
pub fn run_mac(args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).context(MAC_USAGE)?;
    if command == "state" {
        anyhow::ensure!(args.len() == 1, "state takes no arguments");
        let result = call("state", None, Value::Null)?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let generation: u64 = args
        .get(1)
        .context(MAC_USAGE)?
        .parse()
        .context("GENERATION is a number: `sill-mac state` reports it")?;
    let rest = &args[2..];
    let (request, output) = match command {
        "displays" | "windows" => {
            anyhow::ensure!(rest.is_empty(), "{command} takes only GENERATION");
            (json!({"list": command}), None)
        }
        "screenshot" => {
            let mut target = None;
            let mut output = None;
            let mut i = 0;
            while i < rest.len() {
                let value = rest.get(i + 1).with_context(|| format!("{} needs a value", rest[i]))?;
                match rest[i].as_str() {
                    "--display" => target = Some(json!({"display": value.parse::<u64>()?})),
                    "--window" => target = Some(json!({"window": value.parse::<u64>()?})),
                    "--output" => output = Some(value.clone()),
                    other => anyhow::bail!("unknown option {other}\n{MAC_USAGE}"),
                }
                i += 2;
            }
            (target.context("screenshot needs --display ID or --window ID")?, output)
        }
        _ => anyhow::bail!("{MAC_USAGE}"),
    };
    let result = call("mac_view", Some(generation), request)?;
    // the picture into the jail's own file when asked: never a write on the Mac
    if let (Some(output), Some(path)) = (output, result.get("path").and_then(Value::as_str)) {
        std::fs::copy(path, &output).with_context(|| format!("copying the picture to {output}"))?;
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

/// One request on the session's bridge: the result, or the refusal as the error.
fn call(operation: &str, generation: Option<u64>, parameters: Value) -> Result<Value> {
    let mut socket = UnixStream::connect("/.sill/bridge/host.sock").context(
        "Host control is inactive, or this session needs a restart to install the bridge",
    )?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(40)))?;
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
    Ok(response["result"].clone())
}
