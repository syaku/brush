//! Process management utilities

use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::FutureExt as _;

use crate::{
    ExecutionResult, ShellFd,
    openfiles::OpenFiles,
    processes::{ChildProcess, LaunchRequest, ProcessGroup},
    sys::{
        self,
        commands::{self, CommandExt, CommandFdInjectionExt, CommandFgControlExt},
    },
};

pub(crate) type ProcessId = i32;

/// Starts a native child process.
///
/// # Arguments
///
/// * `command` - The process to start.
pub(crate) fn spawn(command: LaunchRequest) -> std::io::Result<ChildProcess> {
    let process_group = command.process_group;
    // See `CreateOptions::kill_external_commands_on_drop` (false for ordinary shells).
    let kill_on_drop = command.kill_on_drop;
    let interrupted = Arc::clone(&command.interrupted);

    let mut command = tokio::process::Command::from(compose_std_command(command)?);
    command.kill_on_drop(kill_on_drop);
    let child = command.spawn()?;

    #[expect(clippy::cast_possible_wrap)]
    let pid = child.id().map(|id| id as ProcessId);
    if pid.is_none() {
        tracing::warn!("could not retrieve pid for child process");
    }

    let pgid = match process_group {
        ProcessGroup::Inherit => None,
        ProcessGroup::Join(pgid) => Some(pgid),
        ProcessGroup::New { .. } | ProcessGroup::NewSession => pid,
    };

    let exited = child.wait_with_output().map(move |output| {
        output.map(|output| {
            mark_if_ended_by_ctrl_c(output.status, &interrupted);
            ExecutionResult::from(output)
        })
    });
    Ok(ChildProcess::from_future(exited).native_child(pid, pgid))
}

// Done on the raw status, not the `ExecutionResult`: that reports a Ctrl-C exit as 130, the same
// code a child can return for itself.
fn mark_if_ended_by_ctrl_c(status: std::process::ExitStatus, interrupted: &AtomicBool) {
    if sys::signal::exited_from_ctrl_c(status) {
        interrupted.store(true, Ordering::SeqCst);
    }
}

/// Replaces the current process with a new one. Returns only if that fails, with
/// the reason why.
///
/// # Arguments
///
/// * `command` - The process to replace the current one with.
pub(crate) fn exec(command: LaunchRequest) -> std::io::Error {
    match compose_std_command(command) {
        Ok(command) => commands::exec(command),
        Err(err) => err,
    }
}

/// Composes the `std::process::Command` that starts a process.
fn compose_std_command(command: LaunchRequest) -> std::io::Result<std::process::Command> {
    let mut cmd = sys::process::create_command(&command.program, &command.working_dir);

    // Override argv[0].
    // NOTE: Not supported on all platforms.
    cmd.arg0(command.argv0);
    commands::set_args(&mut cmd, &command.args);
    cmd.env_clear();
    cmd.envs(command.env);

    // A closed standard stream gets the null device.
    // TODO(redirection): close it instead, as bash does (e.g., with a `pre_exec`
    // on Unix), so that the process fails to use it.
    let mut open_files = command.open_files;
    cmd.stdin(take_stdio(&mut open_files, OpenFiles::STDIN_FD)?);
    cmd.stdout(take_stdio(&mut open_files, OpenFiles::STDOUT_FD)?);
    cmd.stderr(take_stdio(&mut open_files, OpenFiles::STDERR_FD)?);

    cmd.inject_fds(open_files.iter_fds().map(|(fd, file)| (fd, file.clone())))
        .map_err(std::io::Error::other)?;

    match command.process_group {
        ProcessGroup::Inherit => {}
        ProcessGroup::Join(pgid) => {
            cmd.process_group(pgid);
        }
        ProcessGroup::New { take_foreground } => {
            cmd.process_group(0);
            if take_foreground {
                cmd.take_foreground();
            }
        }
        // setsid() makes a new process group too.
        ProcessGroup::NewSession => cmd.lead_session(),
    }

    Ok(cmd)
}

/// Takes the file for standard stream `fd` out of `open_files`, for the process
/// to have as that stream: the null device if it's closed.
fn take_stdio(open_files: &mut OpenFiles, fd: ShellFd) -> std::io::Result<Stdio> {
    match open_files.remove_fd(fd) {
        Some(file) => commands::stdio_for(file),
        None => Ok(Stdio::null()),
    }
}
