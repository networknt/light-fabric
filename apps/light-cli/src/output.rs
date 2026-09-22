//! Where the session's output goes.
//!
//! Chat output arrives while the person may be typing, so it cannot simply `println!`: in the
//! interactive terminal it goes through the line editor's external printer, which repaints the
//! prompt and what was typed. In a pipe, or a test, it goes to stdout or a collector.

use std::sync::Mutex;

pub trait Output: Send + Sync {
    /// One message, printed on its own line(s). The text must already be safe to print.
    fn line(&self, text: &str);
}

/// Plain standard output.
pub struct Stdout;

impl Output for Stdout {
    fn line(&self, text: &str) {
        println!("{text}");
    }
}

/// Collects lines, for tests and for callers that want to inspect what was said.
#[derive(Default)]
pub struct Collector {
    lines: Mutex<Vec<String>>,
}

impl Collector {
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    pub fn text(&self) -> String {
        self.lines().join("\n")
    }
}

impl Output for Collector {
    fn line(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
}
