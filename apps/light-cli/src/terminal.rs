//! Running a [`Shell`] against a person at a terminal, against piped input, or against `-c` lines.
//!
//! The terminal uses a line editor (history, cursor keys). It blocks while it waits for a key, so
//! it lives on its own thread and hands finished lines to the async session; while it is
//! waiting, chat replies are printed above the prompt without disturbing what is being typed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rustyline::error::ReadlineError;
use rustyline::{Config, DefaultEditor, ExternalPrinter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::output::Output;
use crate::shell::{Flow, Shell};

/// Prints above the line being edited while the editor is waiting for input, and plainly when it
/// is not (while a command is running).
pub struct TerminalOutput {
    editing: Arc<AtomicBool>,
    printer: Mutex<Box<dyn ExternalPrinter + Send>>,
}

impl Output for TerminalOutput {
    fn line(&self, text: &str) {
        if self.editing.load(Ordering::SeqCst)
            && self
                .printer
                .lock()
                .is_ok_and(|mut printer| printer.print(format!("{text}\n")).is_ok())
        {
            return;
        }
        println!("{text}");
    }
}

enum Input {
    Line(String),
    Interrupted,
    Eof,
    Failed(String),
}

/// A line editor ready to run on its own thread.
pub struct Terminal {
    editor: DefaultEditor,
    editing: Arc<AtomicBool>,
    history: PathBuf,
}

impl Terminal {
    /// Open the line editor. Errors when there is no usable terminal.
    pub fn open(history: &Path) -> Result<(Terminal, Arc<TerminalOutput>), String> {
        let config = Config::builder()
            .auto_add_history(false)
            // A line that starts with a space stays out of the history.
            .history_ignore_space(true)
            .max_history_size(1000)
            .map_err(|e| e.to_string())?
            .build();
        let mut editor = DefaultEditor::with_config(config).map_err(|e| e.to_string())?;
        let _ = editor.load_history(history);
        let editing = Arc::new(AtomicBool::new(false));
        let printer = editor
            .create_external_printer()
            .map_err(|e| e.to_string())?;
        let output = Arc::new(TerminalOutput {
            editing: Arc::clone(&editing),
            printer: Mutex::new(Box::new(printer)),
        });
        Ok((
            Terminal {
                editor,
                editing,
                history: history.to_path_buf(),
            },
            output,
        ))
    }

    /// Read and run lines until `/exit`, Ctrl-D, or Ctrl-C pressed twice at an empty prompt.
    pub async fn run(self, shell: &mut Shell) {
        let Terminal {
            mut editor,
            editing,
            history,
        } = self;
        let (lines, mut input) = mpsc::unbounded_channel();
        let (prompts, wanted) = std::sync::mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            while let Ok(prompt) = wanted.recv() {
                editing.store(true, Ordering::SeqCst);
                let read = editor.readline(&prompt);
                editing.store(false, Ordering::SeqCst);
                let (input, last) = match read {
                    Ok(line) => {
                        let _ = editor.add_history_entry(line.as_str());
                        (Input::Line(line), false)
                    }
                    Err(ReadlineError::Interrupted) => (Input::Interrupted, false),
                    Err(ReadlineError::Eof) => (Input::Eof, true),
                    Err(other) => (Input::Failed(other.to_string()), true),
                };
                if lines.send(input).is_err() || last {
                    break;
                }
            }
            save_history(&mut editor, &history);
        });

        let mut interrupted = false;
        loop {
            if prompts.send(shell.prompt()).is_err() {
                break;
            }
            match input.recv().await {
                Some(Input::Line(line)) => {
                    interrupted = false;
                    if shell.handle(&line).await == Flow::Exit {
                        break;
                    }
                }
                Some(Input::Interrupted) => {
                    if interrupted {
                        break;
                    }
                    interrupted = true;
                    println!("(press Ctrl-C again, or type /exit, to leave)");
                }
                Some(Input::Failed(error)) => {
                    println!("error: the terminal failed: {error}");
                    break;
                }
                Some(Input::Eof) | None => break,
            }
        }
        shell.shutdown().await;
        drop(prompts);
        let _ = tokio::task::spawn_blocking(move || reader.join()).await;
    }
}

fn save_history(editor: &mut DefaultEditor, path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // What was said to an agent may be private: keep the history readable by its owner only.
    if editor.save_history(path).is_ok() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Run lines one after another (a script, a pipe, or `-c`) until one says `/exit` or they run out.
pub async fn run_lines(shell: &mut Shell, lines: impl IntoIterator<Item = String>) {
    for line in lines {
        if shell.handle(&line).await == Flow::Exit {
            break;
        }
    }
    shell.shutdown().await;
}

/// Read lines from standard input until it closes, then run them as [`run_lines`] does, one at a
/// time so a slow command is finished before the next line is read.
pub async fn run_stdin(shell: &mut Shell) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if shell.handle(&line).await == Flow::Exit {
            break;
        }
    }
    shell.shutdown().await;
}
