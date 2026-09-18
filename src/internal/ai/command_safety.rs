//! Shell command safety classification — plan-20260920 RC-09.
//!
//! KEEP `sandbox` / `automation` call this module instead of
//! `tools::utils::classify_ai_command_safety`. The LibraVcs arm stays
//! in `tools` until RC-23.

use std::{ffi::OsStr, path::Path};

use crate::internal::ai::{
    hardening::{BlastRadius, SafetyDecision, SafetyDisposition},
    libra_vcs,
};

/// Returns true when a shell command appears to invoke Git as a version-control
/// executable. This is deliberately conservative: Libra-managed agents must use
/// Libra VCS tools instead of shelling out to `git`.
pub fn command_invokes_git_version_control(command: &str) -> bool {
    shlex::split(command).is_some_and(|words| shell_words_invoke_git(&words))
}

fn shell_words_invoke_git(words: &[String]) -> bool {
    let mut start = 0;
    for (idx, word) in words.iter().enumerate() {
        if matches!(word.as_str(), "&&" | "||" | ";" | "|") {
            if shell_segment_invokes_git(&words[start..idx]) {
                return true;
            }
            start = idx + 1;
        }
    }

    shell_segment_invokes_git(&words[start..])
}

fn shell_segment_invokes_git(segment: &[String]) -> bool {
    let mut idx = 0;
    while segment
        .get(idx)
        .is_some_and(|word| word.contains('=') && !word.starts_with('-'))
    {
        idx += 1;
    }

    while matches!(
        segment.get(idx).map(String::as_str),
        Some("command" | "sudo" | "env")
    ) {
        idx += 1;
        while segment
            .get(idx)
            .is_some_and(|word| word.contains('=') && !word.starts_with('-'))
        {
            idx += 1;
        }
    }

    segment
        .get(idx)
        .and_then(|word| executable_name(word))
        .is_some_and(|name| name == "git")
}

pub fn classify_shell_command_safety(command: &str) -> SafetyDecision {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return SafetyDecision::deny("shell.empty", "empty shell command", BlastRadius::Unknown);
    }

    if !trimmed.is_ascii() {
        return SafetyDecision::needs_human(
            "shell.non_ascii_command",
            "shell command contains non-ASCII characters and needs review",
            BlastRadius::Unknown,
        );
    }

    if command_invokes_git_version_control(trimmed) {
        return SafetyDecision::deny(
            "shell.direct_git_forbidden",
            "AI shell tools must use Libra VCS tools instead of invoking git directly",
            BlastRadius::Repository,
        );
    }

    let lower = trimmed.to_ascii_lowercase();
    if network_command_piped_to_shell(&lower) {
        return SafetyDecision::deny(
            "shell.network_code_execution",
            "network download piped into a shell is not allowed",
            BlastRadius::Network,
        );
    }

    if lower.contains("$(") || lower.contains('`') {
        return SafetyDecision::needs_human(
            "shell.dynamic_evaluation",
            "shell command uses dynamic evaluation or command substitution",
            BlastRadius::System,
        );
    }

    if contains_redirection_or_pipeline(&lower) {
        return SafetyDecision::needs_human(
            "shell.redirection_or_pipeline",
            "shell command combines commands or redirects data and needs review",
            shell_meta_blast_radius(&lower),
        );
    }

    let Some(parts) = shlex::split(trimmed).filter(|parts| !parts.is_empty()) else {
        return SafetyDecision::needs_human(
            "shell.unparseable",
            "shell command could not be parsed into plain words",
            BlastRadius::Unknown,
        );
    };

    classify_shell_words(&parts)
}

fn classify_shell_words(parts: &[String]) -> SafetyDecision {
    let Some(cmd) = executable_name(parts.first().map(String::as_str).unwrap_or_default()) else {
        return SafetyDecision::needs_human(
            "shell.unparseable",
            "shell command has no executable name",
            BlastRadius::Unknown,
        );
    };

    if matches!(cmd.as_str(), "command" | "env")
        && let Some(decision) = classify_shell_wrapper_words(&cmd, &parts[1..])
    {
        return decision;
    }

    if is_shell_interpreter(&cmd)
        && let Some(script) = shell_c_script(&parts[1..])
    {
        let decision = classify_shell_command_safety(script);
        return match decision.disposition {
            SafetyDisposition::Deny => decision,
            _ => SafetyDecision::needs_human(
                "shell.dynamic_evaluation",
                "shell interpreter command uses -c and needs review",
                BlastRadius::System,
            ),
        };
    }

    if cmd == "git" {
        return SafetyDecision::deny(
            "shell.direct_git_forbidden",
            "AI shell tools must use Libra VCS tools instead of invoking git directly",
            BlastRadius::Repository,
        );
    }

    if cmd == "sudo" {
        if let Some(blast_radius) = destructive_shell_blast_radius(&parts[1..]) {
            return SafetyDecision::deny(
                "shell.destructive_command",
                "privileged destructive shell command is not allowed",
                match blast_radius {
                    BlastRadius::Workspace => BlastRadius::System,
                    other => other,
                },
            );
        }
        return SafetyDecision::needs_human(
            "shell.privileged_execution",
            "privileged shell command needs human approval",
            BlastRadius::System,
        );
    }

    if let Some(blast_radius) = destructive_shell_blast_radius(parts) {
        return SafetyDecision::deny(
            "shell.destructive_command",
            "destructive shell command is not allowed",
            blast_radius,
        );
    }

    if cmd == "libra" {
        return classify_shell_libra_command(parts);
    }

    if is_network_executable(&cmd) {
        return SafetyDecision::needs_human(
            "shell.network_access",
            "shell command requires network access",
            BlastRadius::Network,
        );
    }

    if shell_words_are_read_only(&cmd, &parts[1..]) {
        return SafetyDecision::allow(
            "shell.read_only_allowlist",
            "read-only shell command is allowlisted",
            BlastRadius::Workspace,
        );
    }

    SafetyDecision::needs_human(
        "shell.workspace_mutation_or_execution",
        "shell command may execute code or mutate the workspace",
        BlastRadius::Workspace,
    )
}

fn classify_shell_wrapper_words(wrapper: &str, args: &[String]) -> Option<SafetyDecision> {
    let mut idx = 0;
    if wrapper == "env" {
        idx = skip_env_prefix(args);
    }
    if idx >= args.len() {
        return Some(SafetyDecision::needs_human(
            "shell.workspace_mutation_or_execution",
            "shell wrapper command without an executable needs review",
            BlastRadius::Workspace,
        ));
    }
    Some(classify_shell_words(&args[idx..]))
}

fn skip_env_prefix(args: &[String]) -> usize {
    let mut idx = 0;
    while let Some(arg) = args.get(idx).map(String::as_str) {
        if arg.contains('=') && !arg.starts_with('-') {
            idx += 1;
            continue;
        }
        if arg.starts_with("--unset=") || arg.starts_with("--chdir=") {
            idx += 1;
            continue;
        }
        if matches!(arg, "-i" | "--ignore-environment" | "-0" | "--null") {
            idx += 1;
            continue;
        }
        if matches!(arg, "-u" | "--unset" | "-C" | "--chdir") {
            idx += 1;
            if idx < args.len() {
                idx += 1;
            }
            continue;
        }
        break;
    }
    idx
}

fn is_shell_interpreter(cmd: &str) -> bool {
    matches!(cmd, "bash" | "dash" | "sh" | "zsh")
}

fn shell_c_script(args: &[String]) -> Option<&str> {
    args.windows(2).find_map(|pair| {
        if pair[0] == "-c" {
            Some(pair[1].as_str())
        } else {
            None
        }
    })
}

fn classify_shell_libra_command(parts: &[String]) -> SafetyDecision {
    let Some(subcommand) = parts.get(1).map(String::as_str) else {
        return SafetyDecision::needs_human(
            "shell.workspace_mutation_or_execution",
            "libra command without a subcommand needs review",
            BlastRadius::Repository,
        );
    };
    let decision = libra_vcs::classify_run_libra_vcs_safety(subcommand, &parts[2..]);
    if decision.is_allow() {
        SafetyDecision::allow(
            "shell.libra_read_only",
            "read-only libra command is allowlisted",
            BlastRadius::Repository,
        )
    } else {
        decision
    }
}

fn contains_redirection_or_pipeline(command: &str) -> bool {
    command.contains(['>', '<', '|', ';']) || command.contains("&&") || command.contains("||")
}

fn shell_meta_blast_radius(command: &str) -> BlastRadius {
    if command.contains('>') || command.contains('<') {
        BlastRadius::Workspace
    } else {
        BlastRadius::Unknown
    }
}

fn network_command_piped_to_shell(command: &str) -> bool {
    (command.starts_with("curl ")
        || command.starts_with("wget ")
        || command.contains(" curl ")
        || command.contains(" wget "))
        && command.contains('|')
        && (command.contains(" sh") || command.contains(" bash"))
}

fn shell_words_are_read_only(cmd: &str, args: &[String]) -> bool {
    match cmd {
        "cat" | "cut" | "echo" | "false" | "grep" | "head" | "id" | "ls" | "nl" | "paste"
        | "pwd" | "rev" | "seq" | "stat" | "tail" | "tr" | "true" | "uname" | "uniq" | "wc"
        | "which" | "whoami" => true,
        "rg" => !args.iter().map(String::as_str).any(|arg| {
            matches!(arg, "--pre" | "--hostname-bin" | "--search-zip" | "-z")
                || arg.starts_with("--pre=")
                || arg.starts_with("--hostname-bin=")
        }),
        "find" => !args.iter().map(String::as_str).any(|arg| {
            matches!(
                arg,
                "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-delete"
                    | "-fls"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
            )
        }),
        "sed" => args
            .first()
            .is_some_and(|arg| arg == "-n" && args.get(1).is_some_and(|arg| sed_print_arg(arg))),
        _ => false,
    }
}

fn destructive_shell_blast_radius(parts: &[String]) -> Option<BlastRadius> {
    let cmd = executable_name(parts.first().map(String::as_str)?)?;
    let args = &parts[1..];

    match cmd.as_str() {
        "rm" if rm_args_are_recursive_force(args) => {
            if args
                .iter()
                .any(|arg| arg == "/" || arg.starts_with("/dev/"))
            {
                Some(BlastRadius::System)
            } else {
                Some(BlastRadius::Workspace)
            }
        }
        "chmod"
            if args.iter().any(|arg| arg == "-R" || arg.starts_with("-R"))
                && args.iter().any(|arg| arg == "777") =>
        {
            Some(BlastRadius::Workspace)
        }
        "chown" if args.iter().any(|arg| arg == "-R" || arg.starts_with("-R")) => {
            Some(BlastRadius::Workspace)
        }
        "dd" if args.iter().any(|arg| arg.starts_with("of=/dev/")) => Some(BlastRadius::System),
        "mkfs" | "mkfs.ext4" | "shutdown" | "reboot" | "poweroff" => Some(BlastRadius::System),
        _ => None,
    }
}

fn rm_args_are_recursive_force(args: &[String]) -> bool {
    let recursive = args.iter().map(String::as_str).any(|arg| {
        matches!(arg, "-r" | "-R" | "--recursive") || short_flag_group_contains(arg, 'r')
    });
    let force = args
        .iter()
        .map(String::as_str)
        .any(|arg| matches!(arg, "-f" | "--force") || short_flag_group_contains(arg, 'f'));
    recursive && force
}

fn is_network_executable(cmd: &str) -> bool {
    matches!(
        cmd,
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "nc" | "netcat" | "gh"
    )
}

fn executable_name(command: &str) -> Option<String> {
    Path::new(command)
        .file_name()
        .and_then(OsStr::to_str)
        .map(|name| name.trim_end_matches(".exe").to_ascii_lowercase())
}

fn short_flag_group_contains(arg: &str, target: char) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.chars().skip(1).any(|c| c == target)
}

fn sed_print_arg(arg: &str) -> bool {
    let Some(core) = arg.strip_suffix('p') else {
        return false;
    };
    let parts: Vec<&str> = core.split(',').collect();
    match parts.as_slice() {
        [one] => !one.is_empty() && one.chars().all(|ch| ch.is_ascii_digit()),
        [start, end] => {
            !start.is_empty()
                && !end.is_empty()
                && start.chars().all(|ch| ch.is_ascii_digit())
                && end.chars().all(|ch| ch.is_ascii_digit())
        }
        _ => false,
    }
}
