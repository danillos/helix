use crate::{compositor::Compositor, job, ui::EditorView};
use anyhow::{bail, Context, Result};
use helix_view::{tree, Editor};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Notify,
    task::JoinHandle,
    time::timeout,
};

const PROTOCOL: u32 = 1;
const REQUEST_LIFETIME: Duration = Duration::from_millis(150);
const IO_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Moved,
    Edge,
    Blocked,
    Stale,
    NotApplied,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Response {
    outcome: Outcome,
    before: Option<String>,
    after: Option<String>,
}

impl Response {
    fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            before: None,
            after: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn tree(self) -> tree::Direction {
        match self {
            Self::Left => tree::Direction::Left,
            Self::Right => tree::Direction::Right,
            Self::Up => tree::Direction::Up,
            Self::Down => tree::Direction::Down,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Hello {
    protocol: u32,
    instance: String,
    session: String,
    block: String,
    #[serde(default)]
    buffer_navigation: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Request {
    Navigate {
        instance: String,
        id: String,
        direction: Direction,
        deadline_ms: u64,
    },
    Buffer {
        instance: String,
        id: String,
        index: usize,
        deadline_ms: u64,
    },
    Cancel {
        instance: String,
        id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Navigate(Direction),
    Buffer(usize),
}

struct Pending {
    action: Option<Action>,
    expires: Instant,
    result: Mutex<Option<Response>>,
    ready: Notify,
}

impl Pending {
    fn new(action: Option<Action>, lifetime: Duration) -> Self {
        Self {
            action,
            expires: Instant::now() + lifetime,
            result: Mutex::new(None),
            ready: Notify::new(),
        }
    }

    fn cancel(&self) -> Response {
        let mut result = self.result.lock().unwrap();
        let response = result
            .get_or_insert_with(|| Response::new(Outcome::NotApplied))
            .clone();
        self.ready.notify_one();
        response
    }

    fn apply(&self, navigate: impl FnOnce() -> Response) {
        // Cancellation and the editor change share this lock. A confirmed
        // cancellation prevents a queued callback from moving a view later.
        let mut result = self.result.lock().unwrap();
        if result.is_none() {
            *result = Some(if Instant::now() >= self.expires {
                Response::new(Outcome::NotApplied)
            } else {
                navigate()
            });
        }
        self.ready.notify_one();
    }

    async fn response(&self) -> Response {
        loop {
            let notified = self.ready.notified();
            if let Some(response) = self.result.lock().unwrap().clone() {
                return response;
            }
            if tokio::time::timeout_at(self.expires.into(), notified)
                .await
                .is_err()
            {
                return self.cancel();
            }
        }
    }
}

type Requests = Arc<Mutex<HashMap<String, Arc<Pending>>>>;

pub struct Bridge {
    path: PathBuf,
    task: JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_file(&self.path);
    }
}

fn socket_path(pid: u32) -> PathBuf {
    helix_loader::cache_dir()
        .join("rex")
        .join(format!("{pid}.sock"))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn remove_stale_endpoints(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sock") {
            continue;
        }
        let Some(pid) = path
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        if unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub fn start() -> Result<Option<Bridge>> {
    let (Ok(session), Ok(block)) = (std::env::var("REX_SESSION"), std::env::var("REX_BLOCK"))
    else {
        return Ok(None);
    };
    let path = socket_path(std::process::id());
    let directory = path.parent().unwrap();
    fs::create_dir_all(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("Rex integration directory must belong to the current user");
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    remove_stale_endpoints(directory)?;
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let hello = Hello {
        protocol: PROTOCOL,
        instance: format!("{}-{}", std::process::id(), unix_ms()),
        session,
        block,
        buffer_navigation: true,
    };
    let requests: Requests = Arc::default();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let hello = hello.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                if let Err(error) = serve(stream, hello, requests).await {
                    log::debug!("Rex navigation request: {error:#}");
                }
            });
        }
    });
    Ok(Some(Bridge { path, task }))
}

async fn write_json<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    timeout(IO_TIMEOUT, stream.write_all(&bytes)).await??;
    Ok(())
}

async fn read_json<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(stream);
    timeout(IO_TIMEOUT, async {
        loop {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            let length = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(chunk.len(), |n| n + 1);
            if bytes.len() + length > 4096 {
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
            }
            let done = chunk[length - 1] == b'\n';
            bytes.extend_from_slice(&chunk[..length]);
            reader.consume(length);
            if done {
                return Ok(());
            }
        }
    })
    .await??;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn serve(mut stream: UnixStream, hello: Hello, requests: Requests) -> Result<()> {
    write_json(&mut stream, &hello).await?;
    let request: Request = read_json(&mut stream).await?;
    let (instance, id, action, deadline_ms) = match request {
        Request::Cancel { instance, id } => {
            if instance != hello.instance {
                bail!("Helix process instance changed");
            }
            let pending = {
                let mut requests = requests.lock().unwrap();
                requests
                    .entry(id)
                    .or_insert_with(|| Arc::new(Pending::new(None, Duration::ZERO)))
                    .clone()
            };
            return write_json(&mut stream, &pending.cancel()).await;
        }
        Request::Navigate {
            instance,
            id,
            direction,
            deadline_ms,
        } => (instance, id, Action::Navigate(direction), deadline_ms),
        Request::Buffer {
            instance,
            id,
            index,
            deadline_ms,
        } => (instance, id, Action::Buffer(index), deadline_ms),
    };
    if instance != hello.instance || id.len() > 128 {
        bail!("Invalid editor request");
    }
    let lifetime =
        Duration::from_millis(deadline_ms.saturating_sub(unix_ms())).min(REQUEST_LIFETIME);
    let (pending, created) = {
        let mut requests = requests.lock().unwrap();
        requests.retain(|_, p| p.expires.elapsed().as_secs() < 60);
        match requests.get(&id) {
            Some(p) => {
                if p.action.is_some_and(|previous| previous != action) {
                    bail!("Request ID reused with another action");
                }
                (p.clone(), false)
            }
            None => {
                let pending = Arc::new(Pending::new(Some(action), lifetime));
                requests.insert(id, pending.clone());
                (pending, true)
            }
        }
    };
    if created {
        let callback = pending.clone();
        job::dispatch(move |editor, compositor| {
            callback.apply(|| apply_action(editor, compositor, action));
        })
        .await;
    }
    write_json(&mut stream, &pending.response().await).await
}

fn apply_action(editor: &mut Editor, compositor: &mut Compositor, action: Action) -> Response {
    let overlay = compositor.layer_count() != 1;
    let Some(view) = compositor.find::<EditorView>() else {
        return Response::new(Outcome::Blocked);
    };
    if !view.rex_terminal_focused() {
        return Response::new(Outcome::Stale);
    }
    if overlay || view.rex_input_pending() || editor.count.is_some() {
        return Response::new(Outcome::Blocked);
    }
    if let Action::Buffer(index) = action {
        let document = index
            .checked_sub(1)
            .and_then(|index| editor.documents().nth(index))
            .map(|doc| doc.id());
        let Some(document) = document else {
            editor.set_error(format!("No Helix buffer at position {index}"));
            return Response::new(Outcome::Blocked);
        };
        let before = editor.tree.get(editor.tree.focus).doc;
        editor.switch(document, helix_view::editor::Action::Replace);
        return Response {
            outcome: Outcome::Moved,
            before: Some(format!("{before:?}")),
            after: Some(format!("{document:?}")),
        };
    }
    let Action::Navigate(direction) = action else {
        unreachable!()
    };
    let before = editor.tree.focus;
    if let Some(next) = editor
        .tree
        .find_split_in_direction(before, direction.tree())
    {
        editor.focus_preserving_mode(next);
    }
    let after = editor.tree.focus;
    Response {
        outcome: if before == after {
            Outcome::Edge
        } else {
            Outcome::Moved
        },
        before: Some(format!("{before:?}")),
        after: Some(format!("{after:?}")),
    }
}

async fn connect(pid: u32, session: &str, block: &str) -> Result<(UnixStream, Hello)> {
    let mut stream = timeout(IO_TIMEOUT, UnixStream::connect(socket_path(pid))).await??;
    let hello: Hello = read_json(&mut stream).await?;
    if hello.protocol != PROTOCOL || hello.session != session || hello.block != block {
        bail!("Helix integration capability does not match the originating pane");
    }
    Ok((stream, hello))
}

async fn request(pid: u32, session: &str, block: &str, action: Action) -> Response {
    let Ok((mut stream, hello)) = connect(pid, session, block).await else {
        return Response::new(Outcome::Unavailable);
    };
    let id = format!("{}-{}", std::process::id(), unix_ms());
    if matches!(action, Action::Buffer(_)) && !hello.buffer_navigation {
        return Response::new(Outcome::Unavailable);
    }
    let deadline_ms = unix_ms() + REQUEST_LIFETIME.as_millis() as u64;
    let request = match action {
        Action::Navigate(direction) => Request::Navigate {
            instance: hello.instance.clone(),
            id: id.clone(),
            direction,
            deadline_ms,
        },
        Action::Buffer(index) => Request::Buffer {
            instance: hello.instance.clone(),
            id: id.clone(),
            index,
            deadline_ms,
        },
    };
    if write_json(&mut stream, &request).await.is_ok() {
        if let Ok(response) = read_json(&mut stream).await {
            return response;
        }
    }
    // A lost reply can follow a completed move. Resolve this same ID;
    // a timeout alone never authorizes a second, outer-pane move.
    if let Ok((mut stream, current)) = connect(pid, session, block).await {
        if current.instance == hello.instance {
            let cancel = Request::Cancel {
                instance: hello.instance,
                id,
            };
            if write_json(&mut stream, &cancel).await.is_ok() {
                if let Ok(response) = read_json(&mut stream).await {
                    return response;
                }
            }
        }
    }
    Response::new(Outcome::Unknown)
}

pub async fn cli(args: &[String]) -> Result<i32> {
    if matches!(args, [operation] if operation == "probe") {
        println!("{}", serde_json::json!({ "protocol": PROTOCOL }));
        return Ok(0);
    }
    if matches!(args, [operation] if operation == "probe-buffer") {
        println!("{}", serde_json::json!({ "buffer_navigation": true }));
        return Ok(0);
    }
    if args.len() != 4 {
        bail!("--rex-navigate requires PID DIRECTION SESSION BLOCK");
    }
    let pid = args[0].parse().context("Invalid Helix process ID")?;
    let action = if let Some(index) = args[1].strip_prefix("buffer:") {
        Action::Buffer(index.parse().context("Invalid buffer position")?)
    } else {
        Action::Navigate(
            serde_json::from_value(serde_json::Value::String(args[1].clone()))
                .context("Invalid navigation direction")?,
        )
    };
    let response = request(pid, &args[2], &args[3], action).await;
    println!("{}", serde_json::to_string(&response)?);
    Ok(0)
}

fn helper_command(arguments: Vec<String>) -> Result<tokio::process::Command> {
    let home = std::env::var_os("HOME").context("HOME is required for the Rex helpers")?;
    let mut command = tokio::process::Command::new(
        std::env::var_os("HELIX_REX_HELPERS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".dotfiles/support/pipe.rb")),
    );
    command
        .args(arguments)
        .current_dir(helix_stdx::env::current_working_dir());
    command.kill_on_drop(true);
    Ok(command)
}

pub fn launch_tool(
    cx: &mut crate::compositor::Context,
    args: helix_core::command_line::Args,
    event: crate::ui::PromptEvent,
) -> Result<()> {
    if event != crate::ui::PromptEvent::Validate {
        return Ok(());
    }
    let mut command = helper_command(args.iter().map(|arg| arg.to_string()).collect())?;
    command.stdin(std::process::Stdio::null());
    cx.jobs.callback(async move {
        let output = command.output().await?;
        if !output.status.success() {
            bail!(
                "Rex tool: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(job::Callback::Editor(Box::new(|editor| {
            editor.set_status("Rex tool opened")
        })))
    });
    Ok(())
}

#[derive(Clone)]
struct EditContext {
    view: helix_view::ViewId,
    document: helix_view::DocumentId,
    version: i32,
    mode: helix_view::document::Mode,
    selection: helix_core::Selection,
}

impl EditContext {
    fn capture(editor: &Editor) -> Self {
        let view = editor.tree.get(editor.tree.focus);
        let doc = editor.document(view.doc).unwrap();
        Self {
            view: view.id,
            document: doc.id(),
            version: doc.version(),
            mode: editor.mode,
            selection: doc.selection(view.id).clone(),
        }
    }

    fn matches(&self, editor: &Editor, compositor: &mut Compositor) -> bool {
        let Some(view) = editor.tree.try_get(self.view) else {
            return false;
        };
        let Some(doc) = editor.document(self.document) else {
            return false;
        };
        let unobstructed = compositor.layer_count() == 1
            && compositor
                .find::<EditorView>()
                .is_some_and(|view| view.rex_terminal_focused() && !view.rex_input_pending());
        unobstructed
            && editor.mode == self.mode
            && editor.count.is_none()
            && editor.tree.focus == self.view
            && view.doc == self.document
            && doc.version() == self.version
            && doc.selection(self.view) == &self.selection
    }
}

pub fn transform_selection(
    cx: &mut crate::compositor::Context,
    args: helix_core::command_line::Args,
    event: crate::ui::PromptEvent,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if event != crate::ui::PromptEvent::Validate {
        return Ok(());
    }
    let copy_to_clipboard = args.has_flag("clipboard");
    let context = EditContext::capture(cx.editor);
    if context.selection.len() != 1 {
        bail!("Select one function or output region for this helper");
    }
    let doc = cx.editor.document(context.document).unwrap();
    let input = context
        .selection
        .primary()
        .slice(doc.text().slice(..))
        .to_string();
    let mut command = helper_command(args.iter().map(|arg| arg.to_string()).collect())?;
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cx.jobs.callback(async move {
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let writer = tokio::spawn(async move { stdin.write_all(input.as_bytes()).await });
        let output = child.wait_with_output().await?;
        writer.await??;
        if !output.status.success() {
            bail!(
                "Helix helper: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let output = String::from_utf8(output.stdout)?;
        Ok(job::Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                if !context.matches(editor, compositor) {
                    editor.set_error("Helper result discarded because its editor context changed");
                    return;
                }
                if copy_to_clipboard {
                    if let Err(error) = editor.registers.write('+', vec![output.clone()]) {
                        editor.set_error(error.to_string());
                        return;
                    }
                }
                let scrolloff = editor.config().scrolloff;
                let (view, doc) = helix_view::current!(editor);
                let range = context.selection.primary();
                let selection = helix_core::Selection::single(
                    range.from(),
                    range.from() + output.chars().count(),
                );
                let transaction = helix_core::Transaction::change(
                    doc.text(),
                    [(range.from(), range.to(), Some(output.into()))].into_iter(),
                )
                .with_selection(selection);
                doc.apply(&transaction, view.id);
                doc.append_changes_to_history(view);
                view.ensure_cursor_in_view(doc, scrolloff);
            },
        )))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn crash_cleanup_preserves_live_endpoints_and_other_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let dead = directory.path().join(format!("{dead_pid}.sock"));
        let live = directory
            .path()
            .join(format!("{}.sock", std::process::id()));
        let _dead_listener = std::os::unix::net::UnixListener::bind(&dead).unwrap();
        let _live_listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        let other = directory.path().join("notes.txt");
        fs::write(&other, "keep").unwrap();
        remove_stale_endpoints(directory.path()).unwrap();
        assert!(!dead.exists());
        assert!(live.exists());
        assert_eq!(fs::read_to_string(other).unwrap(), "keep");
    }

    #[tokio::test]
    async fn cancellation_fences_a_delayed_editor_callback() {
        let pending = Pending::new(Some(Action::Navigate(Direction::Right)), REQUEST_LIFETIME);
        assert_eq!(pending.cancel().outcome, Outcome::NotApplied);
        pending.apply(|| panic!("A cancelled request must not reach the editor"));
        assert_eq!(pending.response().await.outcome, Outcome::NotApplied);
    }

    #[tokio::test]
    async fn expired_request_cannot_change_editor_focus() {
        let pending = Pending::new(Some(Action::Navigate(Direction::Right)), Duration::ZERO);
        pending.apply(|| panic!("An expired request must not reach the editor"));
        assert_eq!(pending.response().await.outcome, Outcome::NotApplied);
    }

    #[tokio::test]
    async fn duplicate_or_cancel_after_a_move_returns_the_committed_result() {
        let calls = AtomicUsize::new(0);
        let pending = Pending::new(Some(Action::Navigate(Direction::Right)), REQUEST_LIFETIME);
        pending.apply(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            Response::new(Outcome::Moved)
        });
        pending.apply(|| panic!("A repeated ID must not move twice"));
        assert_eq!(pending.cancel().outcome, Outcome::Moved);
        assert_eq!(pending.response().await.outcome, Outcome::Moved);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unresponsive_editor_returns_a_confirmed_cancellation() {
        let pending = Pending::new(
            Some(Action::Navigate(Direction::Left)),
            Duration::from_millis(1),
        );
        assert_eq!(pending.response().await.outcome, Outcome::NotApplied);
        pending.apply(|| panic!("A timed out request must stay cancelled"));
    }
}
