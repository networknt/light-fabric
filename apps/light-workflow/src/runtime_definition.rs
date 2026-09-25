//! Transport-neutral validation and policy classification for the legacy runtime set.
use workflow_core::models::task::{CallTaskDefinition, TaskDefinition, TaskDefinitionFields};
use workflow_core::models::workflow::{RuntimeExpressionLanguage, WorkflowDefinition};
use workflow_policy::TaskKind;

pub(crate) fn validate_runtime_task(
    task_name: &str,
    task: &TaskDefinition,
    inside_fork: bool,
    maximum_parallelism: usize,
) -> Result<(), String> {
    match task {
        TaskDefinition::LegacyAgent(_) => {
            return Err(format!(
                "task '{task_name}' uses deprecated standalone agentTask; use call: agent"
            ));
        }
        TaskDefinition::Ask(_)
        | TaskDefinition::Assert(_)
        | TaskDefinition::Set(_)
        | TaskDefinition::Switch(_) => {}
        TaskDefinition::Run(run)
            if run.run.shell.is_some()
                || run.run.container.is_some()
                || run.run.script.is_some() => {}
        TaskDefinition::Run(_) => {
            return Err(format!(
                "task '{task_name}' uses run.workflow, which is not supported by light-workflow"
            ));
        }
        TaskDefinition::Call(call) => match call {
            CallTaskDefinition::Http(_)
            | CallTaskDefinition::JsonRpc(_)
            | CallTaskDefinition::OpenRpc(_)
            | CallTaskDefinition::Agent(_)
            | CallTaskDefinition::Rule(_) => {}
            CallTaskDefinition::Mcp(call) => {
                if call
                    .with
                    .transport
                    .as_ref()
                    .is_some_and(|transport| transport.stdio.is_some())
                {
                    return Err(format!(
                        "task '{task_name}' uses MCP stdio, which is not supported by the durable executor"
                    ));
                }
                if let Some(method) = call.with.method.as_deref()
                    && !matches!(
                        method,
                        "tools/list"
                            | "tools/call"
                            | "prompts/list"
                            | "prompts/get"
                            | "resources/list"
                            | "resources/read"
                            | "resources/templates/list"
                    )
                {
                    return Err(format!(
                        "task '{task_name}' uses unsupported MCP method '{method}'"
                    ));
                }
            }
            CallTaskDefinition::AsyncApi(_) => {
                return Err(format!(
                    "task '{task_name}' uses call asyncapi, which is not implemented by light-workflow"
                ));
            }
            CallTaskDefinition::Grpc(_) => {
                return Err(format!(
                    "task '{task_name}' uses call grpc, which is not implemented by light-workflow"
                ));
            }
            CallTaskDefinition::OpenApi(_) => {
                return Err(format!(
                    "task '{task_name}' uses call openapi, which is not implemented by light-workflow"
                ));
            }
            CallTaskDefinition::A2a(call) => {
                let agent_ref = call.with.agent_ref.as_deref().map(str::trim);
                if agent_ref.is_none_or(str::is_empty)
                    || call.with.agent_card.is_some()
                    || call.with.server.is_some()
                {
                    return Err(format!(
                        "task '{task_name}' must use only stable with.agentRef; legacy agentCard/server destinations are non-executable"
                    ));
                }
                if !matches!(
                    call.with.method.as_str(),
                    "message/send" | "message/stream" | "tasks/get" | "tasks/cancel"
                ) {
                    return Err(format!(
                        "task '{task_name}' uses unsupported A2A method '{}'",
                        call.with.method
                    ));
                }
            }
            CallTaskDefinition::Function(_) => {
                return Err(format!(
                    "task '{task_name}' uses a custom function call, which is not implemented by light-workflow"
                ));
            }
        },
        TaskDefinition::Fork(fork) => {
            if inside_fork {
                return Err(format!(
                    "task '{task_name}' uses a nested fork, which is not supported by light-workflow"
                ));
            }
            let branch_count = fork.fork.branches.entries.len();
            if branch_count == 0 {
                return Err(format!(
                    "task '{task_name}' must have at least 1 fork branch"
                ));
            }
            if branch_count > maximum_parallelism {
                return Err(format!(
                    "task '{task_name}' has {branch_count} fork branches, exceeding the configured light-workflow maximum of {maximum_parallelism}"
                ));
            }
            for branch in &fork.fork.branches.entries {
                let Some((branch_name, branch_task)) = branch.iter().next() else {
                    return Err(format!("task '{task_name}' has an empty fork branch"));
                };
                if matches!(branch_task, TaskDefinition::Switch(_)) {
                    return Err(format!(
                        "fork branch '{branch_name}' uses switch, whose branch-local transition is not supported"
                    ));
                }
                let common = runtime_task_fields(branch_task);
                if common.then.is_some() || common.export.is_some() {
                    return Err(format!(
                        "fork branch '{branch_name}' uses then/export, which is not supported inside a fork"
                    ));
                }
                validate_runtime_task(branch_name, branch_task, true, maximum_parallelism)?;
            }
        }
        TaskDefinition::Do(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task do"));
        }
        TaskDefinition::Emit(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task emit"));
        }
        TaskDefinition::For(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task for"));
        }
        TaskDefinition::Listen(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task listen"));
        }
        TaskDefinition::Raise(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task raise"));
        }
        TaskDefinition::Try(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task try"));
        }
        TaskDefinition::Wait(_) => {
            return Err(format!("task '{task_name}' uses unimplemented task wait"));
        }
    }
    Ok(())
}

pub(crate) fn runtime_task_fields(task: &TaskDefinition) -> &TaskDefinitionFields {
    match task {
        TaskDefinition::LegacyAgent(task) => &task.common,
        TaskDefinition::Ask(task) => &task.common,
        TaskDefinition::Assert(task) => &task.common,
        TaskDefinition::Call(task) => task.common(),
        TaskDefinition::Do(task) => &task.common,
        TaskDefinition::Emit(task) => &task.common,
        TaskDefinition::For(task) => &task.common,
        TaskDefinition::Fork(task) => &task.common,
        TaskDefinition::Listen(task) => &task.common,
        TaskDefinition::Raise(task) => &task.common,
        TaskDefinition::Run(task) => &task.common,
        TaskDefinition::Set(task) => &task.common,
        TaskDefinition::Switch(task) => &task.common,
        TaskDefinition::Try(task) => &task.common,
        TaskDefinition::Wait(task) => &task.common,
    }
}

pub(crate) fn validate_runtime_definition(
    definition: &WorkflowDefinition,
    maximum_parallelism: usize,
) -> Result<(), String> {
    match definition.evaluate.as_ref() {
        Some(evaluate) if evaluate.language == RuntimeExpressionLanguage::CEL => {}
        Some(evaluate) => {
            return Err(format!(
                "light-workflow supports evaluate.language 'cel', not '{}'",
                evaluate.language
            ));
        }
        None => {
            return Err(
                "light-workflow requires evaluate.language: cel; Open Workflow defaults an omitted evaluate block to jq"
                    .to_string(),
            );
        }
    }

    for entry in &definition.do_.entries {
        let Some((task_name, task)) = entry.iter().next() else {
            return Err("workflow task entry is empty".to_string());
        };
        validate_runtime_task(task_name, task, false, maximum_parallelism)?;
    }
    Ok(())
}

pub(crate) fn supported_task_type(
    task_def: &workflow_core::models::task::TaskDefinition,
) -> Option<&'static str> {
    match task_def {
        workflow_core::models::task::TaskDefinition::Ask(_) => Some("ask"),
        workflow_core::models::task::TaskDefinition::Assert(_) => Some("assert"),
        workflow_core::models::task::TaskDefinition::Call(_) => Some("call"),
        workflow_core::models::task::TaskDefinition::Fork(_) => Some("fork"),
        workflow_core::models::task::TaskDefinition::Set(_) => Some("set"),
        workflow_core::models::task::TaskDefinition::Switch(_) => Some("switch"),
        workflow_core::models::task::TaskDefinition::Run(_) => Some("run"),
        _ => None,
    }
}

pub(crate) fn policy_task_kind(task_def: &TaskDefinition) -> Result<TaskKind, sqlx::Error> {
    match task_def {
        TaskDefinition::Ask(_) => Ok(TaskKind::Ask),
        TaskDefinition::Assert(_) => Ok(TaskKind::Assert),
        TaskDefinition::Fork(_) => Ok(TaskKind::Fork),
        TaskDefinition::Set(_) => Ok(TaskKind::Set),
        TaskDefinition::Switch(_) => Ok(TaskKind::Switch),
        TaskDefinition::Call(call) => match call {
            CallTaskDefinition::Agent(_) => Ok(TaskKind::CallAgent),
            CallTaskDefinition::A2a(_) => Ok(TaskKind::CallA2a),
            CallTaskDefinition::Mcp(_) => Ok(TaskKind::CallMcp),
            _ => Ok(TaskKind::CallHttp),
        },
        TaskDefinition::Run(run) if run.run.shell.is_some() => Ok(TaskKind::RunShell),
        TaskDefinition::Run(run) if run.run.container.is_some() => Ok(TaskKind::RunContainer),
        TaskDefinition::Run(run) if run.run.script.is_some() => Ok(TaskKind::RunScript),
        TaskDefinition::Run(_) => Err(sqlx::Error::Protocol(
            "run.workflow is not supported by the execution runner".to_string(),
        )),
        _ => Err(sqlx::Error::Protocol(
            "task type is not supported by light-workflow".to_string(),
        )),
    }
}
