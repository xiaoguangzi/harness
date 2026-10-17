//! Interactive entry points for commands that used to print a list or usage text.

use std::collections::BTreeSet;

use super::{Input, Output, BUILTINS, effort_help, output};
use super::dialog::{self, Item, Keys};
use super::input::Typing;
use super::text;
use crate::{agent::Agent, llm, settings::Settings, setup::Env, skills::Skill};

const SELECT: Keys = Keys { enter: "select", esc: "cancel" };
const BACK: Keys = Keys { enter: "select", esc: "back" };

/// Returns the selected action, or `None` when the command was handled/cancelled.
/// Commands with explicit arguments bypass these menus in the regular dispatcher.
pub async fn interactive(
    input: Input, agent: &Agent, env: &Env, skills: &[Skill], out: &Output, typing: &mut Typing,
) -> Option<Input> {
    match input {
        Input::Help => {
            let mut items: Vec<_> = BUILTINS.iter().map(|(name, description)| Item::new(*name, *description)).collect();
            items.extend(skills.iter().filter(|s| s.user_invocable).map(|s| Item::new(format!("/{}", s.name), &s.description)));
            let keyboard = items.len();
            let keys: Vec<_> = super::help_keys().iter().map(|(key, description)| format!("{key}: {description}")).collect();
            items.push(Item::new("Keyboard shortcuts", "Browse composer, interrupt and rewind shortcuts").details(keys.clone()));
            while let Some(i) = dialog::choose(out, "Commands", "Built-in commands, skills and keyboard shortcuts", Keys { enter: "insert", esc: "cancel" }, items.clone(), 0).await {
                if i == keyboard { dialog::document(out, "Keyboard shortcuts", keys.clone()).await; }
                else { typing.draft.set_text(&items[i].label); break; }
            }
            None
        }
        Input::Effort(None) => {
            let mut items = vec![Item::new("auto", format!("Use configured effort (currently {})", agent.client().effort().as_str()))];
            items.extend(llm::Effort::ALL.iter().map(|e| Item::new(e.as_str(), effort_help(*e))));
            let initial = agent.client().effort_override().and_then(|e| llm::Effort::ALL.iter().position(|level| *level == e)).map_or(0, |i| i + 1);
            let i = dialog::choose(out, "Reasoning effort", "Applies to this session; /model changes the same setting", Keys { enter: "apply", esc: "cancel" }, items, initial).await?;
            Some(Input::Effort(Some(i.checked_sub(1).map(|i| llm::Effort::ALL[i]))))
        }
        Input::ListSkills => {
            let items = skills.iter().map(|s| Item::new(format!("/{}", s.name), &s.description).details(vec![
                format!("Source: {}", s.dir.display()),
                format!("User invocable: {} · model invocable: {}", s.user_invocable, s.model_invocable),
                format!("Runs in a subagent: {}", s.fork.is_some()),
            ])).collect();
            if let Some(i) = dialog::choose(out, "Skills", "Discovered skills; only user-invocable ones go into the prompt", Keys { enter: "insert", esc: "cancel" }, items, 0).await
                && skills[i].user_invocable {
                typing.draft.set_text(&format!("/{} ", skills[i].name));
            }
            None
        }
        Input::ListTools => {
            let items = agent.tools().definitions().into_iter().map(|t| {
                let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
                Item::new(t.name, t.description).details(std::iter::once("Input schema:".into()).chain(schema.lines().map(String::from)))
            }).collect();
            dialog::inspect(out, "Tools", "What the model can call, with input schemas", items).await;
            None
        }
        Input::ListAgents => {
            let items = env.ext.agent_defs.iter().map(|d| Item::new(&d.name, &d.description).details(vec![
                format!("Access: {}", d.permission.as_str()),
                format!("Tools: {}", d.tools.as_ref().map_or("all permitted".into(), |t| t.join(", "))),
                format!("Model: {}", d.model.as_deref().or(agent.client().subagent_model()).unwrap_or(agent.model())),
                format!("Source: {}", d.path.as_ref().map_or("built-in".into(), |p| p.display().to_string())),
                d.prompt.clone(),
            ])).collect();
            dialog::inspect(out, "Agent types", "Subagent types the model can start", items).await;
            None
        }
        Input::ListMcp => {
            let items = env.mcp_statuses().map(|s| {
                let (description, details) = match &s.outcome {
                    Ok(tools) => (format!("Connected · {} {}", tools.len(), text::plural(tools.len(), "tool")), tools.clone()),
                    Err(e) => ("Connection failed".into(), vec![e.clone()]),
                };
                Item::new(&s.name, description).details(details.into_iter().chain(std::iter::once(format!("Log: {}", crate::mcp::log_path(&s.name).display()))))
            }).collect();
            dialog::inspect(out, "MCP servers", "Servers, their tools and connection errors", items).await;
            None
        }
        Input::Usage => {
            let mut items = vec![Item::new("Current context", format!("{} / {} tokens (about {}%)", output::tokens(agent.context_tokens()), output::tokens(agent.client().context_window()), agent.context_tokens().saturating_mul(100) / agent.client().context_window().max(1)))];
            items.extend(agent.usage().models().into_iter().map(|(model, t)| Item::new(model, format!("{} {} · {} output tokens", t.requests, text::plural(t.requests as usize, "request"), output::tokens(t.usage.output_tokens))).details(vec![
                format!("Input: {}", t.usage.input_tokens), format!("Cache reads: {}", t.usage.cache_read_tokens),
                format!("Cache writes: {}", t.usage.cache_write_tokens), format!("Output: {}", t.usage.output_tokens),
            ])));
            dialog::inspect(out, "Usage", "This conversation, by model", items).await;
            None
        }
        Input::Sandbox => {
            let items = crate::sandbox::active().map_or_else(
                || vec![Item::new("Sandbox is off", "Start with --sandbox to enable it; this view does not change permissions")],
                |s| s.describe().into_iter().map(|(label, value)| Item::new(label, value)).collect(),
            );
            dialog::inspect(out, "Sandbox", "What commands may do; read-only", items).await;
            None
        }
        Input::Stop(None) => stop(agent, out).await,
        Input::Tasks => tasks(agent, out).await,
        Input::Providers(args) if args.is_empty() => providers(agent, out).await,
        Input::Plugins { update: false } => plugins(env, out).await,
        input => Some(input),
    }
}

fn task_items(agent: &Agent, running_only: bool) -> Vec<(String, Item, bool)> {
    let mut items = Vec::new();
    for s in agent.subagents().list() {
        let running = s["status"] == "running";
        if running_only && !running { continue; }
        let id = s["id"].as_str().unwrap_or("").to_string();
        let item = Item::new(format!("{id} · {}", s["label"].as_str().unwrap_or("")), format!("{} · {}", s["status"].as_str().unwrap_or(""), s["model"].as_str().unwrap_or("")))
            .details(vec![format!("Tool calls: {} · time: {}", s["tool_calls"], output::elapsed(s["seconds"].as_u64().unwrap_or(0))),
                format!("Latest call: {}", s["last_call"].as_str().unwrap_or("—")), format!("Background: {}", s["background"])]);
        items.push((id, item, running));
    }
    for (id, command, running) in agent.shells().list() {
        if running_only && !running { continue; }
        let item = Item::new(format!("{id} · background command"), if running { "running" } else { "ended" }).details(command.lines().map(String::from));
        items.push((id, item, running));
    }
    items
}

async fn stop(agent: &Agent, out: &Output) -> Option<Input> {
    let tasks = task_items(agent, true);
    if tasks.is_empty() {
        out.line(out.paint("2", "no running tasks"));
        return None;
    }
    let mut items: Vec<_> = tasks.iter().map(|(_, item, _)| item.clone()).collect();
    items.push(Item::new("Stop all running tasks", "Stop every running subagent and background command in this conversation"));
    let i = dialog::choose(out, "Stop tasks", "Running subagents and background commands", Keys { enter: "stop", esc: "cancel" }, items, 0).await?;
    Some(Input::Stop(tasks.get(i).map(|(id, _, _)| id.clone())))
}

async fn tasks(agent: &Agent, out: &Output) -> Option<Input> {
    loop {
        let tasks = task_items(agent, false);
        if tasks.is_empty() {
            out.line(out.paint("2", "no subagents or background commands in this conversation"));
            return None;
        }
        let i = dialog::choose(out, "Tasks", "Subagents and background commands in this conversation", Keys { enter: "open", esc: "cancel" }, tasks.iter().map(|(_, item, _)| item.clone()).collect(), 0).await?;
        let (id, item, running) = &tasks[i];
        let mut actions = vec![Item::new("View details", &item.description)];
        if *running { actions.push(Item::new("Stop this task", format!("Stop {id}"))); }
        let Some(action) = dialog::choose(out, &format!("Task {id}"), "", BACK, actions, 0).await else { continue };
        if action == 1 { return Some(Input::Stop(Some(id.clone()))); }
        dialog::document(out, &item.label, std::iter::once(item.description.clone()).chain(item.details.clone()).collect()).await;
    }
}

fn provider_items(agent: &Agent) -> Vec<Item> {
    let providers = agent.client().providers();
    providers.names().into_iter().map(|name| {
        let config = providers.provider(name).unwrap();
        Item::new(name, format!("{} · {} listed {}", config.protocol.as_str(), config.models.len(), text::plural(config.models.len(), "model"))).details(
            std::iter::once(format!("API key: {}", if config.api_key.is_some() { "configured" } else { "not set" }))
                .chain(config.models.keys().map(|m| format!("{name}/{m}"))),
        )
    }).collect()
}

async fn providers(agent: &Agent, out: &Output) -> Option<Input> {
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
        let action = dialog::choose(out, "Providers", "Changes are saved to the settings file", SELECT, actions.clone(), 0).await?;
        match action {
            0 => dialog::inspect(out, "Provider details", "", provider_items(agent)).await,
            1 => {
                let Some(name) = dialog::input(out, "Add provider", "Provider name (letters, digits, dots, underscores or hyphens)").await else { continue };
                return Some(Input::Providers(vec!["add".into(), name]));
            }
            4 => {
                let names: Vec<_> = load_settings(out)?.providers.into_keys().collect();
                let items = names.iter().map(|name| Item::new(name, "Choose the provider for this model")).collect();
                let Some(i) = dialog::choose(out, "Add model: provider", "Providers in the settings file", BACK, items, 0).await else { continue };
                let Some(model) = dialog::input(out, "Add model", "Model name accepted by the selected gateway").await else { continue };
                return Some(Input::Providers(vec!["model".into(), "add".into(), format!("{}/{model}", names[i])]));
            }
            2 | 6 => {
                let names: Vec<_> = load_settings(out)?.providers.into_keys().collect();
                let items = names.iter().map(|n| Item::new(n, if action == 2 { "Edit settings; the API key stays hidden" } else { "Removes this provider from settings" })).collect();
                let keys = if action == 2 { Keys { enter: "edit", esc: "back" } } else { Keys { enter: "remove", esc: "back" } };
                let Some(i) = dialog::choose(out, if action == 2 { "Edit provider" } else { "Remove provider" }, "Providers in the settings file", keys, items, 0).await else { continue };
                return Some(Input::Providers(vec![if action == 2 { "add" } else { "remove" }.into(), names[i].clone()]));
            }
            3 | 5 => {
                // A model can be removed only from the providers in the
                // settings file, not from those set up by environment variables.
                let models = if action == 3 { agent.client().providers().listed_models() } else {
                    load_settings(out)?.providers.into_iter()
                        .flat_map(|(name, p)| p.models.into_keys().map(move |m| format!("{name}/{m}"))).collect()
                };
                let items = models.iter().map(|m| Item::new(m, if action == 3 { "Saves this default model" } else { "Removes this model from the list" })).collect();
                let keys = if action == 3 { Keys { enter: "save", esc: "back" } } else { Keys { enter: "remove", esc: "back" } };
                let Some(i) = dialog::choose(out, if action == 3 { "Default model" } else { "Remove listed model" }, "", keys, items, 0).await else { continue };
                let args = if action == 3 { vec!["default".into(), models[i].clone()] } else { vec!["model".into(), "remove".into(), models[i].clone()] };
                return Some(Input::Providers(args));
            }
            _ => unreachable!(),
        }
    }
}

async fn plugins(env: &Env, out: &Output) -> Option<Input> {
    let actions = vec![Item::new("Inspect loaded plugins", "Browse versions, extensions and loading errors"),
        Item::new("Manage installed plugin", "Enable, disable, configure or uninstall a selected plugin"),
        Item::new("Install plugin", "Enter plugin@marketplace to install and configure it"),
        Item::new("Update plugins", "Fetch the latest versions of configured plugins and marketplaces"),
        Item::new("Marketplaces", "Browse the marketplaces configured in settings")];
    loop {
        let action = dialog::choose(out, "Plugins", "Changes apply after /reload-plugins", SELECT, actions.clone(), 0).await?;
        match action {
            0 => {
                let items = env.ext.plugins.list.iter().map(|p| {
                    let outcome = p.outcome.as_ref().map_or_else(|e| e.clone(), |parts| parts.join(", "));
                    Item::new(&p.id, p.description.as_deref().unwrap_or("")).details(
                        [format!("Version: {}", p.version.as_deref().unwrap_or("—")), outcome]
                            .into_iter().chain(p.warnings.clone()).chain(p.ignored.iter().map(|i| format!("Not used: {i}"))),
                    )
                }).collect();
                dialog::inspect(out, "Loaded plugins", "", items).await;
            }
            1 => {
                let settings = load_settings(out)?;
                let ids: BTreeSet<_> = settings.enabled_plugins.keys().cloned().chain(env.ext.plugins.list.iter().map(|p| p.id.clone())).collect();
                let ids: Vec<_> = ids.into_iter().collect();
                let items = ids.iter().map(|id| Item::new(id, if settings.enabled_plugins.get(id) == Some(&false) { "disabled" } else { "enabled" })).collect();
                let Some(i) = dialog::choose(out, "Installed plugins", "", BACK, items, 0).await else { continue };
                let id = &ids[i];
                let items = ["Enable", "Disable", "Configure", "Uninstall (keep data)"].iter().map(|label| Item::new(*label, format!("Applies to {id}"))).collect();
                let Some(action) = dialog::choose(out, id, "", Keys { enter: "run", esc: "back" }, items, 0).await else { continue };
                let command = ["enable", "disable", "configure", "uninstall"][action];
                let mut args = vec![command.into(), id.clone()];
                if action == 3 { args.push("--keep-data".into()); }
                return Some(Input::ManagePlugins(args));
            }
            2 => {
                let Some(spec) = dialog::input(out, "Install plugin", "Plugin identity: plugin@marketplace").await else { continue };
                return Some(Input::ManagePlugins(vec!["install".into(), spec]));
            }
            3 => return Some(Input::Plugins { update: true }),
            4 => {
                let settings = load_settings(out)?;
                let items = settings.extra_known_marketplaces.keys().map(|name| Item::new(name, "Configured marketplace")).collect();
                dialog::inspect(out, "Marketplaces", "Manage with /plugins marketplace add|remove", items).await;
            }
            _ => unreachable!(),
        }
    }
}

/// The settings file, or `None` after showing why it can't be read.
fn load_settings(out: &Output) -> Option<Settings> {
    Settings::load().map_err(|e| out.line(out.paint("31", &format!("{e:#}")))).ok()
}
