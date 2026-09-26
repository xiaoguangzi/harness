//! System prompts.
//!
//! The main agent's prompt is the template in `system.md`, with a section
//! describing the tools actually registered and one describing the
//! environment. It is built once per conversation, so it stays a stable
//! prompt-cache prefix.

use std::collections::BTreeSet;
use std::path::Path;

use crate::subagent::AgentDef;
use crate::tools::ToolRegistry;

const TEMPLATE: &str = include_str!("system.md");

/// How to use each built-in tool, in the order they are listed.
const GUIDANCE: &[(&str, &str)] = &[
    (
        "read",
        "view a file with line numbers. Read the code you are about to change or explain; page \
         through large files with `offset` and `limit`.",
    ),
    (
        "edit",
        "change part of an existing file by replacing an exact, unique piece of it. This is how \
         to change files: keep `old_string` to the lines that change plus a little context, and \
         make several small edits rather than one that repeats most of the file. The file must \
         have been read (or attached) first.",
    ),
    (
        "write",
        "create a new file, or replace one you are rewriting from scratch. Not for changes to \
         part of a file, which is what `edit` is for. An existing file must have been read (or \
         attached) first.",
    ),
    ("grep", "search file contents by regular expression. Use it instead of running grep or rg through `bash`."),
    (
        "bash",
        "builds, tests, git, listing files and other shell work. Every call is a fresh process, so \
         `cd` does not persist: use absolute paths or `cd dir && command`. Do not use it to read or \
         search files when the tools above can, and never to create or change files: no Python, \
         Node or Perl scripts, `sed -i`, heredocs or `>` redirection into files. Those changes go \
         through `edit` and `write`, which check that you read the file and show the user the \
         change; commands that edit files themselves are refused. Running the project's own build \
         steps, formatters and code generators is fine.",
    ),
    (
        "skill",
        "load instructions for a specialised kind of task. When the request matches one of the \
         skills it lists, load that skill before starting and follow it.",
    ),
    (
        "agent",
        "hand a task to a subagent with its own context, which returns only its report. Use it for \
         open-ended searches that would take many calls, launching several at once for independent \
         questions; do a lookup that takes one or two calls yourself. With `run_in_background` you keep \
         working and the report arrives by itself in a later message, even after your turn ends: don't \
         poll or wait for it, and don't guess what it will say. Give subagents that change files \
         separate files to work on.",
    ),
    (
        "message_agent",
        "send a subagent from earlier in this conversation a follow-up, a correction or more work; it \
         keeps its context, so prefer this to starting a new one on the same ground.",
    ),
    ("stop_agent", "stop a subagent whose work is no longer needed."),
];

/// The main agent's system prompt, for the tools in `tools`.
pub fn main(tools: &ToolRegistry) -> String {
    TEMPLATE.replace("{{tools}}", &tools_section(tools)).replace("{{environment}}", &environment())
}

/// A subagent's system prompt: its definition's prompt plus the environment.
pub fn subagent(def: &AgentDef) -> String {
    format!(
        "{}\n\nYou were started by another agent, which sees only your final message; the user does \
         not see it directly.\n\n# Environment\n\n{}",
        def.prompt,
        environment()
    )
}

/// One line per tool: built-ins with guidance, then any other tools, then
/// MCP tools grouped by server.
fn tools_section(tools: &ToolRegistry) -> String {
    let mut lines: Vec<String> = GUIDANCE
        .iter()
        .filter(|(name, _)| tools.get(name).is_some())
        .map(|(name, text)| format!("- `{name}`: {text}"))
        .collect();
    let mut servers = BTreeSet::new();
    for name in tools.names() {
        match name.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
            Some((server, _)) => {
                servers.insert(server);
            }
            None if !GUIDANCE.iter().any(|(known, _)| *known == name) => {
                lines.push(format!("- `{name}`: see its description."));
            }
            None => {}
        }
    }
    if !servers.is_empty() {
        let names: Vec<String> = servers.iter().map(|s| format!("`{s}`")).collect();
        lines.push(format!(
            "- `mcp__<server>__<tool>`: tools from the connected MCP servers ({}). Each tool's \
             description says what it does; prefer one over a shell workaround when it fits.",
            names.join(", ")
        ));
    }
    lines.join("\n")
}

fn environment() -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let git = cwd.ancestors().any(|dir| dir.join(".git").exists());
    let shell = std::env::var("SHELL").ok().and_then(|s| Path::new(&s).file_name().map(|n| n.to_string_lossy().into_owned()));
    let mut lines = vec![
        format!("- Working directory: {}", cwd.display()),
        format!("- Git repository: {}", if git { "yes" } else { "no" }),
        format!("- Platform: {} ({})", std::env::consts::OS, std::env::consts::ARCH),
    ];
    if let Some(shell) = shell {
        lines.push(format!("- User's shell: {shell} (the `bash` tool always runs bash)"));
    }
    if let Some(sandbox) = crate::sandbox::active() {
        lines.push(format!("- Sandbox: {}", sandbox.prompt_note()));
    }
    if let Some(date) = today() {
        lines.push(format!("- Today's date: {date}"));
    }
    lines.join("\n")
}

/// The local date as `YYYY-MM-DD`.
fn today() -> Option<String> {
    let now = libc::time_t::try_from(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs(),
    )
    .ok()?;
    // SAFETY: `tm` is plain data, and `localtime_r` only writes into it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return None;
        }
        tm
    };
    Some(format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FileTracker, GrepTool};

    #[test]
    fn fills_the_template() {
        let prompt = main(&ToolRegistry::with_defaults(FileTracker::default()));
        assert!(!prompt.contains("{{"), "{prompt}");
        for tool in ["read", "edit", "write", "grep", "bash"] {
            assert!(prompt.contains(&format!("- `{tool}`: ")), "missing {tool}");
        }
        assert!(!prompt.contains("`skill`:") && !prompt.contains("`agent`:"));
        assert!(prompt.contains("- Working directory: "));
    }

    #[test]
    fn lists_only_registered_tools_and_groups_mcp_servers() {
        let mut tools = ToolRegistry::new();
        tools.register(GrepTool);
        let section = tools_section(&tools);
        assert_eq!(section.lines().count(), 1, "{section}");
        assert!(section.starts_with("- `grep`: "));
        assert!(!section.contains("mcp__"));

        for name in ["mcp__fs__read_file", "mcp__fs__list", "mcp__github__search", "custom"] {
            tools.register(Named(name));
        }
        let section = tools_section(&tools);
        assert!(section.contains("- `custom`: see its description."), "{section}");
        assert!(section.contains("MCP servers (`fs`, `github`)"), "{section}");
        assert_eq!(section.lines().count(), 3, "{section}");
    }

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl crate::tools::Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn run(&self, _: serde_json::Value) -> crate::tools::ToolOutput {
            crate::tools::ToolOutput::ok("")
        }
    }

    #[test]
    fn today_is_a_date() {
        let date = today().unwrap();
        assert_eq!(date.len(), 10, "{date}");
        assert!(date.starts_with("20"), "{date}");
    }
}
