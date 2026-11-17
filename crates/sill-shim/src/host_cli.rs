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
        anyhow::ensure!(args.len() == 3, "expected OP GENERATION JSON_ARGS (use state to read the generation)");
        (Some(args[1].parse::<u64>()?), serde_json::from_str::<Value>(&args[2])?)
    };
    let result = call(operation, generation, parameters)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

const MAC_USAGE: &str = "usage: sill-mac state | displays G | windows G | screenshot G (--display ID | --window ID) [--output PATH]
       sill-mac click G --frame F X Y [--right] [--double] | move G --frame F X Y | drag G --frame F X1 Y1 X2 Y2
       sill-mac scroll G --frame F X Y DY [DX] | type G TEXT | key G COMBO | input G JSON
       sill-mac run G --cwd DIR [--env K=V]… -- PROGRAM ARG… | ps G | output G mac:N | stop G mac:N
  state       Host control's generation and grants, and whether a Mac is attached
  displays    the Mac's displays: id, position and size in points, pixels, scale
  windows     the Mac's windows on screen: id, app, title, position and size in points
  screenshot  a PNG of one display or window, under /.sill/bridge/mac/ (and copied to PATH); its
              reply names the picture's frame
  click/move/drag/scroll  at X Y in frame F's pixels (a picture from the last two minutes whose
              target has not moved); scroll DY/DX in pixels, negative scrolls up/left
  type        text where the Mac's focus is (not through the clipboard)
  key         a combination like cmd+shift+p, return, escape, left
  input       up to 32 actions in one request: {\"frame\":F,\"actions\":[{\"click\":[x,y]},{\"type\":\"text\"},…]}
  run         start PROGRAM on the Mac as the person, in DIR (inside a shared Mac folder), with the
              person's login environment; returns its name mac:N at once
  ps          this session's Mac processes: name, argv, state, exit code
  output      the last 64 KiB of a Mac process's output (stdout and stderr together)
  stop        stop a Mac process and everything it started
G is the generation (`sill-mac state`). Viewing needs the mac_view grant and macOS's Screen
Recording consent for Sill; input needs mac_input and macOS's Accessibility consent; run, ps,
output and stop need mac_commands (unrestricted: as the person; Mac processes stop on revocation
and when Sill quits). When the
person uses the mouse or keyboard, the agent's input stops and says how far it got: never repeat
it blindly, take a new picture.";

/// `sill-mac`: the Mac's screen, through the bridge's `mac_view`.
pub fn run_mac(args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).context(MAC_USAGE)?;
    if command == "state" {
        anyhow::ensure!(args.len() == 1, "state takes no arguments");
        let result = call("state", None, Value::Null)?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let generation: u64 = args.get(1).context(MAC_USAGE)?.parse().context("GENERATION is a number: `sill-mac state` reports it")?;
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
        "run" | "ps" | "output" | "stop" => {
            let request = mac_commands(command, rest)?;
            let result = call("mac_commands", Some(generation), request)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        "click" | "move" | "drag" | "scroll" | "type" | "key" | "input" => {
            let request = mac_input(command, rest)?;
            let result = call("mac_input", Some(generation), request)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
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

/// A process request from `sill-mac run|ps|output|stop` and its arguments.
fn mac_commands(command: &str, rest: &[String]) -> Result<Value> {
    match command {
        "ps" => {
            anyhow::ensure!(rest.is_empty(), "ps takes only GENERATION");
            Ok(json!({"list": true}))
        }
        "output" | "stop" => {
            anyhow::ensure!(rest.len() == 1, "{command} takes a name, mac:N (`sill-mac ps`)");
            Ok(json!({command: rest[0]}))
        }
        _ => {
            let mut cwd = None;
            let mut env = serde_json::Map::new();
            let mut i = 0;
            while i < rest.len() && rest[i] != "--" {
                let value = rest.get(i + 1).with_context(|| format!("{} needs a value", rest[i]))?;
                match rest[i].as_str() {
                    "--cwd" => cwd = Some(value.clone()),
                    "--env" => {
                        let (k, v) = value.split_once('=').context("--env takes KEY=VALUE")?;
                        env.insert(k.into(), json!(v));
                    }
                    other => anyhow::bail!("unknown option {other}\n{MAC_USAGE}"),
                }
                i += 2;
            }
            let argv = rest.get(i + 1..).filter(|a| !a.is_empty()).context("run needs -- PROGRAM ARG…")?;
            let cwd = cwd.context("run needs --cwd DIR, inside a shared Mac folder")?;
            Ok(json!({"run": {"argv": argv, "cwd": cwd, "env": env}}))
        }
    }
}

/// An input request from `sill-mac click|move|drag|scroll|type|key|input` and its arguments.
fn mac_input(command: &str, rest: &[String]) -> Result<Value> {
    if command == "input" {
        anyhow::ensure!(rest.len() == 1, "input takes one JSON argument");
        return Ok(serde_json::from_str(&rest[0])?);
    }
    if command == "type" || command == "key" {
        anyhow::ensure!(rest.len() == 1, "{command} takes one argument (quote it)");
        return Ok(json!({"actions": [{command: rest[0]}]}));
    }
    let mut frame = None;
    let mut numbers = Vec::new();
    let (mut right, mut double) = (false, false);
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--frame" => {
                frame = Some(rest.get(i + 1).context("--frame needs a value")?.clone());
                i += 1;
            }
            "--right" => right = true,
            "--double" => double = true,
            n => numbers.push(n.parse::<f64>().with_context(|| format!("{n} is not a number\n{MAC_USAGE}"))?),
        }
        i += 1;
    }
    let frame = frame.context("--frame F: the frame of a recent picture (`sill-mac screenshot`)")?;
    let action = match (command, numbers.as_slice()) {
        ("click", [x, y]) => {
            let mut a = json!({"click": [x, y]});
            if right {
                a["button"] = json!("right");
            }
            if double {
                a["count"] = json!(2);
            }
            a
        }
        ("move", [x, y]) => json!({"move": [x, y]}),
        ("drag", [x1, y1, x2, y2]) => json!({"drag": [[x1, y1], [x2, y2]]}),
        ("scroll", [x, y, dy]) => json!({"scroll": [x, y], "dy": dy}),
        ("scroll", [x, y, dy, dx]) => json!({"scroll": [x, y], "dy": dy, "dx": dx}),
        _ => anyhow::bail!("wrong number of coordinates for {command}\n{MAC_USAGE}"),
    };
    Ok(json!({"frame": frame, "actions": [action]}))
}

/// One request on the session's bridge: the result, or the refusal as the error.
fn call(operation: &str, generation: Option<u64>, parameters: Value) -> Result<Value> {
    let mut socket =
        UnixStream::connect("/.sill/bridge/host.sock").context("Host control is inactive, or this session needs a restart to install the bridge")?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(40)))?;
    socket.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    writeln!(socket, "{}", json!({"operation":operation,"generation":generation,"args":parameters}))?;
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line).context("host bridge disconnected; action outcome may be unknown; do not replay automatically")?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("{error}");
    }
    Ok(response["result"].clone())
}
