//! Commands that leave the conversation as it is: what they show, line by
//! line or in a panel, and running them beside a turn.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use super::dialog::{self, Item, Keys, List, Mark, Sheet};
use super::input::Typing;
use super::keys::Key;
use super::parse::{BUILTINS, Input, effort_help};
use super::{Output, output, print, text};
use super::style::{BOLD, DIM};
use crate::agent::Agent;
use crate::extensions::skills::Skill;
use crate::llm::{self, Client};
use crate::security::permissions::{Mode, Permissions};
use crate::subagent::Subagents;
use crate::tools::{ToolRegistry, shells::Shells};
use crate::{settings::Settings, setup::Env};

/// What commands see of a conversation, and change in it, without holding
/// its `Agent`: a turn may be running with it.
#[derive(Clone)]
pub struct View {
    client: Client,
    switch_to: Arc<Mutex<Option<Client>>>,
    pub permissions: Arc<Permissions>,
    pub tools: ToolRegistry,
    pub subagents: Arc<Subagents>,
    pub shells: Arc<Shells>,
    /// Whether a turn runs: a model chosen meanwhile is used from the next.
    pub running: bool,
}

impl View {
    pub fn of(agent: &Agent) -> Self {
        Self {
            client: agent.client().clone(),
            switch_to: agent.switch_to().clone(),
            permissions: agent.permissions().clone(),
            tools: agent.tools().clone(),
            subagents: agent.subagents().clone(),
            shells: agent.shells().clone(),
            running: false,
        }
    }

    /// The model in use, or the one chosen to switch to.
    pub fn client(&self) -> Client {
        self.switch_to.lock().unwrap().clone().unwrap_or_else(|| self.client.clone())
    }

    /// Has the conversation go on with `client`; while a turn runs, from
    /// the next one.
    pub fn set_client(&self, client: Client) {
        *self.switch_to.lock().unwrap() = Some(client);
    }

    /// When a model or effort set now is used, to say after it: a turn
    /// that runs goes on as it began.
    fn later(&self) -> &'static str {
        if self.running { ", from the next message" } else { "" }
    }
}

/// Whether `input` leaves the conversation as it is, so that typed while a
/// turn runs it runs at once, beside the turn, as in Claude Code. Other
/// commands wait for the turn to end.
pub(super) fn runs_during_turn(input: &Input) -> bool {
    matches!(
        input,
        Input::Help
            | Input::ListSkills
            | Input::ListTools
            | Input::ListMcp
            | Input::ListAgents
            | Input::Tasks
            | Input::Stop(_)
            | Input::Sandbox
            | Input::Model(_)
            | Input::Advisor(_)
            | Input::Effort(_)
            | Input::Permissions(_)
            | Input::Error(_)
    )
}

/// Runs a command that [`runs_during_turn`], once its panel, if any, is
/// done. A model chosen goes to `view`, for the agent to switch to.
pub(super) async fn run(input: Input, view: &View, env: &Env, skills: &[Skill], out: &Output) {
    let bold = |text: &str| out.paint(BOLD, text);
    match input {
        Input::Help => print::help(out, skills),
        Input::ListSkills => print::skills(out, skills),
        Input::ListTools => print::tools(out, &view.tools),
        Input::ListMcp => print::mcp(out, env.mcp_statuses()),
        Input::ListAgents => print::agents(out, &env.ext.agent_defs),
        Input::Tasks => print::tasks(out, &view.subagents.list(), &view.shells.list()),
        Input::Stop(None) => match (view.subagents.stop_all(), view.shells.stop_all()) {
            (0, 0) => print::done(out, "No running tasks"),
            (stopped, killed) => {
                let stopped = format!("{stopped} {}", text::plural(stopped, "subagent"));
                print::done(out, format!("Stopped {stopped} and {killed} {}", text::plural(killed, "background command")));
            }
        },
        Input::Stop(Some(id)) if id.starts_with("shell_") => match view.shells.stop(&id) {
            Ok(true) => print::done(out, format!("Stopped {}", bold(&id))),
            Ok(false) => print::done(out, format!("{id} is not running")),
            Err(e) => print::failed(out, &e),
        },
        Input::Stop(Some(id)) => match view.subagents.stop(&id, true) {
            Ok(true) => print::done(out, format!("Stopped {}", bold(&id))),
            Ok(false) => print::done(out, format!("{id} is not running")),
            Err(e) => print::failed(out, format!("{e:#}")),
        },
        Input::Sandbox => print::sandbox(out),
        Input::Model(None) => print::models(out, &view.client()),
        Input::Model(Some(spec)) => match view.client().switch(&spec) {
            Ok(client) => {
                print::done(out, format!("Model set to {}{}", bold(client.spec()), view.later()));
                view.set_client(client);
            }
            Err(e) => print::failed(out, format!("{e:#}")),
        },
        Input::Advisor(None) => print::advisor(out, &view.client()),
        Input::Advisor(Some(name)) => {
            let mut client = view.client();
            client.set_advisor(Some(&name));
            let model = client.advisor_model().map(String::from);
            let mut said = model.as_ref().map_or("Advisor turned off".to_string(), |model| format!("Advisor set to {}", bold(model)));
            said.push_str(view.later());
            if let llm::AdvisorState::Unused { reason, .. } = client.advisor_state() {
                said.push_str(&out.paint(DIM, &format!(" · not used with {}: {reason}", client.spec())));
            }
            print::done(out, said);
            if let Err(e) = save_advisor(model.as_deref()) {
                print::failed(out, format!("Set for this session only: {e:#}"));
            }
            view.set_client(client);
        }
        Input::Permissions(None) => print::permissions(&view.permissions, out),
        Input::Permissions(Some(arg)) if arg == "rules" => print::permissions(&view.permissions, out),
        Input::Permissions(Some(arg)) => match Mode::parse(&arg) {
            Ok(mode) => {
                view.permissions.set_mode(mode);
                let about = out.paint(DIM, &format!("· {}", mode.describe()));
                print::done(out, format!("Permission mode set to {} {about}", bold(mode.as_str())));
            }
            Err(e) => print::failed(out, &e),
        },
        Input::Effort(None) => {
            let how = out.paint(DIM, "· /effort low|medium|high|xhigh|max sets it, /effort auto restores the configured one");
            print::done(out, format!("Effort is {} {how}", bold(view.client().effort().as_str())));
        }
        Input::Effort(Some(level)) => {
            let mut client = view.client();
            client.set_effort(level);
            let configured = if level.is_none() { ", as configured" } else { "" };
            print::done(out, format!("Effort set to {}{configured}{}", bold(client.effort().as_str()), view.later()));
            view.set_client(client);
        }
        Input::Error(e) => print::failed(out, &e),
        _ => unreachable!("only commands that leave the conversation as it is run here"),
    }
}

/// Runs `input`, typed while a turn runs, beside the turn: through its
/// panel if `panel`, which takes the keys from the turn's key reader.
/// What the panel types goes into the box under the turn.
pub(super) async fn during_turn(input: Input, panel: bool, view: View, env: &Env, skills: &[Skill], out: Output) {
    let mut typing = Typing::default();
    let input = if panel { interactive(input, &view, env, skills, &out, &mut typing).await } else { Some(input) };
    if let Some(input) = input {
        run(input, &view, env, skills, &out).await;
    }
    if !typing.draft.is_empty() {
        out.edit_input(|t| t.draft.set_text(typing.draft.text()));
    }
}

const SELECT: Keys = Keys { enter: "open", esc: "close" };
const BACK: Keys = Keys { enter: "select", esc: "back" };

/// Opens the panel of a command typed alone. Returns what to run once it
/// closes (a choice made in it), or `None` when it handled the command or
/// was cancelled. With arguments, commands skip their panels.
pub(super) async fn interactive(
    input: Input,
    view: &View,
    env: &Env,
    skills: &[Skill],
    out: &Output,
    typing: &mut Typing,
) -> Option<Input> {
    match input {
        Input::Help => help(skills, out, typing).await,
        Input::Permissions(None) => choose_mode(view, out).await,
        Input::Model(None) => choose_model(view, out).await,
        Input::Effort(None) => choose_effort(view, out).await,
        Input::ListSkills if skills.is_empty() => Some(input),
        Input::Advisor(None) => choose_advisor(view, out).await,
        Input::ListSkills => pick_skill(skills, out, typing).await,
        Input::ListTools => browse(out, "Tools", "No tools", tool_items(view)).await,
        Input::ListAgents => browse(out, "Subagent types", "No subagent types", agent_items(view, env)).await,
        Input::ListMcp => browse(out, "MCP servers", print::NO_MCP, mcp_items(env)).await,
        Input::Sandbox => {
            dialog::sheet(out, "Sandbox", print::sandbox_lines(out, dialog::room())).await;
            None
        }
        Input::Stop(None) => stop(view, out).await,
        Input::Tasks => tasks(view, out).await,
        Input::Providers(args) if args.is_empty() => providers(view, out).await,
        Input::Plugins { update: false } => plugins(env, out).await,
        input => Some(input),
    }
}

/// Shows `items` in a panel to look through; nothing runs after it.
async fn browse(out: &Output, title: &str, empty: &str, items: Vec<Item>) -> Option<Input> {
    dialog::inspect(out, title, "", empty, items).await;
    None
}

/// `/help`: the commands, to put one in the prompt, and beside them the
/// keys.
struct Help {
    commands: List,
    keys: Sheet,
    /// Whether the keys show.
    shortcuts: bool,
}

impl dialog::Panel for Help {
    fn render(&self, width: usize, height: usize, paint: dialog::Paint) -> dialog::Frame {
        if self.shortcuts { self.keys.render(width, height, paint) } else { self.commands.render(width, height, paint) }
    }
}

impl Help {
    /// Tab goes from the commands to the keys and back; other keys are
    /// for the one that shows.
    fn handle(&mut self, key: Key) -> Option<Option<usize>> {
        match key {
            Key::Tab => {
                self.shortcuts = !self.shortcuts;
                None
            }
            key if self.shortcuts => self.keys.handle(key).map(|_| None),
            key => self.commands.handle(key),
        }
    }
}

async fn help(skills: &[Skill], out: &Output, typing: &mut Typing) -> Option<Input> {
    let mut items: Vec<_> = BUILTINS.iter().map(|b| Item::new(b.name, b.about).hint(b.args)).collect();
    let skills = skills.iter().filter(|s| s.user_invocable);
    items.extend(skills.map(|s| Item::new(format!("/{}", s.name), &s.description)));
    let tabs = ["Commands", "Shortcuts"];
    let keys = Keys { enter: "insert", esc: "close" };
    let panel = Help {
        commands: List::new("Help", "", keys, items.clone(), 0).tabs(&tabs, 0),
        keys: Sheet::new("Help", print::key_lines(out)).tabs(&tabs, 1),
        shortcuts: false,
    };
    let picked = dialog::run(out, panel, Help::handle);
    let item = &items[picked.await?];
    // One that takes arguments is left ready for them.
    typing.draft.set_text(&format!("{}{}", item.label, if item.hint.is_empty() { "" } else { " " }));
    None
}

/// `/permissions`: the mode to set.
async fn choose_mode(view: &View, out: &Output) -> Option<Input> {
    let current = view.permissions.mode();
    let mark = |mode: &Mode| if *mode == current { Mark::Check } else { Mark::None };
    let items = Mode::ALL.iter().map(|m| Item::new(m.as_str(), print::capitalized(m.describe())).mark(mark(m))).collect();
    let rules = view.permissions.rules().into_iter().filter(|(_, r)| !r.is_empty());
    let rules: Vec<String> = rules.map(|(kind, r)| format!("{} {kind}", r.len())).collect();
    let about = if rules.is_empty() { String::new() } else { format!("Rules apply in every mode: {} (/permissions rules)", rules.join(" · ")) };
    let selected = Mode::ALL.iter().position(|m| *m == current).unwrap_or(0);
    let keys = Keys { enter: "apply", esc: "cancel" };
    let i = dialog::choose(out, "Permission mode", &about, keys, items, selected).await?;
    Some(Input::Permissions(Some(Mode::ALL[i].as_str().to_string())))
}

/// `/model`: the main and subagent models and the effort, all at once.
async fn choose_model(view: &View, out: &Output) -> Option<Input> {
    let client = super::model::pick(out, &view.client()).await?;
    let subagents = client.subagent_model().map_or("follow it".to_string(), |model| format!("use {model}"));
    let rest = out.paint(DIM, &format!("· {} effort · subagents {subagents}", client.effort().as_str()));
    print::done(out, format!("Model set to {} {rest}{}", out.paint(BOLD, client.spec()), view.later()));
    view.set_client(client);
    None
}

/// Saves the advisor as `advisor_model` in the settings, or removes it.
fn save_advisor(model: Option<&str>) -> anyhow::Result<()> {
    crate::settings::edit(|settings| {
        match model {
            Some(model) => settings.insert("advisor_model".into(), model.into()),
            None => settings.remove("advisor_model"),
        };
        Ok(())
    })
}

/// `/advisor`: no advisor, one of the aliases, or the model set now.
async fn choose_advisor(view: &View, out: &Output) -> Option<Input> {
    let client = view.client();
    let current = client.advisor_model();
    let mut items = vec![Item::new("off", "No advisor")];
    let mut names = vec!["off".to_string()];
    for (alias, model) in llm::advisor::aliases() {
        let pairs = llm::advisor::accepts(client.spec(), model);
        let about = if pairs { model.to_string() } else { format!("{model} · cannot advise {}", client.spec()) };
        items.push(Item::new(*alias, about));
        names.push(model.to_string());
    }
    if let Some(model) = current.filter(|m| !names.iter().any(|n| n == m)) {
        items.push(Item::new(model, "Set now"));
        names.push(model.to_string());
    }
    let chosen = names.iter().position(|n| Some(n.as_str()) == current.or(Some("off"))).unwrap_or(0);
    items[chosen].mark = Mark::Check;
    let about = "Consulted at decision points, on the Anthropic API; saved to the settings";
    let keys = Keys { enter: "apply", esc: "cancel" };
    let i = dialog::choose(out, "Advisor", about, keys, items, chosen).await?;
    Some(Input::Advisor(Some(names[i].clone())))
}

/// `/effort`: a level, or the configured one.
async fn choose_effort(view: &View, out: &Output) -> Option<Input> {
    let client = view.client();
    let chosen = client.effort_override().and_then(|e| llm::Effort::ALL.iter().position(|level| *level == e)).map_or(0, |i| i + 1);
    let configured = if chosen == 0 { format!("The configured effort ({})", client.effort().as_str()) } else { "The configured effort".into() };
    let mut items = vec![Item::new("auto", configured)];
    items.extend(llm::Effort::ALL.iter().map(|e| Item::new(e.as_str(), effort_help(*e))));
    items[chosen].mark = Mark::Check;
    let keys = Keys { enter: "apply", esc: "cancel" };
    let i = dialog::choose(out, "Reasoning effort", "", keys, items, chosen).await?;
    Some(Input::Effort(Some(i.checked_sub(1).map(|i| llm::Effort::ALL[i]))))
}

/// `/skills`: Enter puts a user-invocable one in the prompt.
async fn pick_skill(skills: &[Skill], out: &Output, typing: &mut Typing) -> Option<Input> {
    let items = skills.iter().map(|s| {
        let invoked = match (s.user_invocable, s.model_invocable) {
            (true, true) => "you and the model",
            (true, false) => "only you",
            (false, _) => "only the model",
        };
        Item::new(format!("/{}", s.name), &s.description)
            .field("Invoked by", invoked)
            .field("Runs in", if s.fork.is_some() { "a subagent" } else { "this conversation" })
            .field("Source", s.dir.display().to_string())
    });
    let keys = Keys { enter: "insert", esc: "close" };
    if let Some(i) = dialog::choose(out, "Skills", "", keys, items.collect(), 0).await {
        if skills[i].user_invocable {
            typing.draft.set_text(&format!("/{} ", skills[i].name));
        } else {
            print::done(out, format!("/{} is for the model to invoke, not for the prompt", skills[i].name));
        }
    }
    None
}

/// `/tools`: each tool with its input schema.
fn tool_items(view: &View) -> Vec<Item> {
    let items = view.tools.definitions().into_iter().map(|t| {
        let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
        let item = Item::new(t.name, print::summary(&t.description)).about(t.description.as_str());
        item.section("Input schema").details(schema.lines().map(String::from))
    });
    items.collect()
}

/// `/agents`: the subagent types, with their access, tools, model and
/// prompt.
fn agent_items(view: &View, env: &Env) -> Vec<Item> {
    let client = view.client();
    let items = env.ext.agent_defs.iter().map(|d| {
        Item::new(&d.name, print::summary(&d.description))
            .about(&d.description)
            .field("Access", d.permission.as_str())
            .field("Tools", d.tools.as_ref().map_or("all permitted".into(), |t| t.join(", ")))
            .field("Model", d.model.as_deref().or(client.subagent_model()).unwrap_or(client.spec()))
            .field("Source", d.path.as_ref().map_or("built-in".into(), |p| p.display().to_string()))
            .section("Prompt")
            .details(vec![d.prompt.clone()])
    });
    items.collect()
}

/// `/mcp`: each server, its tools or why it failed, and its log.
fn mcp_items(env: &Env) -> Vec<Item> {
    let items = env.mcp_statuses().map(|s| {
        let took = crate::util::short_duration(s.took);
        let log = crate::extensions::mcp::log_path(&s.name).display().to_string();
        match &s.outcome {
            Ok(tools) => {
                let count = format!("{} {}", tools.len(), text::plural(tools.len(), "tool"));
                let item = Item::new(&s.name, format!("{count} · connected in {took}")).mark(Mark::Dot);
                item.field("Log", log).section("Tools").details(tools.clone())
            }
            Err(e) => {
                let item = Item::new(&s.name, format!("Failed after {took}")).mark(Mark::Cross);
                item.field("Log", log).section("Error").details(vec![e.clone()])
            }
        }
    });
    items.collect()
}

/// `/usage` as a panel: how full the context is, and the tokens used by
/// model.
pub(super) async fn usage(agent: &Agent, out: &Output) {
    dialog::sheet(out, "Usage", print::usage_lines(out, agent, dialog::room())).await;
}

/// The conversation's subagents and background commands (only the running
/// ones if `running_only`), as their ids and items.
fn task_items(view: &View, running_only: bool) -> Vec<(String, Item)> {
    let mark = |running: bool, status: &str| match status {
        _ if running => Mark::Dot,
        "completed" | "ended" => Mark::Check,
        "failed" => Mark::Cross,
        _ => Mark::Ring,
    };
    let mut items = Vec::new();
    for s in view.subagents.list() {
        let status = s["status"].as_str().unwrap_or("");
        let running = status == "running";
        if running_only && !running {
            continue;
        }
        let id = s["id"].as_str().unwrap_or("").to_string();
        let (model, time) = (s["model"].as_str().unwrap_or(""), output::elapsed(s["seconds"].as_u64().unwrap_or(0)));
        let label = s["label"].as_str().unwrap_or("");
        let item = Item::new(&id, format!("{status} · {label} · {time}"))
            .mark(mark(running, status))
            .field("Task", label)
            .field("Status", status)
            .field("Model", model)
            .field("Tool calls", s["tool_calls"].to_string())
            .field("Time", time)
            .field("Latest call", s["last_call"].as_str().unwrap_or("—"))
            .field("Runs in", if s["background"] == true { "the background" } else { "the foreground" });
        items.push((id, item));
    }
    for (id, command, running) in view.shells.list() {
        if running_only && !running {
            continue;
        }
        let status = if running { "running" } else { "ended" };
        let item = Item::new(&id, format!("{status} · $ {}", command.lines().next().unwrap_or("")))
            .mark(mark(running, status))
            .field("Task", "background command")
            .field("Status", status)
            .section("Command")
            .details(command.lines().map(String::from));
        items.push((id, item));
    }
    items
}

/// `/stop`: a running task to stop, or all of them.
async fn stop(view: &View, out: &Output) -> Option<Input> {
    let tasks = task_items(view, true);
    if tasks.is_empty() {
        print::done(out, "No running tasks");
        return None;
    }
    let mut items: Vec<_> = tasks.iter().map(|(_, item)| item.clone()).collect();
    if tasks.len() > 1 {
        items.push(Item::new("All of them", format!("{} running tasks", tasks.len())));
    }
    let i = dialog::choose(out, "Stop a task", "", Keys { enter: "stop", esc: "cancel" }, items, 0).await?;
    Some(Input::Stop(tasks.get(i).map(|(id, _)| id.clone())))
}

/// `/tasks`: the subagents and background commands, to look through.
async fn tasks(view: &View, out: &Output) -> Option<Input> {
    let tasks = task_items(view, false);
    if tasks.is_empty() {
        print::done(out, "No subagents or background commands in this conversation");
        return None;
    }
    let running = tasks.iter().filter(|(_, item)| item.mark == Mark::Dot).count();
    let about = if running > 0 { format!("{running} running · /stop stops them") } else { String::new() };
    dialog::inspect(out, "Tasks", &about, "", tasks.into_iter().map(|(_, item)| item).collect()).await;
    None
}

fn provider_items(view: &View) -> Vec<Item> {
    let client = view.client();
    let providers = client.providers();
    let items = providers.names().into_iter().map(|name| {
        let config = providers.provider(name).unwrap();
        let models = format!("{} {}", config.models.len(), text::plural(config.models.len(), "model"));
        Item::new(name, format!("{} · {models}", config.protocol.as_str()))
            .field("Protocol", config.protocol.as_str())
            .field("Base URL", &config.base_url)
            .field("API key", if config.api_key.is_some() { "configured" } else { "not set" })
            .section("Models")
            .details(config.models.keys().map(|m| format!("{name}/{m}")))
    });
    items.collect()
}

async fn providers(view: &View, out: &Output) -> Option<Input> {
    let actions = vec![
        Item::new("Providers", "Look through the providers, their protocols and models"),
        Item::new("Add a provider", "Name it, then set its protocol, URL, key and models"),
        Item::new("Edit a provider", "Go through a provider's settings again"),
        Item::new("Remove a provider", "Take a provider and its models out of the settings"),
        Item::new("Default model", "Choose the model new conversations start with"),
        Item::new("Add a model", "List one more model under a provider"),
        Item::new("Remove a model", "Take a model off a provider's list"),
        Item::new("Sync models", "List what a provider's gateway offers, by its saved patterns"),
    ];
    loop {
        let about = "Changes are saved to the settings file";
        let action = dialog::choose(out, "Providers", about, SELECT, actions.clone(), 0).await?;
        match action {
            0 => dialog::inspect(out, "Providers", "", "No providers configured", provider_items(view)).await,
            1 => {
                let about = "Its name: letters, digits, dots, underscores or hyphens";
                let Some(name) = dialog::input(out, "Add a provider", about).await else { continue };
                return Some(Input::Providers(vec!["add".into(), name]));
            }
            5 => {
                let names: Vec<_> = load_settings(out)?.providers.into_keys().collect();
                let items = names.iter().map(|name| Item::new(name, "")).collect();
                let Some(i) = dialog::choose(out, "Add a model › provider", "", BACK, items, 0).await else { continue };
                let about = format!("A model name the {} gateway accepts", names[i]);
                let Some(model) = dialog::input(out, "Add a model", &about).await else { continue };
                return Some(Input::Providers(vec!["model".into(), "add".into(), format!("{}/{model}", names[i])]));
            }
            2 | 3 | 7 => {
                let (command, title, about, enter) = match action {
                    2 => ("add", "Edit a provider", "Its API key stays hidden", "edit"),
                    7 => ("sync", "Sync models", "Lists the models the gateway gives at /v1/models", "sync"),
                    _ => ("remove", "Remove a provider", "", "remove"),
                };
                let names: Vec<_> = load_settings(out)?.providers.into_keys().collect();
                let items = names.iter().map(|n| Item::new(n, "")).collect();
                let keys = Keys { enter, esc: "back" };
                let Some(i) = dialog::choose(out, title, about, keys, items, 0).await else { continue };
                return Some(Input::Providers(vec![command.into(), names[i].clone()]));
            }
            4 | 6 => {
                let default = action == 4;
                // A model can be removed only from the providers in the
                // settings file, not from those set up by environment variables.
                let models = if default {
                    view.client().providers().listed_models()
                } else {
                    let providers = load_settings(out)?.providers.into_iter();
                    providers.flat_map(|(name, p)| p.models.into_keys().map(move |m| format!("{name}/{m}"))).collect()
                };
                let items = models.iter().map(|m| Item::new(m, "")).collect();
                let keys = Keys { enter: if default { "save" } else { "remove" }, esc: "back" };
                let title = if default { "Default model" } else { "Remove a model" };
                let Some(i) = dialog::choose(out, title, "", keys, items, 0).await else { continue };
                let model = models[i].clone();
                let args = if default { vec!["default".into(), model] } else { vec!["model".into(), "remove".into(), model] };
                return Some(Input::Providers(args));
            }
            _ => unreachable!(),
        }
    }
}

async fn plugins(env: &Env, out: &Output) -> Option<Input> {
    let actions = vec![
        Item::new("Loaded plugins", "Look through their versions, what they add and what failed"),
        Item::new("Manage a plugin", "Enable, disable, configure or uninstall one"),
        Item::new("Install a plugin", "By its name: plugin@marketplace"),
        Item::new("Update plugins", "Fetch the latest of every plugin and marketplace"),
        Item::new("Marketplaces", "Look through the marketplaces in the settings"),
    ];
    loop {
        let about = "Changes apply after /reload-plugins";
        let action = dialog::choose(out, "Plugins", about, SELECT, actions.clone(), 0).await?;
        match action {
            0 => {
                let items = env.ext.plugins.list.iter().map(|p| {
                    let mut item = Item::new(&p.id, p.description.as_deref().unwrap_or("")).field("Version", p.version.as_deref().unwrap_or("—"));
                    item = match &p.outcome {
                        Ok(parts) if parts.is_empty() => item.mark(Mark::Dot).field("Adds", "nothing the harness can use"),
                        Ok(parts) => item.mark(Mark::Dot).field("Adds", parts.join(", ")),
                        Err(e) => item.mark(Mark::Cross).field("Failed", e),
                    };
                    if let Some(root) = &p.root {
                        item = item.field("Root", root.display().to_string());
                    }
                    if !p.ignored.is_empty() {
                        item = item.field("Not used", p.ignored.join("; "));
                    }
                    item.section("Warnings").details(p.warnings.clone())
                });
                let empty = "No plugins enabled · they go under `enabledPlugins` in ~/.harness/settings.json, or --plugin-dir";
                dialog::inspect(out, "Loaded plugins", "", empty, items.collect()).await;
            }
            1 => {
                let settings = load_settings(out)?;
                let loaded = env.ext.plugins.list.iter().map(|p| p.id.clone());
                let ids: BTreeSet<_> = settings.enabled_plugins.keys().cloned().chain(loaded).collect();
                let ids: Vec<_> = ids.into_iter().collect();
                if ids.is_empty() {
                    print::done(out, "No plugins installed");
                    return None;
                }
                let disabled = |id: &String| settings.enabled_plugins.get(id) == Some(&false);
                let state = |id: &String| if disabled(id) { ("disabled", Mark::Ring) } else { ("enabled", Mark::Dot) };
                let items = ids.iter().map(|id| Item::new(id, state(id).0).mark(state(id).1)).collect();
                let Some(i) = dialog::choose(out, "Manage a plugin", "", BACK, items, 0).await else { continue };
                let id = &ids[i];
                let actions = [
                    ("Enable", "Load it from the next /reload-plugins"),
                    ("Disable", "Keep it installed, but don't load it"),
                    ("Configure", "Answer its settings again"),
                    ("Uninstall", "Remove it, keeping its data"),
                ];
                let items = actions.iter().map(|(label, about)| Item::new(*label, *about)).collect();
                let keys = Keys { enter: "run", esc: "back" };
                let Some(action) = dialog::choose(out, &format!("Manage a plugin › {id}"), "", keys, items, 0).await else { continue };
                let command = ["enable", "disable", "configure", "uninstall"][action];
                let mut args = vec![command.into(), id.clone()];
                if action == 3 {
                    args.push("--keep-data".into());
                }
                return Some(Input::ManagePlugins(args));
            }
            2 => {
                let about = "Its name: plugin@marketplace";
                let Some(spec) = dialog::input(out, "Install a plugin", about).await else { continue };
                return Some(Input::ManagePlugins(vec!["install".into(), spec]));
            }
            3 => return Some(Input::Plugins { update: true }),
            4 => {
                let settings = load_settings(out)?;
                let items = settings.extra_known_marketplaces.keys().map(|name| Item::new(name, "")).collect();
                let about = "/plugins marketplace add|remove changes them";
                dialog::inspect(out, "Marketplaces", about, "None but the built-in claude-plugins-official", items).await;
            }
            _ => unreachable!(),
        }
    }
}

/// The settings file, or `None` after showing why it can't be read.
fn load_settings(out: &Output) -> Option<Settings> {
    Settings::load().map_err(|e| out.error(format!("{e:#}"))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse::parse_input;

    #[test]
    fn help_shows_the_commands_or_the_keys() {
        use dialog::Panel;
        let tabs = ["Commands", "Shortcuts"];
        let items = BUILTINS.iter().map(|b| Item::new(b.name, b.about).hint(b.args)).collect();
        let out = Output::capture();
        let mut help = Help {
            commands: List::new("Help", "", Keys { enter: "insert", esc: "close" }, items, 0).tabs(&tabs, 0),
            keys: Sheet::new("Help", print::key_lines(&out)).tabs(&tabs, 1),
            shortcuts: false,
        };
        let rows = |help: &Help| help.render(100, 40, &|_, s| s.into()).0.iter().map(|r| text::strip_ansi(r)).collect::<Vec<_>>();
        let shown = rows(&help);
        assert_eq!(shown[1], "  ● Commands  ○ Shortcuts");
        assert!(shown.contains(&"    /model [provider/model]         Choose the model, the subagents' model and the effort".to_string()), "{shown:?}");
        assert_eq!(shown.last().unwrap(), "  ↑↓ select · enter insert · tab shortcuts · esc close");
        assert_eq!(help.handle(Key::Tab), None);
        let shown = rows(&help);
        assert_eq!(shown[1], "  ○ Commands  ● Shortcuts");
        assert!(shown.contains(&"    Esc Esc         Go back to before an earlier prompt (/rewind)".to_string()), "{shown:?}");
        assert_eq!(shown.last().unwrap(), "  tab commands · esc close");
        assert_eq!(help.handle(Key::Enter), Some(None), "the keys have nothing to take");
        help.handle(Key::Tab);
        help.handle(Key::Down);
        assert_eq!(help.handle(Key::Enter), Some(Some(1)));
    }

    #[test]
    fn commands_say_how_they_went_under_their_line() {
        let out = Output::capture();
        print::done(&out, "Effort set to max");
        print::failed(&out, "unknown command /nope (try /help)");
        print::help(&out, &[]);
        let lines = out.captured();
        assert_eq!(lines[..3], ["  ⎿ Effort set to max", "  ⎿ unknown command /nope (try /help)", "  Commands"]);
        assert!(lines.contains(&"    /stop [id]                      Stop running tasks".to_string()), "{lines:?}");
        assert!(lines.contains(&"  At an empty prompt".to_string()) && lines.contains(&"    Ctrl-C Ctrl-C   Quit".to_string()), "{lines:?}");
    }

    #[test]
    fn commands_that_leave_the_conversation_run_during_a_turn() {
        let now = ["/model", "/model gw/m", "/effort high", "/advisor opus", "/advisor", "/permissions plan", "/tasks", "/stop", "/help", "/models"];
        for line in now {
            assert!(runs_during_turn(&parse_input(line, &[])), "{line}");
        }
        let later = ["/clear", "/compact", "/rewind", "/resume", "/usage", "/btw why", "/reload-plugins", "/exit", "!ls"];
        for line in later {
            assert!(!runs_during_turn(&parse_input(line, &[])), "{line}");
        }
    }
}
