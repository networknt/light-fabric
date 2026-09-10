//! Owner-local CLI and stdio MCP facade. Identity is fixed by process arguments,
//! never accepted from tool input. Do not expose this process over a public socket.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    path::Path,
};
use task_workspace::{Access, FileEdit, Workspace, WorkspaceStore};

#[derive(Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Request {
    Create {
        task: String,
    },
    Status {
        task: String,
    },
    Files {
        task: String,
    },
    Index {
        task: String,
        provider: String,
    },
    IndexQuery {
        task: String,
        provider: String,
        repository: String,
        query: String,
    },
    IndexStatus {
        task: String,
        provider: String,
    },
    Read {
        task: String,
        repository: String,
        path: String,
    },
    Edit {
        task: String,
        edit: FileEdit,
    },
    Freeze {
        task: String,
    },
    Execute {
        task: String,
        access: Access,
        executable: String,
        args: Vec<String>,
        timeout_seconds: u64,
    },
    Review {
        task: String,
        checkpoint: String,
        approved: bool,
        findings: String,
    },
    Remediate {
        task: String,
    },
    Commit {
        task: String,
        message: String,
    },
    Push {
        task: String,
    },
    Github {
        task: String,
        repository: String,
        action: String,
        title: String,
        body: String,
    },
}

fn execute(store: &WorkspaceStore, workspace: &str, agent: &str, value: Value) -> Result<Value> {
    let request: Request = serde_json::from_value(value)?;
    Ok(match request {
        Request::Create { task } => {
            serde_json::to_value(store.create_task(workspace, &task, agent)?)?
        }
        Request::Status { task } => serde_json::to_value(store.status(workspace, &task, agent)?)?,
        Request::Index { task, provider } => {
            serde_json::to_value(store.index(workspace, &task, agent, &provider)?)?
        }
        Request::IndexQuery {
            task,
            provider,
            repository,
            query,
        } => serde_json::to_value(store.query_index(
            workspace,
            &task,
            agent,
            &provider,
            &repository,
            &query,
        )?)?,
        Request::IndexStatus { task, provider } => {
            serde_json::to_value(store.index_status(workspace, &task, agent, &provider)?)?
        }
        Request::Files { task } => serde_json::to_value(store.files(workspace, &task, agent)?)?,
        Request::Read {
            task,
            repository,
            path,
        } => serde_json::to_value(store.read_file(workspace, &task, agent, &repository, &path)?)?,
        Request::Edit { task, edit } => {
            serde_json::to_value(store.edit_file(workspace, &task, agent, edit)?)?
        }
        Request::Execute {
            task,
            access,
            executable,
            args,
            timeout_seconds,
        } => serde_json::to_value(store.run(
            workspace,
            &task,
            agent,
            access,
            Path::new(&executable),
            &args,
            timeout_seconds,
        )?)?,
        Request::Freeze { task } => serde_json::to_value(store.freeze(workspace, &task, agent)?)?,
        Request::Review {
            task,
            checkpoint,
            approved,
            findings,
        } => serde_json::to_value(store.review(
            workspace,
            &task,
            agent,
            &checkpoint,
            approved,
            findings,
        )?)?,
        Request::Remediate { task } => {
            serde_json::to_value(store.remediate(workspace, &task, agent)?)?
        }
        Request::Commit { task, message } => {
            serde_json::to_value(store.commit(workspace, &task, agent, &message)?)?
        }
        Request::Push { task } => serde_json::to_value(store.push(workspace, &task, agent)?)?,
        Request::Github {
            task,
            repository,
            action,
            title,
            body,
        } => serde_json::to_value(store.github(
            workspace,
            &task,
            agent,
            &repository,
            &action,
            &title,
            &body,
        )?)?,
    })
}

fn schema(operation: &str, additional: Value, required: &[&str]) -> Value {
    let mut properties =
        json!({"operation":{"const":operation,"type":"string"},"task":{"type":"string"}});
    for (key, value) in additional.as_object().unwrap() {
        properties[key] = value.clone();
    }
    let mut required_fields = vec!["operation", "task"];
    required_fields.extend_from_slice(required);
    json!({"type":"object","properties":properties,"required":required_fields,"additionalProperties":false})
}
fn tools() -> Value {
    let text = json!({"type":"string"});
    let variants = vec![
        schema("create", json!({}), &[]),
        schema("status", json!({}), &[]),
        schema("files", json!({}), &[]),
        schema(
            "index-query",
            json!({"provider":text,"repository":text,"query":text}),
            &["provider", "repository", "query"],
        ),
        schema("index", json!({"provider":text}), &["provider"]),
        schema("index-status", json!({"provider":text}), &["provider"]),
        schema(
            "read",
            json!({"repository":text,"path":text}),
            &["repository", "path"],
        ),
        schema(
            "edit",
            json!({"edit":{"type":"object","properties":{"repository":text,"path":text,"content":{"type":["string","null"]},"expectedDigest":{"type":["string","null"]}},"required":["repository","path","content","expectedDigest"],"additionalProperties":false}}),
            &["edit"],
        ),
        schema(
            "execute",
            json!({"access":{"enum":["implement","review"]},"executable":text,"args":{"type":"array","items":text},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":300}}),
            &["access", "executable", "args", "timeoutSeconds"],
        ),
        schema("freeze", json!({}), &[]),
        schema(
            "review",
            json!({"checkpoint":text,"approved":{"type":"boolean"},"findings":{"type":"string","maxLength":65536}}),
            &["checkpoint", "approved", "findings"],
        ),
        schema("remediate", json!({}), &[]),
        schema("commit", json!({"message":text}), &["message"]),
        schema("push", json!({}), &[]),
        schema(
            "github",
            json!({"repository":text,"action":{"enum":["issue","pull-request"]},"title":text,"body":text}),
            &["repository", "action", "title", "body"],
        ),
    ];
    json!({"tools":[{"name":"task_workspace","description":"Manage a registered multi-repository task workspace. All repositories are accessible. Create a task, read files and edit with expectedDigest, freeze for an independent review, then commit/push and create linked GitHub issues or PRs as authorized. Frozen edits and stale reviews fail. Identity and workspace are fixed by the host. Never pass host paths or credentials.","inputSchema":{"type":"object","oneOf":variants}}]})
}

fn respond(store: &WorkspaceStore, workspace: &str, agent: &str, request: Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let result: Result<Value> = match request["method"].as_str() {
        Some("initialize") => Ok(
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"light-workspace","version":env!("CARGO_PKG_VERSION")}}),
        ),
        Some("ping") => Ok(json!({})),
        Some("tools/list") => Ok(tools()),
        Some("tools/call") if request["params"]["name"] == "task_workspace" => {
            let result = execute(
                store,
                workspace,
                agent,
                request["params"]["arguments"].clone(),
            );
            Ok(match result {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(error) => {
                    json!({"content":[{"type":"text","text":error.to_string()}],"isError":true})
                }
            })
        }
        _ => {
            return Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method or tool not found"}}),
            );
        }
    };
    Some(match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":error.to_string()}})
        }
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("discover") {
        if args.len() < 5 {
            bail!(
                "usage: light-workspace discover <source-directory> <workspace-id> <host-id> <agent-id>..."
            );
        }
        let discovered = task_workspace::discover(
            Path::new(&args[1]),
            &args[2],
            &args[3],
            args[4..].iter().cloned().collect(),
        )?;
        println!("{}", serde_json::to_string_pretty(&discovered)?);
        return Ok(());
    }
    if args.len() < 3 {
        bail!(
            "usage: light-workspace <store-directory> register <workspace.json> | <store-directory> <call|serve> <workspace-id> <agent-id>"
        );
    }
    let store = WorkspaceStore::open(Path::new(&args[0]))?;
    if args[1] == "register" {
        if args.len() != 3 {
            bail!("register requires exactly one registration file");
        }
        let workspace: Workspace = serde_json::from_slice(&std::fs::read(&args[2])?)?;
        store.register(&workspace)?;
        println!("{}", json!({"registered":workspace.id}));
        return Ok(());
    }
    if args[1] == "recover" {
        if args.len() != 7 || args[5] != "--fenced-generation" {
            bail!(
                "usage: light-workspace STORE recover WORKSPACE TASK AGENT --fenced-generation GENERATION (operator must first confirm old execution termination)"
            );
        }
        let generation = args[6]
            .parse::<u64>()
            .context("invalid fenced generation")?;
        let task = store.recover_generation_after_fencing(
            &args[2],
            &args[3],
            &args[4],
            Some(generation),
        )?;
        println!("{}", serde_json::to_string_pretty(&task)?);
        return Ok(());
    }
    if args.len() != 4 {
        bail!("call/serve require workspace and authenticated local agent identity");
    }
    if args[1] == "call" {
        let value: Value = serde_json::from_reader(io::stdin().lock())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&execute(&store, &args[2], &args[3], value)?)?
        );
        return Ok(());
    }
    if args[1] != "serve" {
        bail!("unknown command");
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(frame) = read_frame(&mut input)? {
        let response = match frame {
            Frame::Oversized => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"MCP request exceeds 2 MiB"}}),
            ),
            Frame::Message(line) => match serde_json::from_slice(&line) {
                Ok(value) => respond(&store, &args[2], &args[3], value),
                Err(_) => Some(
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
                ),
            },
        };
        if let Some(response) = response {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
enum Frame {
    Message(Vec<u8>),
    Oversized,
}

fn read_frame(input: &mut impl BufRead) -> io::Result<Option<Frame>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return Ok(if oversized {
                Some(Frame::Oversized)
            } else if bytes.is_empty() {
                None
            } else {
                Some(Frame::Message(bytes))
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if !oversized {
            if bytes.len() + consumed > MAX_FRAME_BYTES {
                bytes.clear();
                oversized = true;
            } else {
                bytes.extend_from_slice(&available[..consumed]);
            }
        }
        input.consume(consumed);
        if newline.is_some() {
            return Ok(Some(if oversized {
                Frame::Oversized
            } else {
                Frame::Message(bytes)
            }));
        }
    }
}
