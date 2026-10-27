//! Commands that leave the conversation as it is: what they show, line by
//! line or in a panel, and running them beside a turn.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use super::dialog::{self, Item, Keys};
use super::input::Typing;
use super::parse::{BUILTINS, Input, effort_help, help_keys};
use super::{Output, output, print, text};
use super::style::BOLD;
use crate::agent::Agent;
use crate::llm::{self, Client};
use crate::permissions::{Mode, Permissions};
use crate::subagent::Subagents;
use crate::tools::{ToolRegistry, shells::Shells};
use crate::{settings::Settings, setup::Env, skills::Skill};

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
    pub fn set_client(&self, client: Client, out: &Output) {
        if self.running {
            out.note("used from the next message; this one goes on as it began");
        }
        *self.switch_to.lock().unwrap() = Some(client);
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
            | Input::Effort(_)
            | Input::Permissions(_)
            | Input::Error(_)
    )
}

/// Runs a command that [`runs_during_turn`], once its panel, if any, is
/// done. A model chosen goes to `view`, for the agent to switch to.
pub(super) async fn run(input: Input, view: &View, env: &Env, skills: &[Skill], out: &Output) {
    match input {
        Input::Help => print::help(out, skills),
        Input::ListSkills => print::skills(out, skills),
        Input::ListTools => print::tools(out, &view.tools),
        Input::ListMcp => print::mcp(out, env.mcp_statuses()),
        Input::ListAgents => print::agents(out, &env.ext.agent_defs),
        Input::Tasks => print::tasks(out, &view.subagents.list(), &view.shells.list()),
        Input::Stop(None) => {
            let stopped = view.subagents.stop_all();
            let killed = view.shells.stop_all();
            let note = format!(
                "stopped {stopped} {} and {killed} {}",
                text::plural(stopped, "subagent"),
                text::plural(killed, "background command")
            );
            out.note(&note);
        }
        Input::Stop(Some(id)) if id.starts_with("shell_") => match view.shells.stop(&id) {
            Ok(true) => out.note(format!("stopped {id}")),
            Ok(false) => out.note(format!("{id} is not running")),
            Err(e) => out.error(&e),
        },
        Input::Stop(Some(id)) => match view.subagents.stop(&id, true) {
            Ok(true) => out.note(format!("stopped {id}")),
            Ok(false) => out.note(format!("{id} is not running")),
            Err(e) => out.error(format!("{e:#}")),
        },
        Input::Sandbox => print::sandbox(out),
        Input::Model(None) => print::models(out, &view.client()),
        Input::Model(Some(spec)) => match view.client().switch(&spec) {
            Ok(client) => {
                out.line(format!("model: {}", out.paint(BOLD, client.spec())));
                view.set_client(client, out);
            }
            Err(e) => out.error(format!("{e:#}")),
        },
        Input::Permissions(None) => print::permissions(&view.permissions, out),
        Input::Permissions(Some(arg)) if arg == "rules" => print::permissions(&view.permissions, out),
        Input::Permissions(Some(arg)) => match Mode::parse(&arg) {
            Ok(mode) => {
                view.permissions.set_mode(mode);
                out.line(format!("permission mode: {} — {}", out.paint(BOLD, mode.as_str()), mode.describe()));
            }
            Err(e) => out.error(&e),
        },
        Input::Effort(None) => {
            out.line(format!("effort: {}", out.paint(BOLD, view.client().effort().as_str())));
            out.note("set with /effort low|medium|high|xhigh|max, or /effort auto");
        }
        Input::Effort(Some(level)) => {
            let mut client = view.client();
            client.set_effort(level);
            out.line(format!("effort: {}", out.paint(BOLD, client.effort().as_str())));
            view.set_client(client, out);
        }
        Input::Error(e) => out.error(&e),
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

const SELECT: Keys = Keys { enter: "select", esc: "cancel" };
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
        Input::ListSkills => pick_skill(skills, out, typing).await,
        Input::ListTools => browse(out, "Tools", "What the model can call, with input schemas", tool_items(view)).await,
        Input::ListAgents => browse(out, "Agent types", "Subagent types the model can start", agent_items(view, env)).await,
        Input::ListMcp => browse(out, "MCP servers", "Servers, their tools and connection errors", mcp_items(env)).await,
        Input::Sandbox => browse(out, "Sandbox", "What commands may do; read-only", sandbox_items()).await,
        Input::Stop(None) => stop(view, out).await,
        Input::Tasks => tasks(view, out).await,
        Input::Providers(args) if args.is_empty() => providers(view, out).await,
        Input::Plugins { update: false } => plugins(env, out).await,
        input => Some(input),
    }
}

/// Shows `items` in a panel to look through; nothing runs after it.
async fn browse(out: &Output, title: &str, about: &str, items: Vec<Item>) -> Option<Input> {
    dialog::inspect(out, title, about, items).await;
    None
}

/// `/help`: the commands, skills and keys; Enter puts a command in the
/// prompt.
async fn help(skills: &[Skill], out: &Output, typing: &mut Typing) -> Option<Input> {
    let mut items: Vec<_> = BUILTINS.iter().map(|(name, description)| Item::new(*name, *description)).collect();
    let skills = skills.iter().filter(|s| s.user_invocable);
    items.extend(skills.map(|s| Item::new(format!("/{}", s.name), &s.description)));
    let keyboard = items.len();
    let keys: Vec<_> = help_keys().iter().map(|(key, description)| format!("{key}: {description}")).collect();
    items.push(Item::new("Keyboard shortcuts", "Browse composer, interrupt and rewind shortcuts").details(keys.clone()));
    let about = "Built-in commands, skills and keyboard shortcuts";
    let insert = Keys { enter: "insert", esc: "cancel" };
    while let Some(i) = dialog::choose(out, "Commands", about, insert, items.clone(), 0).await {
        if i == keyboard {
            dialog::document(out, "Keyboard shortcuts", keys.clone()).await;
        } else {
            typing.draft.set_text(&items[i].label);
            break;
        }
    }
    None
}

/// `/permissions`: the mode to set, with the rules beside each.
async fn choose_mode(view: &View, out: &Output) -> Option<Input> {
    let rules = view.permissions.rules().into_iter().filter(|(_, r)| !r.is_empty());
    let rules: Vec<String> = rules.map(|(kind, r)| format!("{kind}: {}", r.join(", "))).collect();
    let items = Mode::ALL.iter().map(|m| Item::new(m.as_str(), m.describe()).details(rules.clone())).collect();
    let current = Mode::ALL.iter().position(|m| *m == view.permissions.mode()).unwrap_or(0);
    let about = "Which tool calls ask first; Shift+Tab cycles them too";
    let keys = Keys { enter: "apply", esc: "cancel" };
    let i = dialog::choose(out, "Permission mode", about, keys, items, current).await?;
    Some(Input::Permissions(Some(Mode::ALL[i].as_str().to_string())))
}

/// `/model`: the main and subagent models and the effort, all at once.
async fn choose_model(view: &View, out: &Output) -> Option<Input> {
    let client = super::model::pick(out, &view.client()).await?;
    out.line(format!("model: {}", out.paint(BOLD, client.spec())));
    out.line(format!("subagents: {}", client.subagent_model().unwrap_or("follow main model")));
    out.line(format!("effort: {}", out.paint(BOLD, client.effort().as_str())));
    view.set_client(client, out);
    None
}

/// `/effort`: a level, or the configured one.
async fn choose_effort(view: &View, out: &Output) -> Option<Input> {
    let client = view.client();
    let configured = format!("Use configured effort (currently {})", client.effort().as_str());
    let mut items = vec![Item::new("auto", configured)];
    items.extend(llm::Effort::ALL.iter().map(|e| Item::new(e.as_str(), effort_help(*e))));
    let chosen = client.effort_override().and_then(|e| llm::Effort::ALL.iter().position(|level| *level == e));
    let about = "Applies to this session; /model changes the same setting";
    let keys = Keys { enter: "apply", esc: "cancel" };
    let i = dialog::choose(out, "Reasoning effort", about, keys, items, chosen.map_or(0, |i| i + 1)).await?;
    Some(Input::Effort(Some(i.checked_sub(1).map(|i| llm::Effort::ALL[i]))))
}

/// `/skills`: Enter puts a user-invocable one in the prompt.
async fn pick_skill(skills: &[Skill], out: &Output, typing: &mut Typing) -> Option<Input> {
    let items = skills.iter().map(|s| {
        Item::new(format!("/{}", s.name), &s.description).details(vec![
            format!("Source: {}", s.dir.display()),
            format!("User invocable: {} · model invocable: {}", s.user_invocable, s.model_invocable),
            format!("Runs in a subagent: {}", s.fork.is_some()),
        ])
    });
    let about = "Discovered skills; only user-invocable ones go into the prompt";
    let keys = Keys { enter: "insert", esc: "cancel" };
    if let Some(i) = dialog::choose(out, "Skills", about, keys, items.collect(), 0).await
        && skills[i].user_invocable
    {
        typing.draft.set_text(&format!("/{} ", skills[i].name));
    }
    None
}

/// `/tools`: each tool with its input schema.
fn tool_items(view: &View) -> Vec<Item> {
    let items = view.tools.definitions().into_iter().map(|t| {
        let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
        let details = std::iter::once("Input schema:".into()).chain(schema.lines().map(String::from));
        Item::new(t.name, t.description).details(details)
    });
    items.collect()
}

/// `/agents`: the subagent types, with their access, tools, model and
/// prompt.
fn agent_items(view: &View, env: &Env) -> Vec<Item> {
    let client = view.client();
    let items = env.ext.agent_defs.iter().map(|d| {
        Item::new(&d.name, &d.description).details(vec![
            format!("Access: {}", d.permission.as_str()),
            format!("Tools: {}", d.tools.as_ref().map_or("all permitted".into(), |t| t.join(", "))),
            format!("Model: {}", d.model.as_deref().or(client.subagent_model()).unwrap_or(client.spec())),
            format!("Source: {}", d.path.as_ref().map_or("built-in".into(), |p| p.display().to_string())),
            d.prompt.clone(),
        ])
    });
    items.collect()
}

/// `/mcp`: each server, its tools or why it failed, and its log.
fn mcp_items(env: &Env) -> Vec<Item> {
    let items = env.mcp_statuses().map(|s| {
        let took = crate::util::short_duration(s.took);
        let (description, details) = match &s.outcome {
            Ok(tools) => {
                let count = format!("{} {}", tools.len(), text::plural(tools.len(), "tool"));
                (format!("Connected in {took} · {count}"), tools.clone())
            }
            Err(e) => (format!("Connection failed after {took}"), vec![e.clone()]),
        };
        let log = format!("Log: {}", crate::mcp::log_path(&s.name).display());
        Item::new(&s.name, description).details(details.into_iter().chain(std::iter::once(log)))
    });
    items.collect()
}

/// `/sandbox`: what it lets commands do, and what it costs each one.
fn sandbox_items() -> Vec<Item> {
    let Some(sandbox) = crate::sandbox::active() else {
        return vec![Item::new("Sandbox is off", "Start with --sandbox to enable it; this view does not change permissions")];
    };
    let per_command = crate::util::short_duration(sandbox.startup());
    let startup = ("startup", format!("{per_command} per command (measured at launch)"));
    sandbox.describe().into_iter().chain([startup]).map(|(label, value)| Item::new(label, value)).collect()
}

/// `/usage` as a panel: the context's size, and the tokens used by model.
pub(super) async fn usage(agent: &Agent, out: &Output) {
    let (used, window) = (agent.context_tokens(), agent.client().context_window());
    let context = format!(
        "{} / {} tokens (about {}%)",
        output::tokens(used),
        output::tokens(window),
        used.saturating_mul(100) / window.max(1)
    );
    let mut items = vec![Item::new("Current context", context)];
    items.extend(agent.usage().models().into_iter().map(|(model, t)| {
        let requests = format!("{} {}", t.requests, text::plural(t.requests as usize, "request"));
        Item::new(model, format!("{requests} · {} output tokens", output::tokens(t.usage.output_tokens))).details(vec![
            format!("Input: {}", t.usage.input_tokens),
            format!("Cache reads: {}", t.usage.cache_read_tokens),
            format!("Cache writes: {}", t.usage.cache_write_tokens),
            format!("Output: {}", t.usage.output_tokens),
        ])
    }));
    if let Some(cost) = print::usage_cost(agent) {
        items.push(Item::new("Cost", cost));
    }
    dialog::inspect(out, "Usage", "This conversation, by model", items).await;
}

/// The conversation's subagents and background commands (only the running
/// ones if `running_only`), as their ids, items and whether they run.
fn task_items(view: &View, running_only: bool) -> Vec<(String, Item, bool)> {
    let mut items = Vec::new();
    for s in view.subagents.list() {
        let running = s["status"] == "running";
        if running_only && !running {
            continue;
        }
        let id = s["id"].as_str().unwrap_or("").to_string();
        let label = format!("{id} · {}", s["label"].as_str().unwrap_or(""));
        let status = format!("{} · {}", s["status"].as_str().unwrap_or(""), s["model"].as_str().unwrap_or(""));
        let item = Item::new(label, status).details(vec![
            format!("Tool calls: {} · time: {}", s["tool_calls"], output::elapsed(s["seconds"].as_u64().unwrap_or(0))),
            format!("Latest call: {}", s["last_call"].as_str().unwrap_or("—")),
            format!("Background: {}", s["background"]),
        ]);
        items.push((id, item, running));
    }
    for (id, command, running) in view.shells.list() {
        if running_only && !running {
            continue;
        }
        let item = Item::new(format!("{id} · background command"), if running { "running" } else { "ended" })
            .details(command.lines().map(String::from));
        items.push((id, item, running));
    }
    items
}

async fn stop(view: &View, out: &Output) -> Option<Input> {
    let tasks = task_items(view, true);
    if tasks.is_empty() {
        out.note("no running tasks");
        return None;
    }
    let mut items: Vec<_> = tasks.iter().map(|(_, item, _)| item.clone()).collect();
    items.push(Item::new(
        "Stop all running tasks",
        "Stop every running subagent and background command in this conversation",
    ));
    let about = "Running subagents and background commands";
    let i = dialog::choose(out, "Stop tasks", about, Keys { enter: "stop", esc: "cancel" }, items, 0).await?;
    Some(Input::Stop(tasks.get(i).map(|(id, _, _)| id.clone())))
}

async fn tasks(view: &View, out: &Output) -> Option<Input> {
    loop {
        let tasks = task_items(view, false);
        if tasks.is_empty() {
            out.note("no subagents or background commands in this conversation");
            return None;
        }
        let items = tasks.iter().map(|(_, item, _)| item.clone()).collect();
        let about = "Subagents and background commands in this conversation";
        let i = dialog::choose(out, "Tasks", about, Keys { enter: "open", esc: "cancel" }, items, 0).await?;
        let (id, item, running) = &tasks[i];
        let mut actions = vec![Item::new("View details", &item.description)];
        if *running {
            actions.push(Item::new("Stop this task", format!("Stop {id}")));
        }
        let Some(action) = dialog::choose(out, &format!("Task {id}"), "", BACK, actions, 0).await else { continue };
        if action == 1 {
            return Some(Input::Stop(Some(id.clone())));
        }
        let lines = std::iter::once(item.description.clone()).chain(item.details.clone()).collect();
        dialog::document(out, &item.label, lines).await;
    }
}

fn provider_items(view: &View) -> Vec<Item> {
    let client = view.client();
    let providers = client.providers();
    let items = providers.names().into_iter().map(|name| {
        let config = providers.provider(name).unwrap();
        let models = format!("{} listed {}", config.models.len(), text::plural(config.models.len(), "model"));
        let key = format!("API key: {}", if config.api_key.is_some() { "configured" } else { "not set" });
        Item::new(name, format!("{} · {models}", config.protocol.as_str()))
            .details(std::iter::once(key).chain(config.models.keys().map(|m| format!("{name}/{m}"))))
    });
    items.collect()
}

async fn providers(view: &View, out: &Output) -> Option<Input> {
    let actions = vec![
        Item::new("Inspect providers", "Browse configured providers, protocols and models"),
        Item::new("Add provider", "Enter a name, then configure its protocol, URL, key and models"),
        Item::new("Edit provider", "Choose a provider and start its configuration wizard"),
        Item::new("Set default model", "Save the default for new conversations in the settings file"),
        Item::new("Add listed model", "Choose a provider and enter a model name"),
        Item::new("Remove listed model", "Remove a model from the configured list"),
        Item::new("Remove provider", "Remove the provider and its model list from settings"),
    ];
    loop {
        let about = "Changes are saved to the settings file";
        let action = dialog::choose(out, "Providers", about, SELECT, actions.clone(), 0).await?;
        match action {
            0 => dialog::inspect(out, "Provider details", "", provider_items(view)).await,
            1 => {
                let about = "Provider name (letters, digits, dots, underscores or hyphens)";
                let Some(name) = dialog::input(out, "Add provider", about).await else { continue };
                return Some(Input::Providers(vec!["add".into(), name]));
            }
            4 => {
                let names: Vec<_> = load_settings(out)?.providers.into_keys().collect();
                let items = names.iter().map(|name| Item::new(name, "Choose the provider for this model")).collect();
                let title = "Add model: provider";
                let Some(i) = dialog::choose(out, title, "Providers in the settings file", BACK, items, 0).await else {
                    continue;
                };
                let about = "Model name accepted by the selected gateway";
                let Some(model) = dialog::input(out, "Add model", about).await else { continue };
                return Some(Input::Providers(vec!["model".into(), "add".into(), format!("{}/{model}", names[i])]));
            }
            2 | 6 => {
                let edit = action == 2;
                let names: Vec<_> = load_settings(out)?.providers.into_keys().collect();
                let what = if edit { "Edit settings; the API key stays hidden" } else { "Removes this provider from settings" };
                let items = names.iter().map(|n| Item::new(n, what)).collect();
                let keys = Keys { enter: if edit { "edit" } else { "remove" }, esc: "back" };
                let title = if edit { "Edit provider" } else { "Remove provider" };
                let Some(i) = dialog::choose(out, title, "Providers in the settings file", keys, items, 0).await else {
                    continue;
                };
                return Some(Input::Providers(vec![if edit { "add" } else { "remove" }.into(), names[i].clone()]));
            }
            3 | 5 => {
                let default = action == 3;
                // A model can be removed only from the providers in the
                // settings file, not from those set up by environment variables.
                let models = if default {
                    view.client().providers().listed_models()
                } else {
                    let providers = load_settings(out)?.providers.into_iter();
                    providers.flat_map(|(name, p)| p.models.into_keys().map(move |m| format!("{name}/{m}"))).collect()
                };
                let what = if default { "Saves this default model" } else { "Removes this model from the list" };
                let items = models.iter().map(|m| Item::new(m, what)).collect();
                let keys = Keys { enter: if default { "save" } else { "remove" }, esc: "back" };
                let title = if default { "Default model" } else { "Remove listed model" };
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
        Item::new("Inspect loaded plugins", "Browse versions, extensions and loading errors"),
        Item::new("Manage installed plugin", "Enable, disable, configure or uninstall a selected plugin"),
        Item::new("Install plugin", "Enter plugin@marketplace to install and configure it"),
        Item::new("Update plugins", "Fetch the latest versions of configured plugins and marketplaces"),
        Item::new("Marketplaces", "Browse the marketplaces configured in settings"),
    ];
    loop {
        let about = "Changes apply after /reload-plugins";
        let action = dialog::choose(out, "Plugins", about, SELECT, actions.clone(), 0).await?;
        match action {
            0 => {
                let items = env.ext.plugins.list.iter().map(|p| {
                    let outcome = p.outcome.as_ref().map_or_else(|e| e.clone(), |parts| parts.join(", "));
                    let version = format!("Version: {}", p.version.as_deref().unwrap_or("—"));
                    let ignored = p.ignored.iter().map(|i| format!("Not used: {i}"));
                    let details = [version, outcome].into_iter().chain(p.warnings.clone()).chain(ignored);
                    Item::new(&p.id, p.description.as_deref().unwrap_or("")).details(details)
                });
                dialog::inspect(out, "Loaded plugins", "", items.collect()).await;
            }
            1 => {
                let settings = load_settings(out)?;
                let loaded = env.ext.plugins.list.iter().map(|p| p.id.clone());
                let ids: BTreeSet<_> = settings.enabled_plugins.keys().cloned().chain(loaded).collect();
                let ids: Vec<_> = ids.into_iter().collect();
                let state =
                    |id: &String| if settings.enabled_plugins.get(id) == Some(&false) { "disabled" } else { "enabled" };
                let items = ids.iter().map(|id| Item::new(id, state(id))).collect();
                let Some(i) = dialog::choose(out, "Installed plugins", "", BACK, items, 0).await else { continue };
                let id = &ids[i];
                let actions = ["Enable", "Disable", "Configure", "Uninstall (keep data)"];
                let items = actions.iter().map(|label| Item::new(*label, format!("Applies to {id}"))).collect();
                let keys = Keys { enter: "run", esc: "back" };
                let Some(action) = dialog::choose(out, id, "", keys, items, 0).await else { continue };
                let command = ["enable", "disable", "configure", "uninstall"][action];
                let mut args = vec![command.into(), id.clone()];
                if action == 3 {
                    args.push("--keep-data".into());
                }
                return Some(Input::ManagePlugins(args));
            }
            2 => {
                let about = "Plugin identity: plugin@marketplace";
                let Some(spec) = dialog::input(out, "Install plugin", about).await else { continue };
                return Some(Input::ManagePlugins(vec!["install".into(), spec]));
            }
            3 => return Some(Input::Plugins { update: true }),
            4 => {
                let settings = load_settings(out)?;
                let names = settings.extra_known_marketplaces.keys();
                let items = names.map(|name| Item::new(name, "Configured marketplace")).collect();
                dialog::inspect(out, "Marketplaces", "Manage with /plugins marketplace add|remove", items).await;
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
    fn commands_that_leave_the_conversation_run_during_a_turn() {
        let now = ["/model", "/model gw/m", "/effort high", "/permissions plan", "/tasks", "/stop", "/help", "/models"];
        for line in now {
            assert!(runs_during_turn(&parse_input(line, &[])), "{line}");
        }
        let later = ["/clear", "/compact", "/rewind", "/resume", "/usage", "/btw why", "/reload-plugins", "/exit", "!ls"];
        for line in later {
            assert!(!runs_during_turn(&parse_input(line, &[])), "{line}");
        }
    }
}
