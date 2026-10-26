use std::{
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use nanocode::{
    Tool, ToolWrapper,
    claude_tools::contract::{
        self, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult,
    },
    process::{FileHost, ProcessFuture},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::sandbox::{self, Change, Kind, Sandbox, Try};

const WATCH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Manual,
    #[default]
    EditPerms,
    EditAccess,
    Auto,
}

impl Mode {
    pub const ALL: [Self; 4] = [Self::Manual, Self::EditPerms, Self::EditAccess, Self::Auto];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::EditPerms => "edit perms",
            Self::EditAccess => "edit access",
            Self::Auto => "auto",
        }
    }

    pub const fn describe(self) -> &'static str {
        match self {
            Self::Manual => "asks before every command and file change",
            Self::EditPerms => "asks with a diff before anything changes files in the repo",
            Self::EditAccess => "file edits apply; commands that change the repo ask with a diff",
            Self::Auto => "runs everything without asking",
        }
    }

    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    const fn index(self) -> u8 {
        match self {
            Self::Manual => 0,
            Self::EditPerms => 1,
            Self::EditAccess => 2,
            Self::Auto => 3,
        }
    }

    const fn from_index(index: u8) -> Self {
        match index {
            0 => Self::Manual,
            1 => Self::EditPerms,
            2 => Self::EditAccess,
            _ => Self::Auto,
        }
    }
}

pub struct FilePair {
    pub label: String,
    pub old: PathBuf,
    pub new: PathBuf,
}

pub struct Request {
    pub session_id: String,
    pub title: String,
    pub detail: String,
    pub command: bool,
    pub diff: String,
    pub files: Vec<FilePair>,
    pub elsewhere: Vec<String>,
    reply: Option<oneshot::Sender<bool>>,
    scratch: Option<PathBuf>,
}

impl Request {
    pub fn answer(&mut self, approved: bool) {
        if let Some(reply) = self.reply.take() {
            reply.send(approved).ok();
        }
    }

    pub fn is_stale(&self) -> bool {
        self.reply.as_ref().is_none_or(oneshot::Sender::is_closed)
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(scratch) = self.scratch.take() {
            drop(std::fs::remove_dir_all(scratch));
        }
    }
}

pub struct Permissions {
    mode: AtomicU8,
    requests: mpsc::UnboundedSender<Request>,
    workspace: PathBuf,
    sandbox: Option<Try>,
    scratch: PathBuf,
}

impl Permissions {
    pub fn new(
        mode: Mode,
        workspace: PathBuf,
        home: &Path,
    ) -> crate::host::Result<(Arc<Self>, mpsc::UnboundedReceiver<Request>)> {
        let (requests, receiver) = mpsc::unbounded_channel();
        let sandbox = Try::install(home)?;
        let scratch = std::env::temp_dir().join(format!("nanocode-review-{}", std::process::id()));
        Ok((
            Arc::new(Self {
                mode: AtomicU8::new(mode.index()),
                requests,
                workspace,
                sandbox,
                scratch,
            }),
            receiver,
        ))
    }

    pub fn mode(&self) -> Mode {
        Mode::from_index(self.mode.load(Ordering::Relaxed))
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode.index(), Ordering::Relaxed);
    }

    pub const fn sandboxed(&self) -> bool {
        self.sandbox.is_some()
    }

    pub fn cleanup(&self) {
        if let Some(sandbox) = &self.sandbox {
            sandbox.cleanup();
        }
        drop(std::fs::remove_dir_all(&self.scratch));
    }

    async fn ask(&self, mut request: Request) -> bool {
        let (reply, answer) = oneshot::channel();
        request.reply = Some(reply);
        if self.requests.send(request).is_err() {
            return false;
        }
        answer.await.unwrap_or(false)
    }

    fn request(
        &self,
        session_id: &str,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Request {
        Request {
            session_id: session_id.to_owned(),
            title: title.into(),
            detail: detail.into(),
            command: false,
            diff: String::new(),
            files: Vec::new(),
            elsewhere: Vec::new(),
            reply: None,
            scratch: None,
        }
    }

    fn label(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    async fn review(
        &self,
        session_id: &str,
        command: &str,
        sandbox: Sandbox,
    ) -> Result<Option<String>, String> {
        let changes = match sandbox.changes() {
            Ok(changes) => changes,
            Err(error) => {
                sandbox.discard();
                return Err(format!(
                    "nanocode could not read the sandbox this command ran in, so its file changes were discarded: {error}"
                ));
            }
        };
        let (inside, outside): (Vec<&Change>, Vec<&Change>) = changes
            .iter()
            .partition(|change| change.path.starts_with(&self.workspace));
        if inside.is_empty() {
            let failures = sandbox.commit(&changes);
            sandbox.discard();
            return Ok((!failures.is_empty()).then(|| commit_failures(&failures)));
        }
        let mut request = self.request(session_id, "Command changes files", command);
        request.command = true;
        let mut diffs = Vec::new();
        for change in &inside {
            let label = self.label(&change.path);
            let (old, new) = match change.kind {
                Kind::Modified => (change.path.clone(), change.upper.clone()),
                Kind::Added => (PathBuf::from("/dev/null"), change.upper.clone()),
                Kind::Deleted if !change.path.is_dir() => {
                    (change.path.clone(), PathBuf::from("/dev/null"))
                }
                _ => {
                    diffs.push(format!("# {label}: {}", change.kind.label()));
                    continue;
                }
            };
            diffs.push(sandbox::unified_diff(&old, &new, &label));
            request.files.push(FilePair { label, old, new });
        }
        request.diff = diffs.join("\n");
        request.elsewhere = outside
            .iter()
            .map(|change| format!("{} ({})", change.path.display(), change.kind.label()))
            .collect();
        let summary = inside
            .iter()
            .map(|change| format!("{} ({})", self.label(&change.path), change.kind.label()))
            .collect::<Vec<_>>()
            .join(", ");
        if self.ask(request).await {
            let failures = sandbox.commit(&changes);
            sandbox.discard();
            Ok(Some(if failures.is_empty() {
                format!(
                    "The user approved this command's file changes and they were applied: {summary}"
                )
            } else {
                commit_failures(&failures)
            }))
        } else {
            sandbox.discard();
            Err(format!(
                "The user denied this command's file changes, so none of them were applied: {summary}. Ask the user how to proceed if the change is needed."
            ))
        }
    }
}

fn commit_failures(failures: &[String]) -> String {
    format!(
        "Some of this command's file changes could not be applied: {}",
        failures.join("; ")
    )
}

pub struct Gate(pub Arc<Permissions>);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Gated {
    Command,
    FileEdit,
    Input,
}

impl ToolWrapper for Gate {
    fn wrap(&self, tool: &Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        let definition = tool.definition();
        let kind = match definition.name() {
            "Bash" | "exec_command" => Gated::Command,
            "Edit" | "Write" | "apply_patch" => Gated::FileEdit,
            "write_stdin" => Gated::Input,
            _ => return None,
        };
        Some(Arc::new(GatedTool {
            inner: Arc::clone(tool),
            definition,
            kind,
            permissions: Arc::clone(&self.0),
        }))
    }
}

struct GatedTool {
    inner: Arc<dyn Tool>,
    definition: ToolDefinition,
    kind: Gated,
    permissions: Arc<Permissions>,
}

fn copy_input(input: &ToolInput) -> ToolInput {
    match input {
        ToolInput::Function(raw) => ToolInput::Function(raw.clone()),
        ToolInput::Freeform(text) => ToolInput::Freeform(text.clone()),
    }
}

fn input_json(input: &ToolInput) -> Value {
    match input {
        ToolInput::Function(raw) => serde_json::from_str(raw.get()).unwrap_or(Value::Null),
        ToolInput::Freeform(text) => Value::String(text.clone()),
    }
}

fn denied(what: &str) -> ToolResult {
    Ok(ToolOutput::error(format!(
        "The user denied permission to {what}. Nothing was run or changed; ask the user how to proceed if this was needed."
    )))
}

fn annotate(result: ToolResult, note: Option<String>) -> ToolResult {
    let Some(note) = note else {
        return result;
    };
    let mut output = result?;
    match &mut output.output {
        contract::ToolOutputBody::Text(text) => {
            text.push_str("\n\n");
            text.push_str(&note);
        }
        contract::ToolOutputBody::Content(content) => {
            content.push(contract::ToolOutputContent::InputText { text: note.clone() });
        }
    }
    let structured = match output.structured_result() {
        Value::Object(mut object) => {
            object.insert("permission".to_owned(), Value::String(note));
            Value::Object(object)
        }
        Value::String(text) => Value::String(format!("{text}\n\n{note}")),
        other => json!({ "result": other, "permission": note }),
    };
    Ok(output.with_structured_result(structured))
}

impl GatedTool {
    async fn run(&self, input: ToolInput, context: ToolContext<'_>) -> ToolResult {
        let mode = self.permissions.mode();
        let session_id = context.session_id().to_owned();
        match (self.kind, mode) {
            (_, Mode::Auto) | (Gated::FileEdit, Mode::EditAccess) => {
                self.inner.execute(input, context).await
            }
            (Gated::Input, Mode::Manual) => {
                let detail = input_json(&input);
                let request = self.permissions.request(
                    &session_id,
                    "Send input to a running command",
                    serde_json::to_string_pretty(&detail).unwrap_or_default(),
                );
                if self.permissions.ask(request).await {
                    self.inner.execute(input, context).await
                } else {
                    denied("send input to the running command")
                }
            }
            (Gated::Input, _) => self.inner.execute(input, context).await,
            (Gated::FileEdit, _) => self.edit(input, context, &session_id).await,
            (Gated::Command, _) => self.command(input, context, &session_id, mode).await,
        }
    }

    async fn edit(
        &self,
        input: ToolInput,
        context: ToolContext<'_>,
        session_id: &str,
    ) -> ToolResult {
        let name = self.definition.name().to_owned();
        let mut request =
            self.permissions
                .request(session_id, format!("{name} files"), String::new());
        match self.preview(&input, context).await {
            Ok((diff, files, scratch)) => {
                request.detail = files
                    .iter()
                    .map(|file| file.label.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                request.diff = diff;
                request.files = files;
                request.scratch = Some(scratch);
            }
            Err(error) => {
                request.detail = format!(
                    "nanocode could not preview this change ({error}); input:\n{}",
                    serde_json::to_string_pretty(&input_json(&input)).unwrap_or_default()
                );
            }
        }
        if self.permissions.ask(request).await {
            self.inner.execute(input, context).await
        } else {
            denied("make this file change")
        }
    }

    async fn preview(
        &self,
        input: &ToolInput,
        context: ToolContext<'_>,
    ) -> Result<(String, Vec<FilePair>, PathBuf), String> {
        let (writes, mut written) = mpsc::unbounded_channel();
        let files: Arc<dyn FileHost> = Arc::new(DryRun { writes });
        let workspace = self.permissions.workspace.clone();
        let tools = match self.definition.name() {
            "apply_patch" => nanocode::codex_tools::host_file_tools(workspace, files),
            _ => nanocode::claude::host_file_tools(workspace, files),
        }
        .map_err(|error| error.to_string())?;
        let tool = tools
            .registered_tools()
            .find(|tool| tool.definition().name() == self.definition.name())
            .cloned()
            .ok_or("no preview tool")?;
        let output = tool
            .execute(copy_input(input), context)
            .await
            .map_err(|error| error.to_string())?;
        if !output.success {
            return Err(match output.output {
                contract::ToolOutputBody::Text(text) => text,
                contract::ToolOutputBody::Content(_) => "the preview failed".to_owned(),
            });
        }
        drop(tool);
        drop(tools);
        let scratch = self
            .permissions
            .scratch
            .join(uuid::Uuid::now_v7().simple().to_string());
        std::fs::create_dir_all(&scratch).map_err(|error| error.to_string())?;
        let mut pairs = Vec::new();
        let mut diffs = Vec::new();
        let mut index = 0;
        while let Ok((path, contents)) = written.try_recv() {
            index += 1;
            let label = self.permissions.label(&path);
            let new = match contents {
                Some(contents) => {
                    let name = path.file_name().map_or_else(
                        || format!("file-{index}"),
                        |name| format!("{index}-{}", name.to_string_lossy()),
                    );
                    let new = scratch.join(name);
                    std::fs::write(&new, contents).map_err(|error| error.to_string())?;
                    new
                }
                None => PathBuf::from("/dev/null"),
            };
            let old = if path.exists() {
                path.clone()
            } else {
                PathBuf::from("/dev/null")
            };
            diffs.push(sandbox::unified_diff(&old, &new, &label));
            pairs.push(FilePair { label, old, new });
        }
        Ok((diffs.join("\n"), pairs, scratch))
    }

    async fn command(
        &self,
        input: ToolInput,
        context: ToolContext<'_>,
        session_id: &str,
        mode: Mode,
    ) -> ToolResult {
        let mut arguments = input_json(&input);
        let field = if self.definition.name() == "Bash" {
            "command"
        } else {
            "cmd"
        };
        let Some(command) = arguments
            .get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return self.inner.execute(input, context).await;
        };
        let background = arguments
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let sandbox = self
            .permissions
            .sandbox
            .as_ref()
            .filter(|_| mode != Mode::Manual && !background);
        let Some(sandbox) = sandbox else {
            let title = if background {
                "Run a background command"
            } else {
                "Run a command"
            };
            let mut request = self.permissions.request(session_id, title, command);
            request.command = true;
            return if self.permissions.ask(request).await {
                self.inner.execute(input, context).await
            } else {
                denied("run this command")
            };
        };
        let (sandbox, wrapped) = match sandbox.sandbox(&command) {
            Ok(created) => created,
            Err(error) => {
                return Ok(ToolOutput::error(format!(
                    "nanocode could not create a sandbox for this command: {error}"
                )));
            }
        };
        arguments[field] = Value::String(wrapped);
        let Ok(raw) = serde_json::value::to_raw_value(&arguments) else {
            sandbox.discard();
            return self.inner.execute(input, context).await;
        };
        let result = self.inner.execute(ToolInput::Function(raw), context).await;
        if sandbox.finished() {
            return match self.permissions.review(session_id, &command, sandbox).await {
                Ok(note) => annotate(result, note),
                Err(denial) => Ok(ToolOutput::error(match result {
                    Ok(output) => format!(
                        "{denial}\n\nOutput of the command in its sandbox:\n{}",
                        output_text(&output)
                    ),
                    Err(error) => format!("{denial}\n\nThe command failed in its sandbox: {error}"),
                })),
            };
        }
        let permissions = Arc::clone(&self.permissions);
        let session_id = session_id.to_owned();
        tokio::spawn(async move {
            while !sandbox.finished() {
                tokio::time::sleep(WATCH_INTERVAL).await;
            }
            drop(permissions.review(&session_id, &command, sandbox).await);
        });
        annotate(
            result,
            Some("This command runs in a sandbox: the file changes it makes in the workspace are applied only after it exits and the user approves them.".to_owned()),
        )
    }
}

impl Tool for GatedTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.inner.supports_parallel_tool_calls()
    }

    fn running_tasks(&self) -> Vec<contract::RunningTask> {
        self.inner.running_tasks()
    }

    fn end_turn<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        session_id: &'life1 str,
        turn_id: &'life2 str,
        hook_event_name: &'life3 str,
    ) -> Pin<Box<dyn Future<Output = Result<(), contract::ToolError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.end_turn(session_id, turn_id, hook_event_name)
    }

    fn execute<'life0, 'life1, 'async_trait>(
        &'life0 self,
        input: ToolInput,
        context: ToolContext<'life1>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(self.run(input, context))
    }
}

struct DryRun {
    writes: mpsc::UnboundedSender<(PathBuf, Option<String>)>,
}

impl FileHost for DryRun {
    fn read<'a>(&'a self, path: &'a Path) -> ProcessFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            match tokio::fs::read_to_string(path).await {
                Ok(text) => Ok(Some(text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        })
    }

    fn write<'a>(
        &'a self,
        path: &'a Path,
        contents: &'a str,
    ) -> ProcessFuture<'a, Result<(), String>> {
        drop(
            self.writes
                .send((path.to_owned(), Some(contents.to_owned()))),
        );
        Box::pin(async { Ok(()) })
    }

    fn remove<'a>(&'a self, path: &'a Path) -> ProcessFuture<'a, Result<(), String>> {
        drop(self.writes.send((path.to_owned(), None)));
        Box::pin(async { Ok(()) })
    }
}

fn output_text(output: &ToolOutput) -> String {
    match &output.output {
        contract::ToolOutputBody::Text(text) => text.clone(),
        contract::ToolOutputBody::Content(content) => content
            .iter()
            .filter_map(|item| match item {
                contract::ToolOutputContent::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}
