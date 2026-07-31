//! Local keyterm storage and terminal management commands.

use anyhow::{bail, Context, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::Print,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};

const MAX_TERMS: usize = 1_000;
const MAX_TERM_CHARS: usize = 50;
const MAX_TERM_WORDS: usize = 5;

/// Load keyterms from `~/.config/voice-daemon/keyterms.txt`.
pub fn load() -> Result<Vec<String>> {
    load_from_path(&path())
}

pub(crate) fn add(term: &str) -> Result<()> {
    add_to_path(&path(), term)
}

pub(crate) fn remove(term: &str) -> Result<()> {
    remove_from_path(&path(), term)
}

fn add_to_path(path: &Path, term: &str) -> Result<()> {
    let mut terms = load_from_path(path)?;
    let term = validate_term(term)?;
    if terms.iter().any(|existing| existing == &term) {
        bail!("keyterm already exists");
    }
    if terms.len() >= MAX_TERMS {
        bail!("keyterm limit of {MAX_TERMS} reached");
    }
    terms.push(term);
    write_to_path(path, &terms)
}

fn remove_from_path(path: &Path, term: &str) -> Result<()> {
    let mut terms = load_from_path(path)?;
    let term = validate_term(term)?;
    let Some(index) = terms.iter().position(|existing| existing == &term) else {
        bail!("keyterm not found");
    };
    terms.remove(index);
    write_to_path(path, &terms)
}

pub(crate) fn interactive() -> Result<()> {
    let mut terms = load()?;
    let mut selected = 0;
    let mut terminal_guard = TerminalGuard::new()?;
    let mut stdout = terminal_guard.stdout();

    loop {
        render(&mut stdout, &terms, selected)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = move_selection(selected, terms.len(), -1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                selected = move_selection(selected, terms.len(), 1)
            }
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('D') if !terms.is_empty() => {
                let term = terms[selected].clone();
                if confirm_delete(&mut stdout, &term)? {
                    terms.remove(selected);
                    write_to_path(&path(), &terms)?;
                    selected = selected.min(terms.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }
}

fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("voice-daemon")
        .join("keyterms.txt")
}

fn load_from_path(path: &Path) -> Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => parse(&contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn parse(contents: &str) -> Result<Vec<String>> {
    let mut terms = Vec::new();
    let mut seen = HashSet::new();
    for line in contents.lines() {
        let term = line.trim();
        if term.is_empty() {
            continue;
        }
        let term = validate_term(term)?;
        if seen.insert(term.clone()) {
            if terms.len() == MAX_TERMS {
                bail!("keyterm limit of {MAX_TERMS} exceeded");
            }
            terms.push(term);
        }
    }
    Ok(terms)
}

fn validate_term(term: &str) -> Result<String> {
    let term = term.trim();
    if term.is_empty() {
        bail!("keyterm must not be empty");
    }
    if term.contains(['\n', '\r']) {
        bail!("keyterm must be a single line");
    }
    if term.chars().count() > MAX_TERM_CHARS {
        bail!("keyterm must be at most {MAX_TERM_CHARS} characters");
    }
    if term.split_whitespace().count() > MAX_TERM_WORDS {
        bail!("keyterm must contain at most {MAX_TERM_WORDS} words");
    }
    if term
        .chars()
        .any(|character| matches!(character, '<' | '>' | '{' | '}' | '[' | ']' | '\\'))
    {
        bail!("keyterm contains characters unsupported by ElevenLabs");
    }
    Ok(term.to_owned())
}

fn write_to_path(path: &Path, terms: &[String]) -> Result<()> {
    let parent = path
        .parent()
        .context("keyterms path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let mut temp_path = None;
    let mut file = None;
    for attempt in 0..100 {
        let candidate = parent.join(format!(".keyterms.{}.{}.tmp", process::id(), attempt));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(created) => {
                temp_path = Some(candidate);
                file = Some(created);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("failed to create temporary keyterms file"),
        }
    }
    let temp_path = temp_path.context("could not create a temporary keyterms file")?;
    let result = (|| -> Result<()> {
        let mut file = file.context("temporary keyterms file was not opened")?;
        for term in terms {
            writeln!(file, "{term}").context("failed to write keyterms")?;
        }
        file.sync_all().context("failed to sync keyterms")?;
        fs::rename(&temp_path, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

struct TerminalGuard {
    stdout: io::Stdout,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error).context("failed to enter interactive terminal");
        }
        Ok(Self { stdout })
    }

    fn stdout(&mut self) -> &mut io::Stdout {
        &mut self.stdout
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn render(stdout: &mut impl Write, terms: &[String], selected: usize) -> Result<()> {
    queue!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0),
        Print(format!("Keyterms ({} of {MAX_TERMS})", terms.len())),
        cursor::MoveTo(0, 1),
        Print("↑/↓ or j/k: move   D: delete   q/Esc: exit")
    )?;
    if terms.is_empty() {
        queue!(
            stdout,
            cursor::MoveTo(0, 3),
            Print("(no keyterms; add one with: daapstt keyterms add <term>)")
        )?;
    } else {
        for (index, term) in terms.iter().enumerate() {
            let marker = if index == selected { ">" } else { " " };
            queue!(
                stdout,
                cursor::MoveTo(0, index as u16 + 2),
                Print(format!("{marker} {}", display_term(term)))
            )?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn confirm_delete(stdout: &mut impl Write, term: &str) -> Result<bool> {
    execute!(
        stdout,
        cursor::MoveToNextLine(2),
        Print(format!("Delete \"{}\"? y to confirm: ", display_term(term)))
    )?;
    stdout.flush()?;
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Press {
            return Ok(is_delete_confirmation(key.code));
        }
    }
}

fn is_delete_confirmation(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char('y') | KeyCode::Char('Y'))
}

fn display_term(term: &str) -> String {
    term.chars().flat_map(char::escape_default).collect()
}

fn move_selection(selected: usize, len: usize, direction: i8) -> usize {
    if len == 0 {
        return 0;
    }
    match direction {
        -1 => selected.checked_sub(1).unwrap_or(len - 1),
        1 => (selected + 1) % len,
        _ => selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "daapstt-keyterms-test-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn parsing_trims_ignores_empty_and_deduplicates() {
        assert_eq!(
            parse("  Rust\n\nRust\n rust \n  Wayland  \n").unwrap(),
            ["Rust", "rust", "Wayland"]
        );
    }

    #[test]
    fn parsing_rejects_invalid_terms() {
        assert!(parse(&"é".repeat(MAX_TERM_CHARS + 1)).is_err());
        assert!(validate_term("   ").is_err());
        assert!(validate_term("first\nsecond").is_err());
    }

    #[test]
    fn parsing_enforces_the_unique_term_cap() {
        let input = (0..=MAX_TERMS)
            .map(|index| format!("term{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse(&input).is_err());
    }

    #[test]
    fn mutations_persist_and_missing_files_are_empty() {
        let path = test_path().join("keyterms.txt");
        assert!(load_from_path(&path).unwrap().is_empty());
        add_to_path(&path, "first").unwrap();
        add_to_path(&path, "second").unwrap();
        assert!(add_to_path(&path, "first").is_err());
        assert_eq!(load_from_path(&path).unwrap(), ["first", "second"]);
        remove_from_path(&path, "first").unwrap();
        assert!(remove_from_path(&path, "missing").is_err());
        assert_eq!(load_from_path(&path).unwrap(), ["second"]);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn adding_rejects_the_term_cap() {
        let path = test_path().join("keyterms.txt");
        let terms = (0..MAX_TERMS)
            .map(|index| format!("term{index}"))
            .collect::<Vec<_>>();
        write_to_path(&path, &terms).unwrap();
        assert!(add_to_path(&path, "one-too-many").is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn selection_wraps_and_empty_lists_stay_at_zero() {
        assert_eq!(move_selection(0, 0, 1), 0);
        assert_eq!(move_selection(0, 3, -1), 2);
        assert_eq!(move_selection(2, 3, 1), 0);
    }

    #[test]
    fn render_positions_each_row_at_the_left_edge() {
        let mut output = Vec::new();
        render(&mut output, &["pi".to_string(), "mateo".to_string()], 0).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[1;1HKeyterms (2 of 1000)"));
        assert!(output.contains("\x1b[2;1H↑/↓ or j/k: move"));
        assert!(output.contains("\x1b[3;1H> pi"));
        assert!(output.contains("\x1b[4;1H  mateo"));
    }

    #[test]
    fn only_y_confirms_deletion() {
        assert!(is_delete_confirmation(KeyCode::Char('y')));
        assert!(is_delete_confirmation(KeyCode::Char('Y')));
        assert!(!is_delete_confirmation(KeyCode::Char('n')));
        assert!(!is_delete_confirmation(KeyCode::Esc));
    }
}
