//! One-shot mode: `harness "do something"` or `harness /skill args` runs the
//! prompt, and the turns that background work brings, then exits.

use std::process::ExitCode;

use anyhow::Result;
use serde_json::Value;

use super::keys::KeyWatch;
use super::output::{Terminal, TurnEnd};
use super::parse::{Input, parse_input, render_skill};
use super::sessions::{self, show_hook_notices};
use super::{Output, finish_mcp, json, text, turn};
use crate::agent::Agent;
use crate::session::Recorder;
use crate::setup::Env;

/// Runs `line`, the prompt from the command line, printing what happens as
/// `format` says.
pub(super) async fn run(
    agent: &mut Agent,
    env: &mut Env,
    line: &str,
    format: json::Format,
    mut recorder: Option<Recorder>,
    out: &Output,
) -> Result<ExitCode> {
    finish_mcp(agent, env, out).await;
    let input = match render_skill(parse_input(line, &env.ext.skills)).await {
        Input::Skill(skill, args) => match turn::run_fork(agent, env, &skill, &args, out).await {
            Ok(prompt) => Input::Prompt(prompt),
            Err(e) => Input::Error(e),
        },
        input => input,
    };
    let prompt = match input {
        Input::Prompt(prompt) => Ok(prompt),
        Input::Error(e) => Err(anyhow::anyhow!(e)),
        _ => Err(anyhow::anyhow!("REPL commands are only available interactively")),
    };
    if format != json::Format::Text {
        return Ok(run_json(agent, line, prompt, &env.files, format, recorder.as_mut(), out).await);
    }
    let prompt = prompt?;
    let mut term = Terminal::new(out.clone());
    let attached = turn::attachments(line, &[], &env.files);
    let (content, title) = turn::prompt_content(agent, line, &prompt, attached, out);
    // The turns' outcomes are already shown.
    let mut result = turn::run_shown(agent, content, title, out, &mut term, recorder.as_mut(), None).await;
    while result.is_ok() {
        result = match next(agent, out).await {
            Next::Turn => turn::run_shown(agent, Vec::new(), None, out, &mut term, recorder.as_mut(), None).await,
            Next::Done => break,
            Next::Interrupted => Err(TurnEnd::Interrupted),
        };
    }
    show_hook_notices(out, agent.end_session("other").await);
    Ok(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(TurnEnd::Interrupted) => ExitCode::from(130),
        Err(TurnEnd::Failed(_)) => ExitCode::FAILURE,
    })
}

/// One-shot mode with `--output-format json` or `stream-json`: runs
/// `prompt`, and the turns that background work brings, then prints the
/// result (see [`json::Events`]).
async fn run_json(
    agent: &mut Agent,
    line: &str,
    prompt: Result<String>,
    files: &crate::tools::FileTracker,
    format: json::Format,
    recorder: Option<&mut Recorder>,
    out: &Output,
) -> ExitCode {
    let session_id = recorder.as_ref().map(|rec| rec.meta().id.clone());
    let mut events = json::Events::new(format);
    events.init(agent, session_id.as_deref());
    let start = agent.messages().len();
    let mut interrupted = false;
    let mut result = match prompt {
        Ok(prompt) => {
            agent.set_prompt_label(line);
            let content = turn::attachments(line, &[], files).into_content(&prompt);
            run_turn(agent, content, &mut events, &mut interrupted).await
        }
        Err(e) => Err(e),
    };
    while result.is_ok() {
        result = match next(agent, out).await {
            Next::Turn => run_turn(agent, Vec::new(), &mut events, &mut interrupted).await,
            Next::Done => break,
            Next::Interrupted => {
                interrupted = true;
                Err(anyhow::anyhow!("interrupted"))
            }
        };
    }
    if let Some(rec) = recorder {
        sessions::save(agent, rec, out);
    }
    agent.end_session("other").await;
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    json::print(&events.result(agent, session_id.as_deref(), error, start));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) if interrupted => ExitCode::from(130),
        Err(_) => ExitCode::FAILURE,
    }
}

/// Runs a turn until it ends or Ctrl-C sets `interrupted`.
async fn run_turn(agent: &mut Agent, content: Vec<Value>, events: &mut json::Events, interrupted: &mut bool) -> Result<()> {
    tokio::select! {
        result = agent.run_turn(content, events) => result,
        _ = tokio::signal::ctrl_c() => {
            *interrupted = true;
            Err(anyhow::anyhow!("interrupted"))
        }
    }
}

/// What comes after a turn.
enum Next {
    /// Another, for what arrived for the model.
    Turn,
    /// Nothing: no background work is left running.
    Done,
    /// The user stopped the wait for background work.
    Interrupted,
}

/// Waits, after a turn, until something arrives for the model or no
/// background work is left running.
async fn next(agent: &Agent, out: &Output) -> Next {
    if agent.inbox().wants_turn() {
        return Next::Turn;
    }
    let running = agent.subagents().running_in_background() + agent.shells().pending();
    if running == 0 {
        return Next::Done;
    }
    if wait_for_reports(agent, running, out).await { Next::Turn } else { Next::Interrupted }
}

/// Shows that `running` background subagents are still at work, and what
/// they last did, until one reports back; false if the user interrupted the
/// wait.
async fn wait_for_reports(agent: &Agent, running: usize, out: &Output) -> bool {
    out.set_status(Some(&format!("Waiting for {running} background tasks…")));
    let mut esc = KeyWatch::start();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let reported = loop {
        tokio::select! {
            _ = agent.inbox().wanted() => break true,
            _ = tokio::signal::ctrl_c() => break false,
            _ = esc.pressed() => break false,
            _ = tick.tick() => out.set_status_detail(&background_progress(&agent.subagents().list())),
        }
    };
    drop(esc);
    out.set_status(None);
    reported
}

/// The latest call of each running background subagent, for the line
/// under the status.
fn background_progress(subagents: &[Value]) -> String {
    let running = subagents.iter().filter(|s| s["status"] == "running" && s["background"] == true);
    let lines: Vec<String> = running
        .map(|s| {
            let last = s["last_call"].as_str().unwrap_or("starting");
            format!("{} › {}", text::fit(s["label"].as_str().unwrap_or(""), 32), text::printable(last))
        })
        .collect();
    lines.join(" · ")
}
