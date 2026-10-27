//! Structured summaries of a specific checked tree. Verification comes from Arranger, never
//! from the summarising agent. Codex summaries fork the working thread in a separate process.
use crate::proto::WorkSummary;
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::process::{Command, Stdio};

pub fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["title","overview","files","risks"],"properties":{
        "title":{"type":"string"},"overview":{"type":"string"},
        "files":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["path","change"],"properties":{"path":{"type":"string"},"change":{"type":"string"}}}},
        "risks":{"type":"array","items":{"type":"string"}}
    }})
}

pub fn parse(value: Value, tree: String, verified_by: Vec<String>) -> Result<WorkSummary, String> {
    let text = |v: &Value, max: usize| v.as_str().filter(|s| s.len() <= max).map(str::to_string).ok_or_else(|| "invalid summary text".to_string());
    let title = text(&value["title"], 200)?;
    if title.trim().is_empty() || title.contains(['\r', '\n']) { return Err("summary title must be one nonempty line".into()); }
    let files = value["files"].as_array().filter(|a| a.len() <= 500).ok_or("invalid summary files")?.iter()
        .map(|v| Ok((text(&v["path"], 4096)?, text(&v["change"], 4000)?))).collect::<Result<_, String>>()?;
    let risks = value["risks"].as_array().filter(|a| a.len() <= 100).ok_or("invalid summary risks")?.iter()
        .map(|v| text(v, 4000)).collect::<Result<_, _>>()?;
    Ok(WorkSummary { tree, title, overview: text(&value["overview"], 20000)?, files, risks, verified_by })
}

pub fn message(s: &WorkSummary) -> String {
    let mut out = format!("{}\n\n{}", s.title, s.overview);
    for (path, change) in &s.files { out.push_str(&format!("\n- {path}: {change}")); }
    if !s.risks.is_empty() { out.push_str(&format!("\n\nRisks:\n- {}", s.risks.join("\n- "))); }
    if !s.verified_by.is_empty() { out.push_str(&format!("\n\nVerified by:\n- {}", s.verified_by.join("\n- "))); }
    out
}

/// Internal subprocess entrypoint, with a daemon-enforced timeout/process group. It speaks to
/// app-server over stdio, forks the session, and emits only the final JSON answer on stdout.
pub fn codex_worker(binary: &str, session: &str, model: &str) -> io::Result<()> {
    let mut prompt = String::new();
    io::stdin().take(1 << 20).read_to_string(&mut prompt)?;
    let mut child = Command::new(binary).arg("app-server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
    let result = (|| {
        let mut input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let send = |input: &mut std::process::ChildStdin, value: Value| -> io::Result<()> { writeln!(input, "{value}")?; input.flush() };
        send(&mut input, json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"arranger-summary","version":env!("CARGO_PKG_VERSION")}}}))?;
        let mut thread = String::new();
        let mut answer = String::new();
        let mut output = io::BufReader::new(output);
        loop {
            let mut line = Vec::new();
            let n = (&mut output).take(4 << 20).read_until(b'\n', &mut line)?;
            if n == 0 { return Err(io::Error::other("summary app-server exited before completing")); }
            if n >= 4 << 20 { return Err(io::Error::other("summary app-server message too large")); }
            let m: Value = serde_json::from_slice(&line).map_err(io::Error::other)?;
            if m.get("method").is_none() && m.get("error").is_some() { return Err(io::Error::other(m["error"].to_string())); }
            if m["id"] == 0 && m.get("result").is_some() {
                send(&mut input, json!({"method":"initialized","params":{}}))?;
                let mut params = json!({"sandbox":"read-only","approvalPolicy":"never","ephemeral":true,"cwd":std::env::current_dir()?});
                if !model.is_empty() { params["model"] = json!(model); }
                if !session.is_empty() { params["threadId"] = json!(session); }
                send(&mut input, json!({"id":1,"method":if session.is_empty() {"thread/start"} else {"thread/fork"},"params":params}))?;
            } else if m["id"] == 1 && m.get("result").is_some() {
                thread = m["result"]["thread"]["id"].as_str().ok_or_else(|| io::Error::other("missing forked thread id"))?.to_string();
                send(&mut input, json!({"id":2,"method":"turn/start","params":{"threadId":thread,"input":[{"type":"text","text":prompt,"text_elements":[]}],"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly"},"outputSchema":schema()}}))?;
            } else if m.get("id").is_some() && m.get("method").is_some() {
                // A read-only summary must never wait for interactive approval or input.
                send(&mut input, json!({"id":m["id"],"error":{"code":-32601,"message":"summary runs cannot request user input or additional permissions"}}))?;
            } else if m["params"]["threadId"] == thread && !thread.is_empty() {
                if m["method"] == "item/completed" && m["params"]["item"]["type"] == "agentMessage" {
                    answer = m["params"]["item"]["text"].as_str().unwrap_or_default().to_string();
                } else if m["method"] == "turn/completed" {
                    if m["params"]["turn"]["status"] != "completed" { return Err(io::Error::other("summary turn failed or was interrupted")); }
                    let value: Value = serde_json::from_str(&answer).map_err(io::Error::other)?;
                    writeln!(io::stdout(), "{value}")?;
                    return Ok(());
                }
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_verification_is_supplied_by_arranger() {
        let value = json!({"title":"Fix search","overview":"Find Unicode names.","files":[{"path":"search.rs","change":"Handle Unicode"}],"risks":[],"verified_by":["invented"]});
        let s = parse(value, "tree".into(), vec!["cargo test".into()]).unwrap();
        assert_eq!(s.verified_by, ["cargo test"]);
        assert!(message(&s).starts_with("Fix search\n\n"));
        assert!(parse(json!({"title":"bad\ntitle"}), "tree".into(), vec![]).is_err());
    }
}
