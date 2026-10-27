//! System prompts.
//!
//! The main agent's prompt is the template in `system.md`, with a section
//! describing the tools actually registered, one describing the environment
//! and the project's `AGENTS.md`. It is built once per conversation, so it
//! stays a stable prompt-cache prefix.

use std::collections::BTreeSet;
use std::path::Path;

use crate::subagent::AgentDef;
use crate::tools::ToolRegistry;

const TEMPLATE: &str = include_str!("system.md");
/// What `/init` asks the model to do.
const INIT: &str = include_str!("init.md");
/// How much of `AGENTS.md` goes into the prompt, in bytes; the model reads
/// the rest itself.
const NOTES_LIMIT: usize = 40_000;
/// The same for `~/.harness/AGENTS.md` and a subdirectory's `AGENTS.md`.
const OTHER_NOTES_LIMIT: usize = 20_000;

/// How to use each built-in tool, in the order they are listed.
const GUIDANCE: &[(&str, &str)] = &[
    (
        "read",
        "view a file with line numbers. Read the code you are about to change or explain; page \
         through large files with `offset` and `limit`. It also shows you images and PDFs, such as \
         screenshots a test wrote.",
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
    (
        "todo_write",
        "keep a checklist for work with three or more steps, which the user sees: write it when you \
         start, mark each item in_progress as you begin it (one at a time) and completed as soon as \
         it is done, and add items you discover. Skip it for small tasks.",
    ),
    ("grep", "search file contents by regular expression. Use it instead of running grep or rg through `bash`."),
    ("glob", "find files by name or path pattern. Use it instead of running find or ls -R through `bash`."),
    (
        "lsp",
        "ask the language server where a symbol is defined or used, its type, a file's outline, or the \
         errors in a file. More precise than `grep` for following code; check `diagnostics` after \
         changing a file it covers.",
    ),
    (
        "bash",
        "builds, tests, git and other shell work. Every call is a fresh process, so \
         `cd` does not persist: use absolute paths or `cd dir && command`. Do not use it to read or \
         search files when the tools above can, and never to create or change files: no Python, \
         Node or Perl scripts, `sed -i`, heredocs or `>` redirection into files. Those changes go \
         through `edit` and `write`, which check that you read the file and show the user the \
         change; commands that edit files themselves are refused. Running the project's own build \
         steps, formatters and code generators is fine. Start dev servers, watchers and other \
         commands that keep running with `run_in_background`.",
    ),
    (
        "bash_output",
        "read what a background command printed since you last looked, and whether it still runs. \
         You are told when one exits by itself, so don't poll it in a loop.",
    ),
    ("wait_tasks", "wait for background commands or subagents when your next step needs their results. Pass task_ids together, with mode any or all. Timeout leaves tasks running; continue independent work first."),
    ("kill_shell", "stop a background command you no longer need, such as a dev server once you are done with it."),
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
         poll it; use wait_tasks if you need the result before continuing, and don't guess what it will say. Give subagents that change files \
         separate files to work on.",
    ),
    (
        "message_agent",
        "send a subagent from earlier in this conversation a follow-up, a correction or more work; it \
         keeps its context, so prefer this to starting a new one on the same ground.",
    ),
    ("stop_agent", "stop a subagent whose work is no longer needed."),
];

/// Claude Code tools the harness has no counterpart of.
const MISSING: &str = "`WebFetch`, `WebSearch`, `TodoWrite`, `NotebookEdit`, `AskUserQuestion`";

/// The main agent's system prompt, for the tools in `tools`.
pub fn main(tools: &ToolRegistry) -> String {
    let mut section = tools_section(tools);
    if tools.get("skill").is_some() || tools.get("agent").is_some() {
        section.push_str("\n\n");
        section.push_str(&claude_code_names(tools.names().collect()));
    }
    let prompt = TEMPLATE.replace("{{tools}}", &section).replace("{{environment}}", &environment());
    prompt + &user_notes() + &project_notes(&std::env::current_dir().unwrap_or_default())
}

/// The prompt `/init` sends: write or improve `AGENTS.md`, with what the
/// user added after the command.
pub fn init(args: &str) -> String {
    match args.trim() {
        "" => INIT.to_string(),
        args => format!("{INIT}\nAlso: {args}\n"),
    }
}

/// A subagent's system prompt: its definition's prompt plus the
/// environment and the project's `AGENTS.md`, and for a plugin's agent,
/// written for Claude Code, what its tools are called here. The built-in
/// `explore` only searches, and goes without the notes.
pub fn subagent(def: &AgentDef) -> String {
    let names = if def.name.contains(':') {
        let tools: Vec<&str> = crate::hooks::ALIASES.iter().map(|(name, _)| *name).collect();
        format!("\n\n{}", claude_code_names(tools))
    } else {
        String::new()
    };
    let notes = if def.path.is_none() && def.name == "explore" {
        String::new()
    } else {
        user_notes() + &project_notes(&crate::tools::workdir::get())
    };
    format!(
        "{}\n\nYou were started by another agent, which sees only your final message; the user does \
         not see it directly.{names}\n\n# Environment\n\n{}{notes}",
        def.prompt,
        environment()
    )
}

/// How the tool names Claude Code uses, which skills, commands and agents
/// written for it mention, map onto `tools`.
fn claude_code_names(tools: Vec<&str>) -> String {
    let pairs: Vec<String> = crate::hooks::ALIASES
        .iter()
        .filter(|(name, _)| tools.contains(name))
        .map(|(name, aliases)| {
            let aliases: Vec<String> = aliases.iter().map(|a| format!("`{a}`")).collect();
            format!("{} is `{name}`", aliases.join(" or "))
        })
        .collect();
    format!(
        "Skills, commands and agents written for Claude Code name its tools; here {}. {MISSING} have no \
         counterpart: do without them, and ask the user in your reply rather than through a tool.",
        pairs.join(", ")
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
    let cwd = crate::tools::workdir::get();
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
    let commands = crate::tools::bash::added_commands();
    if !commands.is_empty() {
        let shown: Vec<String> = commands.iter().take(30).map(|c| format!("`{c}`")).collect();
        let more = if commands.len() > 30 { format!(" and {} more", commands.len() - 30) } else { String::new() };
        lines.push(format!("- Commands the enabled plugins add, on the `bash` tool's PATH: {}{more}", shown.join(", ")));
    }
    if let Some(sandbox) = crate::sandbox::active() {
        lines.push(format!("- Sandbox: {}", sandbox.prompt_note()));
    }
    if let Some(date) = today() {
        lines.push(format!("- Today's date: {date}"));
    }
    lines.join("\n")
}

/// The section with `dir`'s `AGENTS.md`, empty when there is none (or it is
/// blank). Past [`NOTES_LIMIT`] it is cut at a line, and the model is told
/// to read the rest itself.
fn project_notes(dir: &Path) -> String {
    if crate::util::bare() {
        return String::new();
    }
    let path = dir.join("AGENTS.md");
    let Some(shown) = notes_text(&path, "`AGENTS.md`", NOTES_LIMIT) else { return String::new() };
    format!(
        "\n\n# Project notes\n\n`AGENTS.md` in the working directory holds this project's conventions and \
         commands, written for the agents that work on it. Follow it; the user's own instructions come \
         first.\n\n{shown}"
    )
}

/// The section with `~/.harness/AGENTS.md`, the user's notes for every
/// project, empty without one.
fn user_notes() -> String {
    if crate::util::bare() {
        return String::new();
    }
    let Some(path) = crate::util::harness_home().map(|dir| dir.join("AGENTS.md")) else { return String::new() };
    let Some(shown) = notes_text(&path, &format!("`{}`", path.display()), OTHER_NOTES_LIMIT) else {
        return String::new();
    };
    format!(
        "\n\n# The user's notes\n\n`~/.harness/AGENTS.md` holds the user's own instructions for every \
         project. Follow them; the user's messages come first, and the project's notes below may add to \
         them.\n\n{shown}"
    )
}

/// The notes of the subdirectory holding `path`, an `AGENTS.md` that
/// `read` shows the model with the first file it reads below it; `label`
/// is how the result names it.
pub fn directory_notes(path: &Path, label: &str) -> Option<String> {
    if crate::util::bare() {
        return None;
    }
    let shown = notes_text(path, &format!("`{label}`"), OTHER_NOTES_LIMIT)?;
    let dir = Path::new(label).parent().map(|d| d.display().to_string()).unwrap_or_default();
    Some(format!(
        "`{label}` holds notes for the files in `{dir}/`, written for the agents that work on them. Follow \
         them there; the user's own instructions come first.\n\n{shown}"
    ))
}

/// `path`'s text in `<agents-md>` tags, `None` when it is missing or
/// blank. Past `limit` bytes it is cut at a line, with a note to read the
/// rest; `label` names the file in that note.
fn notes_text(path: &Path, label: &str, limit: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    // Only the end is trimmed, so the line the model reads on from is right.
    let text = text.trim_end();
    if text.trim_start().is_empty() {
        return None;
    }
    let (shown, rest) = if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let end = text[..end].rfind('\n').unwrap_or(end);
        let line = text[..end].lines().count() + 1;
        (&text[..end], format!("\n\n[{label} goes on; read it with `read` from line {line} on.]"))
    } else {
        (text, String::new())
    };
    Some(format!("<agents-md>\n{shown}\n</agents-md>{rest}"))
}

/// The local date as `YYYY-MM-DD`.
fn today() -> Option<String> {
    let tm = local_time()?;
    Some(format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday))
}

/// The local time as `YYYY-MM-DD HH:MM:SS +HH:MM`.
pub fn timestamp() -> Option<String> {
    let tm = local_time()?;
    let offset = tm.tm_gmtoff / 60;
    let sign = if offset < 0 { '-' } else { '+' };
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {sign}{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        offset.abs() / 60,
        offset.abs() % 60
    ))
}

fn local_time() -> Option<libc::tm> {
    let now = libc::time_t::try_from(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs(),
    )
    .ok()?;
    // SAFETY: `tm` is plain data, and `localtime_r` only writes into it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return None;
        }
        Some(tm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FileTracker, GrepTool};

    #[test]
    fn fills_the_template() {
        let prompt = main(&ToolRegistry::with_defaults(FileTracker::default()));
        assert!(!prompt.contains("{{"), "{prompt}");
        for tool in ["read", "edit", "write", "grep", "glob", "bash"] {
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
    fn maps_claude_code_tool_names() {
        let mut tools = ToolRegistry::with_defaults(FileTracker::default());
        assert!(!main(&tools).contains("written for Claude Code"), "nothing written for it to load");
        tools.register(crate::skills::SkillTool::new(&[], crate::subagent::Forks::default()));
        let prompt = main(&tools);
        assert!(prompt.contains("`Bash` is `bash`") && prompt.contains("`Glob` or `LS` is `glob`"), "{prompt}");
        assert!(prompt.contains("`Skill` is `skill`") && !prompt.contains("`Task` is"), "only registered tools");
        assert!(prompt.contains("`WebFetch`"));
        let mut def = crate::subagent::explore();
        assert!(!subagent(&def).contains("Claude Code"));
        def.name = "p:x".into();
        assert!(subagent(&def).contains("`Agent` or `Task` is `agent`"));
    }

    #[test]
    fn carries_agents_md_when_present() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(project_notes(dir.path()), "");
        std::fs::write(dir.path().join("AGENTS.md"), " \n").unwrap();
        assert_eq!(project_notes(dir.path()), "", "a blank file adds nothing");
        std::fs::write(dir.path().join("AGENTS.md"), "# notes\n\nRun `make test`.\n").unwrap();
        let notes = project_notes(dir.path());
        assert!(notes.starts_with("\n\n# Project notes\n"), "{notes}");
        assert!(notes.contains("<agents-md>\n# notes\n\nRun `make test`.\n</agents-md>"), "{notes}");
        assert!(!notes.contains("goes on"), "{notes}");
    }

    #[test]
    fn cuts_a_long_agents_md_at_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let line = "é".repeat(99);
        let text: Vec<&str> = std::iter::repeat_n(line.as_str(), 300).collect();
        std::fs::write(dir.path().join("AGENTS.md"), text.join("\n")).unwrap();
        let notes = project_notes(dir.path());
        let shown = notes.split("<agents-md>\n").nth(1).unwrap().split("\n</agents-md>").next().unwrap();
        assert!(shown.len() <= NOTES_LIMIT && shown.ends_with(&line), "cut at a line");
        let lines = shown.lines().count();
        assert!(notes.ends_with(&format!("read it with `read` from line {} on.]", lines + 1)), "{notes}");
    }

    #[test]
    fn init_adds_what_the_user_asked() {
        assert_eq!(init(" "), INIT);
        let prompt = init(" focus on the tests ");
        assert!(prompt.starts_with(INIT) && prompt.ends_with("\nAlso: focus on the tests\n"), "{prompt}");
    }

    #[test]
    fn today_is_a_date() {
        let date = today().unwrap();
        assert_eq!(date.len(), 10, "{date}");
        assert!(date.starts_with("20"), "{date}");
    }

    #[test]
    fn timestamp_has_time_and_offset() {
        let stamp = timestamp().unwrap();
        assert_eq!(stamp.len(), 26, "{stamp}");
        assert!(stamp.starts_with("20"), "{stamp}");
        assert!(matches!(stamp.as_bytes()[20], b'+' | b'-'), "{stamp}");
    }
}
