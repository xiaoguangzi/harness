//! What a line typed at the prompt means: the built-in commands, and the
//! [`Input`] a line parses into.

use crate::llm;
use crate::skills::Skill;

/// Built-in commands and what they do, as the menu and `/help` show them.
pub(super) const BUILTINS: [(&str, &str); 25] = [
    ("/help", "browse commands; Enter inserts one into the prompt"),
    ("/model", "choose main/subagent models and effort; /model <provider>/<model> switches directly"),
    ("/effort", "choose reasoning effort, or set it with /effort <level>"),
    ("/provider", "manage providers and models; /provider add|remove|model|default … changes them"),
    ("/clear", "start a new conversation"),
    ("/compact", "summarize the conversation to free context; /compact <focus> says what to keep"),
    ("/resume", "pick up an earlier conversation from this directory"),
    ("/rewind", "go back to before an earlier prompt, with the code, the conversation or both (or Esc Esc)"),
    ("/init", "have the model write AGENTS.md, or improve it; /init <notes> says what else to cover"),
    ("/usage", "show the tokens used so far and how full the context is"),
    ("/btw", "ask a side question about the conversation; neither it nor the answer is kept"),
    ("/copy", "copy the model's last reply to the clipboard; /copy <n> the nth latest"),
    ("/export", "write the conversation to a Markdown file; /export <file> names it"),
    ("/tools", "browse tool descriptions and input schemas"),
    ("/output", "browse the expanded conversation (or Ctrl+O, including while running)"),
    ("/skills", "browse skills; Enter inserts an invocation into the prompt"),
    ("/agents", "browse subagent types, models, permissions and tools"),
    ("/tasks", "browse this conversation's tasks and task actions"),
    ("/stop", "choose a running task to stop, or stop one with /stop <id>"),
    ("/mcp", "browse MCP servers, tools and connection errors"),
    ("/plugins", "manage plugins; /plugins install|uninstall|enable|disable|update|marketplace … also works"),
    ("/reload-plugins", "load plugins, skills, agents and hooks again, for the next conversation"),
    ("/sandbox", "show what the sandbox allows commands to do"),
    ("/permissions", "choose the permission mode (or Shift+Tab); /permissions <mode> sets it, /permissions rules lists the rules"),
    ("/exit", "quit (or exit, Ctrl-D)"),
];

/// Keys and input forms, as `/help` lists them after the commands.
pub(super) fn help_keys() -> [(&'static str, &'static str); 10] {
    [
        ("/<skill> [args]", "run a skill"),
        ("@path", "attach a file, directory, image or PDF; @ opens a file menu (@file#L10-20 for lines)"),
        ("Ctrl+O", "expanded transcript; / searches, i shows inputs, Esc closes"),
        ("Ctrl+V", "paste an image from the clipboard (files copied in Finder or a file manager become @ mentions)"),
        ("\\ or Alt+Enter", "continue on a new line"),
        ("↑ ↓ Tab Enter", "pick from the menu shown while typing / or @"),
        ("↑", "while the model works: edit the messages queued for it"),
        ("Esc", "interrupt the running turn"),
        ("Esc Esc", "at an empty prompt: go back to before an earlier prompt (/rewind)"),
        ("Ctrl-C", "interrupt the running turn; twice at the prompt to quit"),
    ]
}

pub(super) fn effort_help(effort: llm::Effort) -> &'static str {
    match effort {
        llm::Effort::Low => "fastest and cheapest; simple tasks",
        llm::Effort::Medium => "balanced speed and depth",
        llm::Effort::High => "deeper thinking",
        llm::Effort::XHigh => "deeper still; suits most coding work",
        llm::Effort::Max => "deepest; when correctness matters most",
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Input {
    Empty,
    Exit,
    Help,
    Clear,
    /// `/compact [focus]` summarizes the conversation.
    Compact(Option<String>),
    ListSkills,
    ListTools,
    ToolOutput,
    ListMcp,
    ListAgents,
    /// `/tasks` lists this conversation's subagents.
    Tasks,
    /// `/stop` stops every running subagent; `/stop <id>` that one.
    Stop(Option<String>),
    Sandbox,
    /// `/usage` shows the tokens used and the context's size.
    Usage,
    /// `/model` shows the current model; `/model <spec>` switches.
    Model(Option<String>),
    /// `/resume` lists saved conversations to pick from; `/resume <id>`
    /// resumes that one.
    Resume(Option<String>),
    /// `/rewind` goes back to before an earlier prompt.
    Rewind,
    /// `/provider …`, as `harness provider` takes it.
    Providers(Vec<String>),
    /// `/effort` shows the effort; `/effort <level>` sets it for this
    /// session, and `/effort auto` restores the configured one.
    Effort(Option<Option<llm::Effort>>),
    /// `/plugins` lists them; `/plugins update` fetches their latest.
    Plugins { update: bool },
    /// `/plugins install …` and the like, as `harness plugin` takes them.
    ManagePlugins(Vec<String>),
    /// `/reload-plugins`.
    ReloadPlugins,
    /// `/permissions` chooses the mode, `/permissions <mode>` sets it,
    /// `/permissions rules` lists the rules.
    Permissions(Option<String>),
    /// `!command` runs it for the user.
    Shell(String),
    /// `/btw <question>` asks a side question.
    Btw(String),
    /// `/copy [n]` copies the nth latest reply.
    Copy(usize),
    /// `/export [file]` writes the conversation as Markdown.
    Export(Option<String>),
    /// `/<skill> args`, before its text is read.
    Skill(Box<Skill>, String),
    Prompt(String),
    Error(String),
}

/// Reads the skill a `/<skill> args` line runs into the prompt; one that
/// runs in a subagent, and other input, passes through.
pub(super) async fn render_skill(input: Input) -> Input {
    match input {
        Input::Skill(skill, args) if skill.fork.is_none() => match skill.invocation_prompt(&args).await {
            Ok(prompt) => Input::Prompt(prompt),
            Err(e) => Input::Error(e),
        },
        input => input,
    }
}

/// Interprets a line of user input. `/<skill> args` is to load the skill's
/// instructions directly into the prompt, saving the model a tool call.
pub(super) fn parse_input(line: &str, skills: &[Skill]) -> Input {
    if let Some(command) = line.strip_prefix('!').map(str::trim).filter(|c| !c.is_empty()) {
        return Input::Shell(command.to_string());
    }
    let Some(command) = line.strip_prefix('/') else {
        return match line {
            "" => Input::Empty,
            // A bare `exit` or `quit` alone on the line quits too.
            "exit" | "quit" => Input::Exit,
            _ => Input::Prompt(line.to_string()),
        };
    };
    let (name, args) = command.split_once(char::is_whitespace).unwrap_or((command, ""));
    // The argument of a command that takes at most one.
    let arg = Some(args.trim()).filter(|a| !a.is_empty()).map(String::from);
    let words = || args.split_whitespace().map(String::from).collect();
    match name {
        "exit" | "quit" => Input::Exit,
        "help" => Input::Help,
        "clear" => Input::Clear,
        "compact" => Input::Compact(arg),
        "resume" => Input::Resume(arg),
        "rewind" => Input::Rewind,
        "init" => Input::Prompt(crate::prompt::init(args)),
        "skills" => Input::ListSkills,
        "tools" => Input::ListTools,
        "output" => Input::ToolOutput,
        "mcp" => Input::ListMcp,
        "plugins" | "plugin" => match args.trim() {
            "" | "list" => Input::Plugins { update: false },
            "update" => Input::Plugins { update: true },
            _ => Input::ManagePlugins(words()),
        },
        "reload-plugins" => Input::ReloadPlugins,
        "btw" => Input::Btw(args.trim().to_string()),
        "permissions" => Input::Permissions(arg),
        "copy" => match args.trim() {
            "" => Input::Copy(1),
            n => match n.parse::<usize>() {
                Ok(n) if n > 0 => Input::Copy(n),
                _ => Input::Error(format!("/copy takes the number of a reply, counting back from 1, not `{n}`")),
            },
        },
        "export" => Input::Export(arg),
        "agents" => Input::ListAgents,
        "tasks" => Input::Tasks,
        "stop" => Input::Stop(arg),
        "sandbox" => Input::Sandbox,
        "usage" => Input::Usage,
        "model" => Input::Model(arg),
        "provider" | "providers" => Input::Providers(words()),
        "effort" => match args.trim() {
            "" => Input::Effort(None),
            "auto" | "default" => Input::Effort(Some(None)),
            level => match llm::Effort::parse(level) {
                Ok(e) => Input::Effort(Some(Some(e))),
                Err(e) => Input::Error(e.to_string()),
            },
        },
        _ => match skills.iter().find(|s| s.name == name && s.user_invocable) {
            Some(skill) => Input::Skill(Box::new(skill.clone()), args.to_string()),
            None => Input::Error(format!("unknown command /{name} (try /help)")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills;

    #[tokio::test]
    async fn parses_commands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("greet")).unwrap();
        std::fs::write(dir.path().join("greet/SKILL.md"), "---\ndescription: d\n---\nSay hi.\n").unwrap();
        let (skills, _) = skills::discover(&[dir.path().into()]);

        assert_eq!(parse_input("", &skills), Input::Empty);
        assert_eq!(parse_input("/exit", &skills), Input::Exit);
        assert_eq!(parse_input("exit", &skills), Input::Exit);
        assert_eq!(parse_input("quit", &skills), Input::Exit);
        assert_eq!(parse_input("exit now", &skills), Input::Prompt("exit now".into()));
        assert_eq!(parse_input("how do I exit vim", &skills), Input::Prompt("how do I exit vim".into()));
        assert_eq!(parse_input("/clear", &skills), Input::Clear);
        assert_eq!(parse_input("/compact", &skills), Input::Compact(None));
        assert_eq!(parse_input("/init", &skills), Input::Prompt(crate::prompt::init("")));
        assert_eq!(parse_input("/init cover the web UI", &skills), Input::Prompt(crate::prompt::init("cover the web UI")));
        assert_eq!(parse_input("/compact keep the API notes", &skills), Input::Compact(Some("keep the API notes".into())));
        assert_eq!(parse_input("/tools", &skills), Input::ListTools);
        assert_eq!(parse_input("/output", &skills), Input::ToolOutput);
        assert_eq!(parse_input("/tasks", &skills), Input::Tasks);
        assert_eq!(parse_input("/usage", &skills), Input::Usage);
        assert_eq!(parse_input("/stop", &skills), Input::Stop(None));
        assert_eq!(parse_input("/stop a3f9c2", &skills), Input::Stop(Some("a3f9c2".into())));
        assert_eq!(parse_input("/resume", &skills), Input::Resume(None));
        assert_eq!(parse_input("/resume 2026 ", &skills), Input::Resume(Some("2026".into())));
        assert_eq!(parse_input("fix it", &skills), Input::Prompt("fix it".into()));
        assert!(matches!(parse_input("/nope", &skills), Input::Error(_)));
        assert_eq!(parse_input("/effort", &skills), Input::Effort(None));
        assert_eq!(parse_input("/provider", &skills), Input::Providers(Vec::new()));
        assert_eq!(parse_input("/providers model add gw/m", &skills), Input::Providers(vec!["model".into(), "add".into(), "gw/m".into()]));
        assert_eq!(parse_input("/effort XHigh", &skills), Input::Effort(Some(Some(llm::Effort::XHigh))));
        assert_eq!(parse_input("/effort auto", &skills), Input::Effort(Some(None)));
        assert!(matches!(parse_input("/effort huge", &skills), Input::Error(_)));
        assert_eq!(parse_input("/plugins", &skills), Input::Plugins { update: false });
        assert_eq!(parse_input("/plugins update", &skills), Input::Plugins { update: true });
        assert_eq!(parse_input("/plugins remove x@m", &skills), Input::ManagePlugins(vec!["remove".into(), "x@m".into()]));
        assert_eq!(parse_input("/reload-plugins", &skills), Input::ReloadPlugins);
        match render_skill(parse_input("/greet  Bob ", &skills)).await {
            Input::Prompt(p) => {
                assert!(p.starts_with("The user invoked the `greet` skill:"));
                assert!(p.ends_with("Say hi.\n\nARGUMENTS: Bob"), "{p}");
            }
            other => panic!("{other:?}"),
        }
    }
}
