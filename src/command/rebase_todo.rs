//! Interactive rebase todo model, render, and parse (HF-21 / ADR-HF-19).
//!
//! The `rebase -i` / `--edit-todo` entry generates a Git-shaped
//! `git-rebase-todo`, runs the sequence editor, and parses the result. This
//! module owns the instruction model, the first-generate text (including
//! autosquash/`--exec` command lists), and the additive `rebase-aux.json`
//! instruction fields consumed by GC.

use std::fmt;

use serde::{Deserialize, Serialize};

/// `fixup` message flag: omitted, `-C` (keep this commit's message), or `-c`
/// (keep this commit's message and open the editor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixupFlag {
    #[serde(rename = "C")]
    KeepThis,
    #[serde(rename = "c")]
    Reword,
}

/// One interactive rebase instruction (ADR-HF-19 §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TodoInstruction {
    Pick {
        commit: String,
    },
    Reword {
        commit: String,
    },
    Edit {
        commit: String,
    },
    Squash {
        commit: String,
    },
    Fixup {
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flag: Option<FixupFlag>,
    },
    Exec {
        cmd: String,
    },
    Break,
    Drop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
}

impl TodoInstruction {
    /// Commit id carried by this instruction, if any (ER-HF-06 / M-TODO T12).
    pub(crate) fn commit_oid(&self) -> Option<&str> {
        match self {
            Self::Pick { commit }
            | Self::Reword { commit }
            | Self::Edit { commit }
            | Self::Squash { commit }
            | Self::Fixup { commit, .. } => Some(commit.as_str()),
            Self::Drop { commit } => commit.as_deref(),
            Self::Exec { .. } | Self::Break => None,
        }
    }
}

/// Parse failure for a todo buffer (M-TODO T11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoParseError {
    InvalidCommand {
        command: String,
        number: usize,
        line: String,
    },
    InvalidLine {
        number: usize,
        line: String,
    },
}

impl fmt::Display for TodoParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommand { command, .. } => {
                write!(f, "invalid command '{command}'")
            }
            Self::InvalidLine { number, line } => {
                write!(f, "invalid line {number}: {line}")
            }
        }
    }
}

impl std::error::Error for TodoParseError {}

/// One first-generate pick line: 7-char abbrev plus subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoRenderCommit {
    pub abbrev: String,
    pub subject: String,
}

/// Git 2.54 `append_todo_help` comment block with `l/t/m/u` omitted (DEFER-02,
/// ADR-HF-19 §5). Bytes match `strbuf_add_commented_lines` over the English
/// C strings in `rebase-interactive.c` (`#` + optional space + line).
pub const TODO_HELP_COMMENTS: &str = "\
#\n\
# Commands:\n\
# p, pick = use commit\n\
# r, reword = use commit, but edit the commit message\n\
# e, edit = use commit, but stop for amending\n\
# s, squash = use commit, but meld into previous commit\n\
# f, fixup [-C | -c] = like \"squash\" but keep only the previous\n\
#  commit's log message, unless -C is used, in which case\n\
#  keep only this commit's message; -c is same as -C but\n\
#  opens the editor\n\
# x, exec = run command (the rest of the line) using shell\n\
# b, break = stop here (continue rebase later with 'git rebase --continue')\n\
# d, drop = remove commit\n\
#\n\
# These lines can be re-ordered; they are executed from top to bottom.\n\
#\n\
# If you remove a line here THAT COMMIT WILL BE LOST.\n\
#\n\
# However, if you remove everything, the rebase will be aborted.\n\
#\n";

/// First-generate todo text (M-TODO T1 / ADR-HF-19 §5).
pub fn render_todo(commits: &[TodoRenderCommit], onto_abbrev: &str, head_abbrev: &str) -> String {
    let mut out = String::new();
    for commit in commits {
        out.push_str("pick ");
        out.push_str(&commit.abbrev);
        out.push_str(" # ");
        out.push_str(&commit.subject);
        out.push('\n');
    }
    out.push('\n');
    let count = commits.len();
    let noun = if count == 1 { "command" } else { "commands" };
    out.push_str("# Rebase ");
    out.push_str(onto_abbrev);
    out.push_str("..");
    out.push_str(head_abbrev);
    out.push_str(" onto ");
    out.push_str(onto_abbrev);
    out.push_str(" (");
    out.push_str(&count.to_string());
    out.push(' ');
    out.push_str(noun);
    out.push_str(")\n");
    out.push_str(TODO_HELP_COMMENTS);
    out
}

/// Render an already-built command list (autosquash / `--exec` injection).
pub fn render_todo_with_commands(
    commands: &[String],
    onto_abbrev: &str,
    head_abbrev: &str,
) -> String {
    let mut out = String::new();
    for command in commands {
        out.push_str(command);
        out.push('\n');
    }
    out.push('\n');
    let count = commands.len();
    let noun = if count == 1 { "command" } else { "commands" };
    out.push_str("# Rebase ");
    out.push_str(onto_abbrev);
    out.push_str("..");
    out.push_str(head_abbrev);
    out.push_str(" onto ");
    out.push_str(onto_abbrev);
    out.push_str(" (");
    out.push_str(&count.to_string());
    out.push(' ');
    out.push_str(noun);
    out.push_str(")\n");
    out.push_str(TODO_HELP_COMMENTS);
    out
}

/// Comment Git prints when `--edit-todo` rewrites an in-progress list (I7 / S5).
pub const ONGOING_REBASE_TODO_HINT: &str =
    "# You are editing the todo file of an ongoing interactive rebase.\n";

/// Remaining-instruction buffer for `--edit-todo` (ADR-HF-19 §5 / I7).
pub fn render_remaining_todo(
    instructions: &[TodoInstruction],
    onto_abbrev: &str,
    head_abbrev: &str,
) -> String {
    let mut out = String::new();
    for instruction in instructions {
        out.push_str(&format_instruction_line(instruction));
        out.push('\n');
    }
    out.push('\n');
    let count = instructions.len();
    let noun = if count == 1 { "command" } else { "commands" };
    out.push_str("# Rebase ");
    out.push_str(onto_abbrev);
    out.push_str("..");
    out.push_str(head_abbrev);
    out.push_str(" onto ");
    out.push_str(onto_abbrev);
    out.push_str(" (");
    out.push_str(&count.to_string());
    out.push(' ');
    out.push_str(noun);
    out.push_str(")\n");
    out.push_str(ONGOING_REBASE_TODO_HINT);
    out.push_str(TODO_HELP_COMMENTS);
    out
}

fn format_instruction_line(instruction: &TodoInstruction) -> String {
    match instruction {
        TodoInstruction::Pick { commit } => format!("pick {commit}"),
        TodoInstruction::Reword { commit } => format!("reword {commit}"),
        TodoInstruction::Edit { commit } => format!("edit {commit}"),
        TodoInstruction::Squash { commit } => format!("squash {commit}"),
        TodoInstruction::Fixup { commit, flag: None } => format!("fixup {commit}"),
        TodoInstruction::Fixup {
            commit,
            flag: Some(FixupFlag::KeepThis),
        } => format!("fixup -C {commit}"),
        TodoInstruction::Fixup {
            commit,
            flag: Some(FixupFlag::Reword),
        } => format!("fixup -c {commit}"),
        TodoInstruction::Exec { cmd } => format!("exec {cmd}"),
        TodoInstruction::Break => "break".to_string(),
        TodoInstruction::Drop {
            commit: Some(commit),
        } => format!("drop {commit}"),
        TodoInstruction::Drop { commit: None } => "drop".to_string(),
    }
}

/// Parse a todo buffer. `#` comments and blank lines are ignored (T10).
pub fn parse_todo(text: &str) -> Result<Vec<TodoInstruction>, TodoParseError> {
    let mut instructions = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let number = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (command, rest) = split_command(trimmed);
        match parse_instruction(command, rest) {
            Ok(None) => {
                return Err(TodoParseError::InvalidCommand {
                    command: command.to_string(),
                    number,
                    line: raw_line.to_string(),
                });
            }
            Ok(Some(instruction)) => instructions.push(instruction),
            Err(()) => {
                return Err(TodoParseError::InvalidLine {
                    number,
                    line: raw_line.to_string(),
                });
            }
        }
    }
    Ok(instructions)
}

fn split_command(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(idx) => (&line[..idx], line[idx..].trim_start()),
        None => (line, ""),
    }
}

fn parse_instruction(command: &str, rest: &str) -> Result<Option<TodoInstruction>, ()> {
    match command {
        "pick" | "p" => require_commit(rest).map(|commit| Some(TodoInstruction::Pick { commit })),
        "reword" | "r" => {
            require_commit(rest).map(|commit| Some(TodoInstruction::Reword { commit }))
        }
        "edit" | "e" => require_commit(rest).map(|commit| Some(TodoInstruction::Edit { commit })),
        "squash" | "s" => {
            require_commit(rest).map(|commit| Some(TodoInstruction::Squash { commit }))
        }
        "fixup" | "f" => parse_fixup(rest).map(Some),
        "exec" | "x" => {
            let cmd = rest.trim();
            if cmd.is_empty() {
                Err(())
            } else {
                Ok(Some(TodoInstruction::Exec {
                    cmd: cmd.to_string(),
                }))
            }
        }
        "break" | "b" => Ok(Some(TodoInstruction::Break)),
        "drop" | "d" => {
            if rest.trim().is_empty() {
                Err(())
            } else {
                Ok(Some(TodoInstruction::Drop {
                    commit: Some(first_token(rest).to_string()),
                }))
            }
        }
        _ => Ok(None),
    }
}

fn parse_fixup(rest: &str) -> Result<TodoInstruction, ()> {
    let mut tokens = rest.split_whitespace();
    let first = tokens.next().ok_or(())?;
    let (flag, commit) = match first {
        "-C" => (Some(FixupFlag::KeepThis), tokens.next().ok_or(())?),
        "-c" => (Some(FixupFlag::Reword), tokens.next().ok_or(())?),
        commit => (None, commit),
    };
    Ok(TodoInstruction::Fixup {
        commit: commit.to_string(),
        flag,
    })
}

fn require_commit(rest: &str) -> Result<String, ()> {
    let commit = first_token(rest);
    if commit.is_empty() {
        Err(())
    } else {
        Ok(commit.to_string())
    }
}

fn first_token(rest: &str) -> &str {
    rest.split_whitespace().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{
        FixupFlag, ONGOING_REBASE_TODO_HINT, TODO_HELP_COMMENTS, TodoInstruction, TodoParseError,
        TodoRenderCommit, parse_todo, render_remaining_todo, render_todo,
        render_todo_with_commands,
    };
    use crate::command::rebase::{RebaseAuxState, rebase_aux_gc_oids};

    #[test]
    fn parse_full_and_abbreviated_commands() {
        let text = "\
pick aaa1111 # keep
p bbb2222
reword ccc3333
r ddd4444
edit eee5555
e fff6666
squash ggg7777
s hhh8888
fixup iii9999
f jjj0000
fixup -C kkk1111
f -c lll2222
exec echo one
x true
break
b
drop mmm3333
d nnn4444

# comment is ignored

";
        let parsed = parse_todo(text).expect("full and abbreviated commands must parse");
        assert_eq!(
            parsed,
            vec![
                TodoInstruction::Pick {
                    commit: "aaa1111".into()
                },
                TodoInstruction::Pick {
                    commit: "bbb2222".into()
                },
                TodoInstruction::Reword {
                    commit: "ccc3333".into()
                },
                TodoInstruction::Reword {
                    commit: "ddd4444".into()
                },
                TodoInstruction::Edit {
                    commit: "eee5555".into()
                },
                TodoInstruction::Edit {
                    commit: "fff6666".into()
                },
                TodoInstruction::Squash {
                    commit: "ggg7777".into()
                },
                TodoInstruction::Squash {
                    commit: "hhh8888".into()
                },
                TodoInstruction::Fixup {
                    commit: "iii9999".into(),
                    flag: None,
                },
                TodoInstruction::Fixup {
                    commit: "jjj0000".into(),
                    flag: None,
                },
                TodoInstruction::Fixup {
                    commit: "kkk1111".into(),
                    flag: Some(FixupFlag::KeepThis),
                },
                TodoInstruction::Fixup {
                    commit: "lll2222".into(),
                    flag: Some(FixupFlag::Reword),
                },
                TodoInstruction::Exec {
                    cmd: "echo one".into()
                },
                TodoInstruction::Exec { cmd: "true".into() },
                TodoInstruction::Break,
                TodoInstruction::Break,
                TodoInstruction::Drop {
                    commit: Some("mmm3333".into())
                },
                TodoInstruction::Drop {
                    commit: Some("nnn4444".into())
                },
            ]
        );
    }

    #[test]
    fn parse_reports_invalid_command_and_line() {
        let command = parse_todo("frobnicate abcdef0\n").expect_err("unknown command");
        assert_eq!(
            command,
            TodoParseError::InvalidCommand {
                command: "frobnicate".into(),
                number: 1,
                line: "frobnicate abcdef0".into(),
            }
        );
        assert_eq!(command.to_string(), "invalid command 'frobnicate'");

        let line = parse_todo("# keep\npick\n").expect_err("pick without commit");
        assert_eq!(
            line,
            TodoParseError::InvalidLine {
                number: 2,
                line: "pick".into()
            }
        );
        assert_eq!(line.to_string(), "invalid line 2: pick");
    }

    #[test]
    fn render_matches_git_todo_template() {
        let rendered = render_todo(
            &[
                TodoRenderCommit {
                    abbrev: "aaa1111".into(),
                    subject: "A".into(),
                },
                TodoRenderCommit {
                    abbrev: "bbb2222".into(),
                    subject: "B".into(),
                },
            ],
            "onto000",
            "headfff",
        );
        let expected = format!(
            "pick aaa1111 # A\n\
             pick bbb2222 # B\n\
             \n\
             # Rebase onto000..headfff onto onto000 (2 commands)\n\
             {TODO_HELP_COMMENTS}"
        );
        assert_eq!(rendered, expected);
        assert!(
            !rendered.contains("l, label")
                && !rendered.contains("t, reset")
                && !rendered.contains("m, merge")
                && !rendered.contains("u, update-ref"),
            "DEFER-02 help lines must stay omitted: {rendered}"
        );
        let one = render_todo(
            &[TodoRenderCommit {
                abbrev: "aaa1111".into(),
                subject: "A".into(),
            }],
            "onto000",
            "headfff",
        );
        assert!(
            one.contains("# Rebase onto000..headfff onto onto000 (1 command)\n"),
            "{one}"
        );
    }

    #[test]
    fn render_todo_with_commands_counts_injected_exec_lines() {
        let rendered = render_todo_with_commands(
            &[
                "pick aaa1111 # A".into(),
                "exec true".into(),
                "pick bbb2222 # B".into(),
                "exec true".into(),
            ],
            "onto000",
            "headfff",
        );
        assert!(
            rendered.starts_with("pick aaa1111 # A\nexec true\npick bbb2222 # B\nexec true\n\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("# Rebase onto000..headfff onto onto000 (4 commands)\n"),
            "{rendered}"
        );
        assert!(rendered.contains(TODO_HELP_COMMENTS), "{rendered}");
    }

    #[test]
    fn render_remaining_todo_lists_only_remaining_and_ongoing_hint() {
        let rendered = render_remaining_todo(
            &[
                TodoInstruction::Pick {
                    commit: "bbb2222".into(),
                },
                TodoInstruction::Exec { cmd: "true".into() },
                TodoInstruction::Break,
            ],
            "onto000",
            "headfff",
        );
        assert!(
            rendered.starts_with("pick bbb2222\nexec true\nbreak\n\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("# Rebase onto000..headfff onto onto000 (3 commands)\n"),
            "{rendered}"
        );
        assert!(rendered.contains(ONGOING_REBASE_TODO_HINT), "{rendered}");
        assert!(
            rendered.contains(TODO_HELP_COMMENTS),
            "help comments stay after the ongoing hint: {rendered}"
        );
        assert!(
            !rendered.contains("pick aaa"),
            "already-applied commands must not be rewritten: {rendered}"
        );
    }

    #[test]
    fn aux_todo_instructions_roundtrip_and_gc_roots() {
        let todo = vec![
            TodoInstruction::Pick {
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            },
            TodoInstruction::Exec { cmd: "true".into() },
            TodoInstruction::Break,
        ];
        let done = vec![
            TodoInstruction::Drop {
                commit: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
            },
            TodoInstruction::Fixup {
                commit: "cccccccccccccccccccccccccccccccccccccccc".into(),
                flag: Some(FixupFlag::KeepThis),
            },
        ];
        let aux = RebaseAuxState::with_todo_instructions(todo.clone(), done.clone());
        let json = serde_json::to_vec_pretty(&aux).expect("serialize aux");
        let back: RebaseAuxState = serde_json::from_slice(&json).expect("deserialize aux");
        assert_eq!(back.todo_instructions, todo);
        assert_eq!(back.done_instructions, done);

        let dir = tempfile::tempdir().expect("temp gitdir");
        std::fs::write(dir.path().join("rebase-aux.json"), &json).expect("write aux");
        let oids = rebase_aux_gc_oids(dir.path())
            .expect("gc scan")
            .expect("aux present");
        let collected: Vec<&str> = oids.iter().map(|(_, oid)| oid.as_str()).collect();
        assert!(
            collected.contains(&"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "{oids:?}"
        );
        assert!(
            collected.contains(&"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            "{oids:?}"
        );
        assert!(
            collected.contains(&"cccccccccccccccccccccccccccccccccccccccc"),
            "{oids:?}"
        );
        assert_eq!(
            collected.len(),
            3,
            "exec/break must not invent oids: {oids:?}"
        );
    }
}
