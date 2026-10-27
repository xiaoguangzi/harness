//! Terminal frontend: the interactive REPL and one-shot mode.

mod actions;
mod approve;
mod calls;
mod clipboard;
mod commands;
mod dialog;
mod diff;
mod input;
pub mod json;
mod keys;
mod markdown;
mod menu;
mod model;
mod one_shot;
mod output;
mod parse;
mod print;
mod repl;
mod rewind;
mod screen;
mod sessions;
pub mod status_line;
mod style;
mod terminal;
mod text;
mod tool_output;
mod transcript;
mod turn;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};
use serde_json::Value;

use crate::agent::Agent;
use crate::hooks::{Event, Hooks};
use crate::llm;
use crate::mentions;
use crate::plugins;
use crate::session::{self, Recorder, Resume, Store};
use crate::setup::Env;
use crate::skills::Skill;
use crate::tools::FileTracker;
use clipboard::Image;
use commands::View;
use input::Typing;
use menu::Command;
pub use output::Output;
use parse::{Input, parse_input, render_skill};
use repl::ReplHelper;
use sessions::show_hook_notices;
use style::{BOLD, YELLOW};
use terminal::Terminal;
use turn::TurnInput;

/// How long the prompt waits after a turn before Notification hooks hear
/// that it does.
const IDLE_NOTICE: Duration = Duration::from_secs(60);

type Editor = rustyline::Editor<ReplHelper, rustyline::history::DefaultHistory>;

pub async fn run(agent: &mut Agent, args: Vec<String>, env: &mut Env, out: Output) -> Result<ExitCode> {
    let (resume, args) = session::parse_args(args)?;
    let (format, args) = json::parse_flag(args)?;
    if format != json::Format::Text {
        if args.is_empty() {
            anyhow::bail!("--output-format json and stream-json need a prompt to run");
        }
        // Only the JSON goes to stdout.
        out.set_silent(true);
    }
    out.spawn_spinner();
    out.set_permissions(agent.permissions().clone());
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let mut recorder = Store::default_location().map(|store| Recorder::new(store, cwd));
    if let Some(rec) = &recorder {
        sessions::attach(agent, rec);
    }
    let one_shot = !args.is_empty();

    let boxed = !one_shot && std::io::stdin().is_terminal() && out.interactive();
    let _screen = boxed.then(|| out.enter_screen());
    if boxed {
        agent.permissions().set_asker(Some(approve::TerminalAsker::new(out.clone())));
    }
    if !one_shot {
        print::banner(agent, &env.ext.skills, env, &out);
    }
    if let Some(resume) = resume {
        let Some(rec) = recorder.as_mut() else { anyhow::bail!("no home directory to find saved sessions in") };
        let id = match resume {
            Resume::Id(id) => Some(id),
            Resume::Latest => match rec.store().list(&rec.meta().cwd).first() {
                Some(latest) => Some(latest.id.clone()),
                None => anyhow::bail!("no saved conversation in this directory"),
            },
            Resume::Pick if one_shot => anyhow::bail!("--resume needs a session id in one-shot mode"),
            Resume::Pick => sessions::pick(rec, &out, boxed).await?,
        };
        if let Some(id) = id {
            sessions::resume(agent, rec, &id, &out, !one_shot).await?;
        }
    }
    if one_shot {
        return one_shot::run(agent, env, &args.join(" "), format, recorder, &out).await;
    }
    Repl::new(agent, env, out, recorder, boxed)?.run().await
}

/// The interactive REPL, and what its loop keeps from one line to the next.
struct Repl<'a> {
    agent: &'a mut Agent,
    env: &'a mut Env,
    out: Output,
    term: Terminal,
    recorder: Option<Recorder>,
    editor: Editor,
    typing: Typing,
    skills: Arc<Vec<Skill>>,
    files: FileTracker,
    /// Whether the input box is drawn; otherwise rustyline reads plain lines.
    boxed: bool,
    /// Whether Ctrl-C was pressed once at the prompt: again, it quits.
    interrupted: bool,
    /// Whether a turn ran since the prompt last waited a while.
    answered: bool,
}

/// What reading the prompt came to.
enum Read {
    Line(String, Vec<(usize, Image)>),
    /// Nothing to run: read again.
    Again,
    Quit,
}

impl<'a> Repl<'a> {
    fn new(agent: &'a mut Agent, env: &'a mut Env, out: Output, recorder: Option<Recorder>, boxed: bool) -> Result<Self> {
        let skills = env.ext.skills.clone();
        let config = rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .auto_add_history(true)
            // Otherwise a lone Esc waits for the next key, and Esc Esc is one.
            .keyseq_timeout(Some(50))
            .build();
        let mut editor = Editor::with_config(config)?;
        let helper = ReplHelper::new(menu_commands(agent, &skills));
        helper.bind_keys(&mut editor);
        editor.set_helper(Some(helper));
        // Alt+Enter inserts a newline, like a trailing `\`.
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
        if let Some(path) = history_path() {
            let _ = editor.load_history(&path);
        }
        let (term, files) = (Terminal::new(out.clone()), env.files.clone());
        let typing = Typing::default();
        Ok(Self { agent, env, out, term, recorder, editor, typing, skills, files, boxed, interrupted: false, answered: false })
    }

    async fn run(mut self) -> Result<ExitCode> {
        loop {
            // A background subagent reported back, or a message sent from the
            // input box is still on its way: the model answers first.
            if self.agent.inbox().wants_turn() {
                finish_mcp(self.agent, self.env, &self.out).await;
                self.run_turn(Vec::new(), None).await;
                continue;
            }
            match self.read().await? {
                Read::Line(line, images) => {
                    if !self.handle(&line, &images).await {
                        break;
                    }
                }
                Read::Again => {}
                Read::Quit => break,
            }
        }
        show_hook_notices(&self.out, self.agent.end_session("prompt_input_exit").await);
        if let Some(path) = history_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = self.editor.save_history(&path);
        }
        Ok(ExitCode::SUCCESS)
    }

    /// Reads the next line to run: a command sent from the input box while
    /// a turn ran, or what the user types at the prompt.
    async fn read(&mut self) -> Result<Read> {
        if let Some(command) = self.typing.next_command() {
            self.out.lines(&turn::prompt_lines(&self.out, &command));
            return Ok(Read::Line(command, Vec::new()));
        }
        let idle = std::mem::take(&mut self.answered).then(|| notify_when_idle(self.agent.hooks(), self.agent.model()));
        let read = if self.boxed { self.read_box().await } else { self.read_line() };
        if let Some(idle) = idle {
            idle.abort();
        }
        match read {
            Ok(read) => {
                self.interrupted = false;
                Ok(read)
            }
            Err(ReadlineError::Interrupted) if !self.interrupted => {
                self.interrupted = true;
                // The input box shows the background work already.
                let running = if self.boxed { 0 } else { self.agent.subagents().running_in_background() };
                let note = match running {
                    0 => String::new(),
                    1 => "; a subagent is still running".to_string(),
                    n => format!("; {n} subagents are still running"),
                };
                self.out.note(format!("(press Ctrl-C again or Ctrl-D to exit{note})"));
                Ok(Read::Again)
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => Ok(Read::Quit),
            Err(e) => Err(e.into()),
        }
    }

    /// Reads the input box; if the user is away long enough, the
    /// conversation is compacted meanwhile.
    async fn read_box(&mut self) -> Result<Read, ReadlineError> {
        let last = mentions::last_pasted_image(self.agent.messages()).max(self.typing.draft.last_image());
        self.typing.draft.note_image(last);
        let past = self.editor.history().iter().cloned().collect();
        let pending = self.agent.subagents().running_in_background() + self.agent.shells().pending();
        if pending > 0 {
            self.out.set_status(Some(&format!("Waiting for {pending} background tasks…")));
        }
        let inbox = self.agent.inbox().clone();
        let session = self.recorder.as_ref().map(|rec| (rec.meta().id.as_str(), rec.file()));
        status_line::refresh(self.agent, session, &self.out);
        let read = self.editor.helper().unwrap().read_box(&self.out, &mut self.typing, inbox, past);
        let read = match self.agent.idle_compaction_after() {
            Some(after) => compacting_while(read, after, self.agent, &self.out, self.recorder.as_mut()).await,
            None => read.await,
        };
        self.out.set_status(None);
        let Some((line, images)) = read? else { return Ok(Read::Again) };
        let line = repl::join_continuations(line.trim());
        if !line.is_empty() {
            let _ = self.editor.add_history_entry(&line);
            self.out.lines(&turn::prompt_lines(&self.out, &line));
        }
        Ok(Read::Line(line, images))
    }

    /// Reads a plain line with rustyline, without the input box.
    fn read_line(&mut self) -> Result<Read, ReadlineError> {
        let (before, after, carried) = self.typing.draft.take();
        let prompt = match self.editor.helper() {
            Some(helper) => {
                let last = mentions::last_pasted_image(self.agent.messages()).max(self.typing.draft.last_image());
                helper.new_line(last, carried);
                helper.prompt()
            }
            None => input::PROMPT.to_string(),
        };
        // Reading a line blocks; let the runtime move other tasks
        // (such as MCP connections) off this worker meanwhile.
        self.out.hold(true);
        let read = tokio::task::block_in_place(|| self.editor.readline_with_initial(&prompt, (&before, &after)));
        // What subagents printed meanwhile.
        self.out.hold(false);
        let line = read?;
        let Some(helper) = self.editor.helper() else { return Ok(Read::Again) };
        self.typing.draft.note_image(helper.last_image());
        let mut line = repl::join_continuations(line.trim());
        // Enter ran the command selected in the menu, rather than the
        // prefix typed.
        if let Some(command) = helper.take_accepted() {
            line = command;
        }
        Ok(Read::Line(line, helper.take_images()))
    }

    /// Runs `line`, read with the pasted `images`; false to quit.
    async fn handle(&mut self, line: &str, images: &[(usize, Image)]) -> bool {
        let input = render_skill(parse_input(line, &self.skills)).await;
        if !matches!(input, Input::Exit | Input::Empty) {
            finish_mcp(self.agent, self.env, &self.out).await;
        }
        // A command typed alone opens its panel.
        let panel = self.boxed && line.split_whitespace().count() == 1;
        let input = if panel {
            let view = View::of(self.agent);
            match commands::interactive(input, &view, self.env, &self.skills, &self.out, &mut self.typing).await {
                Some(input) => input,
                None => {
                    self.agent.switch_now();
                    return true;
                }
            }
        } else {
            input
        };
        match input {
            Input::Empty => {}
            Input::Exit => return false,
            Input::Clear => self.clear().await,
            Input::Compact(focus) => self.compact(focus.as_deref()).await,
            Input::Resume(id) => self.resume(id).await,
            Input::Rewind => self.rewind().await,
            Input::ToolOutput => self.out.show_tool_output(self.boxed),
            Input::Plugins { update: false } => print::plugins(&self.out, &self.env.ext.plugins),
            Input::Plugins { update: true } => print::update_plugins(&self.env.ext.plugins, &self.out).await,
            Input::ManagePlugins(args) => self.manage_plugins(&args),
            Input::ReloadPlugins => self.reload_plugins().await,
            Input::Shell(command) => actions::shell(self.agent, &command, &self.out).await,
            Input::Btw(question) => actions::side_question(self.agent, &question, &self.out).await,
            Input::Copy(n) => actions::copy(self.agent, n, &self.out),
            Input::Export(path) => {
                let session = self.recorder.as_ref().map(|rec| rec.meta().id.clone());
                actions::export(self.agent, path.as_deref(), session.as_deref(), &self.out);
            }
            Input::Usage if panel => commands::usage(self.agent, &self.out).await,
            Input::Usage => print::usage(&self.out, self.agent),
            Input::Providers(args) => self.manage_providers(&args),
            // A skill that runs in a subagent: its report goes to the model.
            Input::Skill(skill, args) => match turn::run_fork(self.agent, self.env, &skill, &args, &self.out).await {
                Ok(prompt) => self.prompt(line, &prompt, images).await,
                Err(e) => self.out.error(&e),
            },
            Input::Prompt(prompt) => self.prompt(line, &prompt, images).await,
            input => {
                commands::run(input, &View::of(self.agent), self.env, &self.skills, &self.out).await;
                self.agent.switch_now();
            }
        }
        true
    }

    /// Runs the turn for `prompt`, what `line`, typed with `images`,
    /// expanded to.
    async fn prompt(&mut self, line: &str, prompt: &str, images: &[(usize, Image)]) {
        let attached = turn::attachments(line, images, &self.files);
        let (content, title) = turn::prompt_content(self.agent, line, prompt, attached, &self.out);
        self.run_turn(content, title).await;
    }

    /// Runs a turn with the input box under it (see [`turn::run_shown`]),
    /// then adds the lines sent from the box to the line editor's history.
    /// The turn's outcome is already shown.
    async fn run_turn(&mut self, content: Vec<Value>, title: Option<&str>) {
        let menu = self.editor.helper().map(ReplHelper::menu);
        let (files, env, skills) = (&self.files, &*self.env, &self.skills);
        let input = self.boxed.then(|| TurnInput { typing: &mut self.typing, files, env, skills, menu });
        let recorder = self.recorder.as_mut();
        let _ = turn::run_shown(self.agent, content, title, &self.out, &mut self.term, recorder, input).await;
        for line in self.typing.sent.drain(..) {
            let _ = self.editor.add_history_entry(line);
        }
        self.out.line("");
        self.answered = true;
    }

    /// `/clear`: starts a new conversation, with what `/reload-plugins`
    /// loaded.
    async fn clear(&mut self) {
        show_hook_notices(&self.out, self.agent.end_session("clear").await);
        self.agent.clear();
        self.out.restore_tool_output(self.agent.messages());
        if apply_reload(self.agent, self.env, &mut self.skills, self.editor.helper()).await {
            self.out.note("the reloaded plugins apply from here");
        } else {
            refresh_system(self.agent);
        }
        if let Some(rec) = self.recorder.as_mut() {
            rec.restart();
            sessions::attach(self.agent, rec);
        }
        self.out.note("conversation cleared");
    }

    async fn compact(&mut self, focus: Option<&str>) {
        if self.agent.messages().is_empty() {
            self.out.note("nothing to compact yet");
            return;
        }
        let _ = turn::compact_shown(self.agent, focus, &self.out, &mut self.term, self.recorder.as_mut()).await;
        self.out.line("");
    }

    /// `/resume [id]`: replaces the conversation with a saved one.
    async fn resume(&mut self, id: Option<String>) {
        let Some(rec) = self.recorder.as_mut() else {
            self.out.error("no home directory to find saved sessions in");
            return;
        };
        let id = match id {
            Some(id) => Ok(Some(id)),
            None => sessions::pick(rec, &self.out, self.boxed).await,
        };
        let resumed = match id {
            Ok(Some(id)) => {
                if self.env.has_pending() {
                    show_hook_notices(&self.out, self.agent.end_session("resume").await);
                    apply_reload(self.agent, self.env, &mut self.skills, self.editor.helper()).await;
                } else {
                    refresh_system(self.agent);
                }
                sessions::resume(self.agent, rec, &id, &self.out, true).await
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = resumed {
            self.out.error(format!("{e:#}"));
        }
    }

    async fn rewind(&mut self) {
        match rewind::run(self.agent, &self.out, &mut self.typing, self.boxed).await {
            Ok(true) => {
                self.out.restore_tool_output(self.agent.messages());
                if let Some(rec) = self.recorder.as_mut() {
                    sessions::save(self.agent, rec, &self.out);
                }
            }
            Ok(false) => {}
            Err(e) => self.out.error(format!("{e:#}")),
        }
    }

    /// `/plugins install …` and the like, as `harness plugin` takes them.
    fn manage_plugins(&self, args: &[String]) {
        match on_terminal(&self.out, || plugins::manage::run(args, &mut plugins::manage::TerminalAsk)) {
            Ok(lines) => self.out.lines(&lines),
            Err(e) => self.out.error(format!("{e:#}")),
        }
        let lists = matches!(args.first().map(String::as_str), Some("list" | "help") | None)
            || matches!(args.get(..2), Some([a, b]) if a == "marketplace" && b == "list");
        if !lists {
            self.out.note("run /reload-plugins to use the change");
        }
    }

    /// `/reload-plugins`: loads them again, for this conversation if it is
    /// still empty, otherwise for the next.
    async fn reload_plugins(&mut self) {
        let lines = match self.env.reload().await {
            Ok(lines) => lines,
            Err(e) => {
                self.out.error(format!("{e:#}"));
                return;
            }
        };
        for (i, line) in lines.iter().enumerate() {
            self.out.line(if i == 0 { line.clone() } else { self.out.paint(YELLOW, line) });
        }
        if self.agent.messages().is_empty() {
            apply_reload(self.agent, self.env, &mut self.skills, self.editor.helper()).await;
            if let Some(rec) = self.recorder.as_ref() {
                sessions::attach(self.agent, rec);
            }
            self.out.note("they apply from the next message");
        } else {
            self.out.note("they apply to the next conversation: /clear starts one");
        }
    }

    /// `/provider …`, as `harness provider` takes it; a change moves the
    /// conversation to the providers as they are now.
    fn manage_providers(&mut self, args: &[String]) {
        match on_terminal(&self.out, || llm::manage::run(args, &mut plugins::manage::TerminalAsk)) {
            Ok(lines) => {
                self.out.lines(&lines);
                if llm::manage::changes(args) {
                    reload_providers(self.agent, &self.skills, self.editor.helper(), &self.out);
                }
            }
            Err(e) => self.out.error(format!("{e:#}")),
        }
    }
}

/// Runs `manage`, which asks its questions on the terminal, with the screen
/// handed over to it.
fn on_terminal(out: &Output, manage: impl FnOnce() -> Result<Vec<String>>) -> Result<Vec<String>> {
    let handoff = out.handoff();
    let lines = tokio::task::block_in_place(manage);
    drop(handoff);
    lines
}

/// What the menu under the prompt offers.
fn menu_commands(agent: &Agent, skills: &[Skill]) -> Vec<Command> {
    let builtins = parse::BUILTINS.iter().map(|(name, description)| Command::new(*name, *description));
    let mut commands: Vec<Command> = builtins.collect();
    commands.extend(
        skills.iter().filter(|s| s.user_invocable).map(|s| Command::new(format!("/{}", s.name), &s.description)),
    );
    commands.extend(
        agent.client().providers().listed_models().iter().map(|m| Command::new(format!("/model {m}"), "switch to this model")),
    );
    let efforts = llm::Effort::ALL.iter();
    commands.extend(efforts.map(|e| Command::new(format!("/effort {}", e.as_str()), parse::effort_help(*e))));
    commands.push(Command::new("/effort auto", "back to the configured effort"));
    commands
}

/// Gives the agent, whose conversation was just cleared or is about to be
/// replaced, what `/reload-plugins` loaded: its tools, system prompt and
/// hooks, and the prompt its commands. False if nothing was reloaded.
async fn apply_reload(agent: &mut Agent, env: &mut Env, skills: &mut Arc<Vec<Skill>>, helper: Option<&ReplHelper>) -> bool {
    if !env.apply() {
        return false;
    }
    let (tools, system) = env.conversation(agent.client());
    agent.set_tools(tools);
    agent.set_system(system);
    agent.set_hooks(Hooks::with_config(env.ext.hooks.clone()));
    *skills = env.ext.skills.clone();
    if let Some(helper) = helper {
        helper.set_commands(menu_commands(agent, skills));
    }
    env.shutdown_retired().await;
    true
}

/// Moves the agent to the providers as the settings have them now, on the
/// same model if its provider is still there.
fn reload_providers(agent: &mut Agent, skills: &[Skill], helper: Option<&ReplHelper>, out: &Output) {
    let settings = crate::settings::Settings::load();
    if let Ok(settings) = &settings {
        crate::tools::bash::hide_env(settings.credential_vars());
    }
    match settings.and_then(|s| llm::Providers::from_settings(&s)) {
        Ok(providers) => {
            let client = agent.client().with_providers(providers);
            if client.spec() != agent.model() {
                out.line(format!("model: {}", out.paint(BOLD, client.spec())));
            }
            agent.set_client(client);
            if let Some(helper) = helper {
                helper.set_commands(menu_commands(agent, skills));
            }
        }
        Err(_) => out.note("this conversation keeps the providers it had"),
    }
}

/// Builds the system prompt again for a new conversation, so it has the
/// `AGENTS.md` as it is now (`/init` may have written it).
fn refresh_system(agent: &mut Agent) {
    let system = crate::prompt::main(agent.tools());
    agent.set_system(system);
}

/// Waits for the MCP servers still connecting at startup, then gives the
/// agent their tools (before its first request, so the prompt cache's
/// prefix holds from there).
async fn finish_mcp(agent: &mut Agent, env: &mut Env, out: &Output) {
    let Some(count) = env.mcp_connecting() else { return };
    out.set_status(Some(&format!("Connecting to {count} MCP {}…", text::plural(count, "server"))));
    let warnings = env.wait_mcp().await;
    out.set_status(None);
    for w in warnings.into_iter().chain(env.unknown_agent_tools(agent.client())) {
        out.warn(&w);
    }
    let (tools, system) = env.conversation(agent.client());
    agent.set_tools(tools);
    agent.set_system(system);
}

/// Waits for `read`, the input box; if the user is away for `after`, the
/// conversation is compacted meanwhile. A message sent before that is done
/// cancels it, which leaves the conversation as it was.
async fn compacting_while<T>(
    read: impl Future<Output = T>,
    after: Duration,
    agent: &mut Agent,
    out: &Output,
    recorder: Option<&mut Recorder>,
) -> T {
    let compact = async {
        tokio::time::sleep(after).await;
        let mut observer = terminal::IdleCompaction::new(out);
        match agent.compact_when_idle(&mut observer).await {
            Ok(()) => {
                if let Some(rec) = recorder {
                    sessions::save(agent, rec, out);
                }
            }
            Err(e) => out.error(format!("could not compact the idle conversation: {e:#}")),
        }
    };
    tokio::pin!(read, compact);
    let mut compacting = true;
    loop {
        tokio::select! {
            result = &mut read => return result,
            () = &mut compact, if compacting => compacting = false,
        }
    }
}

/// Runs the Notification hooks if the prompt still waits after
/// [`IDLE_NOTICE`]; abort the task once a line is read.
fn notify_when_idle(hooks: &Hooks, model: &str) -> tokio::task::JoinHandle<()> {
    let (hooks, model) = (hooks.clone(), model.to_string());
    tokio::spawn(async move {
        if !hooks.has(Event::Notification) {
            return;
        }
        tokio::time::sleep(IDLE_NOTICE).await;
        // On a task of its own, which reading the line does not cut short.
        tokio::spawn(async move {
            hooks.notification("idle_prompt", "harness is waiting for your input", &model).await;
        });
    })
}

fn history_path() -> Option<std::path::PathBuf> {
    crate::util::harness_home().map(|dir| dir.join("history"))
}
