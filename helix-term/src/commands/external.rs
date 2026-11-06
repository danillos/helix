use crate::{compositor::Compositor, job, ui::EditorView};
use anyhow::{bail, Context, Result};
use helix_view::Editor;

fn program_command(arguments: Vec<String>) -> Result<tokio::process::Command> {
    let (program, arguments) = arguments.split_first().context("A program is required")?;
    let mut command = tokio::process::Command::new(
        helix_stdx::path::expand_tilde(std::path::Path::new(program)).as_ref(),
    );
    command
        .args(arguments)
        .current_dir(helix_stdx::env::current_working_dir());
    command.kill_on_drop(true);
    Ok(command)
}

pub fn run_program(
    cx: &mut crate::compositor::Context,
    args: helix_core::command_line::Args,
    event: crate::ui::PromptEvent,
) -> Result<()> {
    if event != crate::ui::PromptEvent::Validate {
        return Ok(());
    }
    let mut command = program_command(args.iter().map(|arg| arg.to_string()).collect())?;
    command.stdin(std::process::Stdio::null());
    cx.jobs.callback(async move {
        let output = command.output().await?;
        if !output.status.success() {
            bail!(
                "Program failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(job::Callback::Editor(Box::new(|editor| {
            editor.set_status("Program completed")
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
                .is_some_and(|view| view.terminal_focused() && !view.input_pending());
        unobstructed
            && editor.mode == self.mode
            && editor.count.is_none()
            && editor.tree.focus == self.view
            && view.doc == self.document
            && doc.version() == self.version
            && doc.selection(self.view) == &self.selection
    }
}

pub fn pipe_program(
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
        bail!("Select one region to pipe to the program");
    }
    let doc = cx.editor.document(context.document).unwrap();
    let input = context
        .selection
        .primary()
        .slice(doc.text().slice(..))
        .to_string();
    let mut command = program_command(args.iter().map(|arg| arg.to_string()).collect())?;
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
                "Program failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let output = String::from_utf8(output.stdout)?;
        Ok(job::Callback::EditorCompositor(Box::new(
            move |editor, compositor| {
                if !context.matches(editor, compositor) {
                    editor.set_error("Program result discarded because its editor context changed");
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
