#![cfg(not(target_family = "wasm"))]

mod actions;
mod ansi;
mod app;
mod approval;
mod batch;
mod cli;
mod commands;
mod config;
mod credentials;
#[cfg(unix)]
mod cua;
mod events;
mod extras;
mod focus;
mod history;
mod host;
mod login;
mod models;
mod permissions;
mod sandbox;
mod sessions;
mod side;
mod split;
mod stream;
mod telemetry;
mod view;

use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use futures_util::StreamExt;
use nanocode::{ModelSelection, ServiceTier, Thinking, durability::TranscriptItem};

use crate::{
    cli::{Cli, Command, HandArgs, HarnessArg, SessionArgs, SessionsArgs},
    host::{Host, HostConfig, Result},
};

pub use host::{ModelClients, Panel};

const DIFF_PREVIEW_LINES: usize = 10;
const TELEMETRY_SHUTDOWN: std::time::Duration = std::time::Duration::from_millis(500);

fn cli_home(cli: &Cli) -> Result<PathBuf> {
    let explicit_home = match &cli.command {
        Some(Command::Exec(args)) => args.home.as_deref(),
        Some(Command::Sessions(args)) => args.home.as_deref(),
        _ => None,
    }
    .or(cli.session.home.as_deref());
    host::home_dir(explicit_home)
}

fn install_telemetry(
    cli: &Cli,
    home: &Path,
) -> Result<Option<nanocode_observability::ObservabilityGuard>> {
    let args = &cli.observability;
    let interactive = !matches!(
        cli.command,
        Some(
            Command::Exec(_)
                | Command::Hand(_)
                | Command::Sessions(_)
                | Command::Models
                | Command::Clean
        )
    ) && !is_cua(cli);
    let output = match (args.log_filter().is_some(), interactive) {
        (false, _) => nanocode_observability::LogOutput::Disabled,
        (true, false) => nanocode_observability::LogOutput::Stderr,
        (true, true) => {
            nanocode_observability::LogOutput::File(home.join("log").join("nanocode.log"))
        }
    };
    Ok(Some(
        args.builder("nanocode", env!("CARGO_PKG_VERSION"), output)
            .otlp_directory(home.join("log").join("otel"))
            .shutdown_timeout(TELEMETRY_SHUTDOWN)
            .install()?,
    ))
}

#[cfg(unix)]
const fn is_cua(cli: &Cli) -> bool {
    matches!(cli.command, Some(Command::Cua(_)))
}

#[cfg(not(unix))]
const fn is_cua(_: &Cli) -> bool {
    false
}

pub fn run() -> ExitCode {
    run_with(ModelClients::default())
}

pub fn run_with(clients: ModelClients) -> ExitCode {
    let entered = std::time::Instant::now();
    let cli = Cli::parse();
    let parsed = entered.elapsed();
    let home = match cli_home(&cli) {
        Ok(home) => home,
        Err(error) => {
            eprintln!("nanocode: failed to resolve the home directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let telemetry_started = std::time::Instant::now();
    let telemetry = match install_telemetry(&cli, &home) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            eprintln!("nanocode: failed to start telemetry: {error}");
            return ExitCode::FAILURE;
        }
    };
    let startup = tracing::info_span!(
        target: "nanocode",
        "nc.startup",
        process.pid = std::process::id(),
        cli.duration_ms = parsed.as_secs_f64() * 1_000.0,
        telemetry.duration_ms = telemetry_started.elapsed().as_secs_f64() * 1_000.0,
        main_to_span.duration_ms = entered.elapsed().as_secs_f64() * 1_000.0,
        runtime.duration_ms = tracing::field::Empty
    );
    let runtime_started = std::time::Instant::now();
    let runtime = match startup.in_scope(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
    }) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("nanocode: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    startup.record(
        "runtime.duration_ms",
        runtime_started.elapsed().as_secs_f64() * 1_000.0,
    );
    drop(startup);
    if matches!(
        cli.command,
        None | Some(Command::Exec(_) | Command::Resume { .. })
    ) && let Some(warming) = clients.warming()
    {
        runtime.spawn(warming);
    }
    let outcome = runtime.block_on(async move {
        match cli.command {
            Some(Command::Hand(args)) => hand(args).await,
            #[cfg(unix)]
            Some(Command::Cua(args)) => cua::cua(args).await,
            Some(Command::Sessions(mut args)) => {
                args.home = args.home.or(cli.session.home);
                list_sessions(args).await
            }
            Some(Command::Exec(args)) => exec(args.or(cli.session), clients).await,
            Some(Command::Clean) => clean(&home),
            Some(Command::Login { provider }) => login_provider(&home, &provider),
            Some(Command::Models) => {
                print_models();
                Ok(())
            }
            Some(Command::Resume { id }) => {
                let mut args = cli.session;
                args.resume = Some(id.unwrap_or_default());
                interactive(args, clients).await
            }
            None => interactive(cli.session, clients).await,
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    drop(telemetry);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nanocode: {error}");
            ExitCode::FAILURE
        }
    }
}

struct Prepared {
    host: Host,
    model: ModelSelection,
    thinking: Thinking,
    tier: ServiceTier,
    home: PathBuf,
    approvals: tokio::sync::mpsc::UnboundedReceiver<permissions::Message>,
}

async fn prepare(
    args: &SessionArgs,
    clients: ModelClients,
    notify: &mut dyn FnMut(&str),
    headless: bool,
) -> Result<Prepared> {
    let workspace = match &args.workspace {
        Some(path) => std::fs::canonicalize(path)
            .map_err(|error| format!("workspace {}: {error}", path.display()))?,
        None => std::env::current_dir()?,
    };
    let home = host::home_dir(args.home.as_deref())?;
    let config = config::Config::load(&home)?;
    host::configure_models(&config)?;
    let requested = args.model.clone().or_else(|| {
        args.harness
            .is_none()
            .then(|| config.model.clone())
            .flatten()
    });
    let codex_requested = args.harness == Some(HarnessArg::Codex)
        || requested.as_deref().is_some_and(|model| {
            host::parse_model(model).is_ok_and(|model| model.as_codex().is_some())
        });
    let gateway = match (
        host::gateway(&args.gateway)?,
        requested
            .as_deref()
            .and_then(|model| config.provider_for(model)),
    ) {
        (Some(gateway), _) => Some(gateway),
        (None, Some((name, provider))) => Some(host::Gateway::provider(name, provider, &home)?),
        (None, None) if codex_requested || clients.supplies_openai() => None,
        (None, None) => config
            .providers
            .iter()
            .find(|(name, provider)| provider.key(name, &home).is_ok())
            .map(|(name, provider)| host::Gateway::provider(name, provider, &home))
            .transpose()?,
    };
    if gateway.is_some() && clients.supplies_openai() {
        return Err("this binary supplies its own Codex client; --gateway is unavailable".into());
    }
    let model = match (&requested, args.harness) {
        (Some(model), harness) => {
            let model = host::parse_model(model)?;
            if let Some(harness) = harness
                && family(harness) != model.family()
            {
                return Err(format!(
                    "model {model} does not belong to the {} harness",
                    family(harness)
                )
                .into());
            }
            model
        }
        (None, Some(harness)) => family(harness).default_model(),
        (None, None) => nanocode::HarnessFamily::Claude.default_model(),
    };
    if let Some(gateway) = &gateway {
        gateway.check(&model)?;
    }
    let defaulted = requested.is_none() && args.harness.is_none();
    let capabilities = model.capabilities(nanocode::ModelTransport::Native);
    let thinking = match (
        &args.thinking,
        host::configured_thinking(&model),
        &config.thinking,
    ) {
        (Some(value), _, _) => host::parse_thinking(value)?,
        (None, Some(thinking), _) => thinking,
        (None, None, Some(value)) => host::parse_thinking(value)?,
        (None, None, None) if defaulted && capabilities.supports_thinking(Thinking::Xhigh) => {
            Thinking::Xhigh
        }
        (None, None, None) => model.default_thinking(),
    };
    host::check_thinking(&model, thinking)?;
    let tier = match &args.tier {
        Some(value) => host::parse_tier(value)?,
        None => ServiceTier::Standard,
    };
    model
        .capabilities(nanocode::ModelTransport::Native)
        .check_service_tier(tier)?;
    let store = if args.ephemeral {
        None
    } else {
        Some(host::open_store(&home).await?)
    };
    let (runners, runner_servers) = host::runner_configs(&args.remote)?;
    let mode = config.permissions.unwrap_or_default();
    let (permissions, approvals) = permissions::Permissions::new(
        mode,
        config.batch && !headless,
        workspace.clone(),
        &home,
        &sandbox::base(config.sandbox_dir.as_deref()),
    )?;
    let host_config = HostConfig {
        workspace,
        store,
        subagents: !args.no_subagents,
        runners,
        runner_servers,
        gateway,
        clients,
        headless,
        permissions: Some(permissions),
        diff_tool: config.diff_tool,
        diff_viewer: config.diff_viewer,
        computer_use: args.computer_use || config.computer_use,
        diff_preview_lines: config.diff_preview_lines.unwrap_or(DIFF_PREVIEW_LINES),
        auto_diff_tool: config.auto_diff_tool,
        providers: config.providers.clone(),
    };
    let config = host_config;
    let host = tracing::Instrument::instrument(
        Host::build(config, &model, thinking),
        tracing::info_span!(target: "nanocode", "nc.host.build"),
    )
    .await?;
    tracing::Instrument::instrument(
        host.runtime.initialize_runners(),
        tracing::info_span!(target: "nanocode", "nc.runners.initialize"),
    )
    .await?;
    for server in host.runtime.runner_servers() {
        notify(&format!(
            "listening {} · token {}",
            server.endpoint(),
            server.token()
        ));
    }
    Ok(Prepared {
        host,
        model,
        thinking,
        tier,
        home,
        approvals,
    })
}

const fn family(harness: HarnessArg) -> nanocode::HarnessFamily {
    match harness {
        HarnessArg::Codex => nanocode::HarnessFamily::Codex,
        HarnessArg::Claude => nanocode::HarnessFamily::Claude,
    }
}

async fn interactive(mut args: SessionArgs, clients: ModelClients) -> Result<()> {
    let reload = match &args.reload_state {
        Some(path) => Some(app::reload::read(path).await?),
        None => None,
    };
    if let Some(state) = &reload {
        args.resume = if state.moved {
            Some(String::new())
        } else {
            state.session.clone()
        };
        args.prompt = None;
        args.continue_last = false;
        args.model = Some(state.model.to_string());
        args.harness = None;
        args.thinking = Some(state.thinking.to_string());
        args.tier = Some(state.tier.as_str().to_owned());
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(
            "the interactive interface needs a terminal; use `nanocode exec <prompt>` for scripts"
                .into(),
        );
    }
    ensure_provider_key(&args)?;
    let terminal =
        nanocode_tui::terminal::TerminalSession::enter(nanocode_tui::terminal::TerminalOptions {
            mouse_capture: !args.no_mouse,
            keyboard_enhancement: true,
        })?;
    nanocode_tui::highlight::prewarm();
    let prepared = prepare(&args, clients, &mut |_| {}, false).await;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            drop(terminal);
            return Err(error);
        }
    };
    let (resume, pick_session) = match args.resume.as_deref() {
        Some("") => (None, true),
        Some(id) => (Some(id.to_owned()), false),
        None if args.continue_last => match latest_session(&prepared.host).await {
            Ok(id) => (Some(id), false),
            Err(error) => {
                drop(terminal);
                return Err(error);
            }
        },
        None => (None, false),
    };
    if (resume.is_some() || pick_session) && prepared.host.config.store.is_none() {
        drop(terminal);
        return Err("--resume and --continue require durability; remove --ephemeral".into());
    }
    let permissions = prepared.host.config.permissions.clone();
    let outcome = app::run(
        app::Launch {
            host: prepared.host,
            model: prepared.model,
            thinking: prepared.thinking,
            tier: prepared.tier,
            resume,
            pick_session,
            prompt: args.prompt,
            approvals: Some(prepared.approvals),
            history_path: Some(prepared.home.join("history.jsonl")),
            home: prepared.home.clone(),
            thinking_override: args
                .thinking
                .as_deref()
                .map(host::parse_thinking)
                .transpose()?,
            tier_override: args.tier.as_deref().map(host::parse_tier).transpose()?,
            reload,
            reload_path: args.reload_state,
        },
        terminal,
    )
    .await;
    if let Some(permissions) = permissions {
        permissions.cleanup();
    }
    outcome
}

async fn latest_session(host: &Host) -> Result<String> {
    let store = host
        .config
        .store
        .as_ref()
        .ok_or("--continue requires durability")?;
    store
        .list()
        .await?
        .into_iter()
        .find(|session| {
            sessions::user_facing(session.record.lineage.origin)
                && session.record.workspace.as_deref() == Some(host.config.workspace.as_path())
        })
        .map(|session| session.record.session_id)
        .ok_or_else(|| "no saved session for this workspace".into())
}

async fn exec(args: SessionArgs, clients: ModelClients) -> Result<()> {
    let prompt = args
        .prompt
        .clone()
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or("exec requires a prompt")?;
    ensure_provider_key(&args)?;
    let prepared = prepare(
        &args,
        clients,
        &mut |message| {
            eprintln!("nanocode: {message}");
        },
        true,
    )
    .await?;
    let host = prepared.host;
    tokio::spawn(answer_in_terminal(prepared.approvals));
    let resume = match (&args.resume, args.continue_last) {
        (Some(id), _) if !id.is_empty() => Some(id.clone()),
        (_, true) => Some(latest_session(&host).await?),
        _ => None,
    };
    let (agent, mut events, pending) = match resume {
        Some(id) => {
            let ready = app::open_session(
                &host,
                &id,
                args.thinking
                    .as_deref()
                    .map(host::parse_thinking)
                    .transpose()?,
                args.tier.as_deref().map(host::parse_tier).transpose()?,
            )
            .await?;
            (ready.agent, ready.events, ready.pending)
        }
        None => {
            let (agent, events) = host.start(&prepared.model, prepared.thinking).await?;
            app::apply_tier(&agent, &prepared.model, prepared.tier).await?;
            (agent, events, Vec::new())
        }
    };
    if let Some(reason) = host.runtime.computer_use_unavailable() {
        eprintln!("nanocode: computer use unavailable, continuing without it: {reason}");
    }
    tokio::spawn(async move { while events.recv().await.is_some() {} });
    if let Some(mut notices) = host.config.clients.notifying() {
        tokio::spawn(async move {
            while let Some(notice) = notices.next().await {
                eprintln!("{notice}");
            }
        });
    }
    let outcome: Result<()> = async {
        for pending in pending {
            eprintln!("nanocode: resuming interrupted turn {}", pending.id);
            let turn = agent
                .prompt(nanocode::PromptRequest::new(pending.prompt).request_id(&pending.id))
                .await?;
            stream_turn(turn, std::io::stderr()).await?;
        }
        stream_turn(agent.prompt(prompt.as_str()).await?, std::io::stdout()).await
    }
    .await;
    let mut stderr = std::io::stderr();
    if let Some(status) = host.config.clients.describe() {
        drop(writeln!(stderr, "{status}"));
    }
    if let Some(panel) = host.config.clients.render_panel() {
        drop(writeln!(stderr, "{}", panel.title));
        for row in panel.aligned() {
            drop(writeln!(stderr, "  {}", row.join("  ").trim_end()));
        }
    }
    drop(writeln!(stderr, "session {}", agent.session_id()));
    let flushed = agent.flush().await;
    let shutdown = host.runtime.shutdown().await;
    if let Some(permissions) = &host.config.permissions {
        permissions.cleanup();
    }
    outcome?;
    flushed?;
    shutdown?;
    Ok(())
}

async fn stream_turn(mut turn: nanocode::agent::Turn, answers: impl Write) -> Result<()> {
    let mut printer = stream::Printer::new(answers);
    while let Some(event) = turn.next().await {
        printer.event(&event)?;
    }
    let result = turn.await?;
    printer.finish(result.final_message())?;
    Ok(())
}

async fn list_sessions(args: SessionsArgs) -> Result<()> {
    let home = host::home_dir(args.home.as_deref())?;
    let store = host::open_store(&home).await?;
    if let Some(id) = &args.session {
        for item in store.load(id).await?.transcript {
            match item {
                TranscriptItem::User(text) => println!("## user\n{text}\n"),
                TranscriptItem::Assistant(text) => println!("## assistant\n{text}\n"),
                _ => {}
            }
        }
        store.flush().await?;
        return Ok(());
    }
    let workspace = std::env::current_dir()?;
    for session in store.list().await? {
        let record = &session.record;
        if !sessions::user_facing(record.lineage.origin) {
            continue;
        }
        if !args.all && record.workspace.as_deref() != Some(workspace.as_path()) {
            continue;
        }
        println!(
            "{}  {:>8}  {:<20}  {}",
            record.session_id,
            sessions::age(record.updated_at_ms),
            record.model.to_string(),
            sessions::title(&session)
        );
    }
    store.flush().await?;
    Ok(())
}

fn print_models() {
    for (family, models) in models::catalog() {
        println!("{}", app::family_label(&family));
        for model in models {
            let capabilities = model.capabilities(nanocode::ModelTransport::Native);
            let thinking = capabilities
                .thinking()
                .map(Thinking::as_str)
                .collect::<Vec<_>>()
                .join("/");
            let features = models::features(&model);
            println!(
                "  {:<20} thinking {:<30} default {:<7} {}",
                model.to_string(),
                thinking,
                capabilities.default_thinking().as_str(),
                features
            );
        }
    }
}

async fn hand(args: HandArgs) -> Result<()> {
    use nanocode::hand::{Hand, HandEvent};
    let directory = match &args.directory {
        Some(path) => std::fs::canonicalize(path)?,
        None => std::env::current_dir()?,
    };
    let mut tools = nanocode::hand::tools(directory.clone())?.into_builder();
    if args.cua {
        let computer =
            nanocode::ComputerTools::connect(nanocode::ComputerConfig::detect()?).await?;
        tools = tools.tool(computer.js()).tool(computer.reset());
    }
    let tools = tools.build()?;
    let name = match args.name {
        Some(name) => name,
        None => Hand::machine_name()?,
    };
    let hand = match (args.url, args.listen) {
        (Some(url), _) => Hand::connect(url, args.token, tools),
        (None, Some(address)) => Hand::listen(address, args.token, tools),
        (None, None) => return Err("pass the brain URL or --listen <ADDR>".into()),
    }
    .name(&name)
    .directory(directory.clone());
    eprintln!("nanocode hand {name} in {}", directory.display());
    let run = hand.run(|event| match event {
        HandEvent::Connecting { endpoint } => eprintln!("hand: connecting {endpoint}"),
        HandEvent::Connected { endpoint } => eprintln!("hand: connected to {endpoint}"),
        HandEvent::Listening { endpoint } => eprintln!("hand: listening {endpoint}"),
        HandEvent::BrainConnected { peer } => eprintln!("hand: brain connected from {peer}"),
        HandEvent::Disconnected { reason } => eprintln!("hand: disconnected ({reason})"),
        HandEvent::Retrying { delay } => eprintln!("hand: retrying in {}ms", delay.as_millis()),
        HandEvent::Revoked => eprintln!("hand: the brain disconnected this hand; stopping"),
        HandEvent::Replaced => eprintln!("hand: another hand attached under this name; stopping"),
        other => eprintln!("hand: {other:?}"),
    });
    tokio::select! {
        result = run => result?,
        signal = tokio::signal::ctrl_c() => signal?,
    }
    Ok(())
}

async fn answer_in_terminal(
    mut approvals: tokio::sync::mpsc::UnboundedReceiver<permissions::Message>,
) {
    while let Some(message) = approvals.recv().await {
        let mut request = match message {
            permissions::Message::Approval(request) => request,
            permissions::Message::Original(recorded) => {
                recorded.discard();
                continue;
            }
        };
        let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        let mut stderr = std::io::stderr();
        drop(writeln!(stderr, "\n{}: {}", request.title, request.detail));
        if !request.diff.is_empty() {
            drop(writeln!(stderr, "{}", request.diff));
        }
        for path in &request.elsewhere {
            drop(writeln!(stderr, "  also changes {path}"));
        }
        if !interactive {
            drop(writeln!(
                stderr,
                "denied: no terminal to ask for permission"
            ));
            request.answer(false);
            continue;
        }
        drop(write!(stderr, "Approve? [y/N] "));
        drop(stderr.flush());
        let line = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        })
        .await;
        let approved = matches!(line, Ok(Ok(line)) if line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes"));
        request.answer(approved);
    }
}

fn clean(home: &Path) -> Result<()> {
    let config = config::Config::load(home)?;
    let (removed, live) = sandbox::remove_stale(&[
        sandbox::base(config.sandbox_dir.as_deref()),
        std::env::temp_dir(),
    ]);
    let mut failed = false;
    for (path, result) in &removed {
        match result {
            Ok(()) => println!("removed {}", path.display()),
            Err(error) => {
                failed = true;
                eprintln!("could not remove {}: {error}", path.display());
            }
        }
    }
    if removed.is_empty() {
        println!("nothing to clean");
    }
    if live > 0 {
        println!("kept {live} directories of running sessions");
    }
    if failed {
        return Err("some directories could not be removed".into());
    }
    Ok(())
}

fn login_provider(home: &Path, provider: &str) -> Result<()> {
    let config = config::Config::load(home)?;
    let path = credentials::login(&config, home, provider)?;
    println!("saved the {provider} key to {}", path.display());
    Ok(())
}

fn ensure_provider_key(args: &SessionArgs) -> Result<()> {
    let home = host::home_dir(args.home.as_deref())?;
    let config = config::Config::load(&home)?;
    let model = args.model.clone().or_else(|| {
        args.harness
            .is_none()
            .then(|| config.model.clone())
            .flatten()
    });
    credentials::ensure_key(&config, &home, model.as_deref())
}
