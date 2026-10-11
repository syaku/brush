//! Process management utilities

use std::path::Path;

pub(crate) use crate::sys::tokio_process::*;

/// Creates a command to run the executable at `program` as a child of the shell, starting in
/// `working_dir` (the shell's working directory) rather than the host process's. If
/// `working_dir` is empty (the shell doesn't know where it is), the child starts in the host
/// process's.
///
/// `program` is a path the shell has already found -- absolute, or relative to `working_dir`
/// -- never a name to search for.
///
/// Windows looks a relative path up against the host process's working directory, not the
/// child's, so `program` is resolved against `working_dir` first. A `PATHEXT` extension is
/// then appended if that names the file, as `cmd.exe` does for `.\tool` or `bin\tool`.
///
/// # Arguments
///
/// * `program` - The path of the executable to run.
/// * `working_dir` - The directory the child starts in.
pub(crate) fn create_command(
    program: impl AsRef<Path>,
    working_dir: &crate::ResolvedPath,
) -> std::process::Command {
    let program = working_dir.join(program.as_ref());
    // Not left to `CreateProcess`: it only ever tries `.exe`, so `tool.cmd` isn't found, and
    // an extensionless file beside it (npm's POSIX shim) is run instead and fails as not a
    // Win32 application. A path that resolves to nothing is kept so the spawn reports it.
    let program = crate::sys::fs::resolve_executable(program.clone()).unwrap_or(program);
    let mut command = std::process::Command::new(program.as_path());
    if !working_dir.is_empty() {
        command.current_dir(working_dir);
    }
    command
}

#[cfg(test)]
#[expect(clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    fn resolved(path: &str) -> crate::ResolvedPath {
        crate::ResolvedPath::try_from(std::path::PathBuf::from(path)).unwrap_or_default()
    }

    #[test]
    fn relative_program_is_resolved_against_the_working_directory() {
        let working_dir = resolved(r"C:\work");

        for (program, expected) in [
            (r"prog.exe", r"C:\work\prog.exe"),
            (r"bin\prog.exe", r"C:\work\bin\prog.exe"),
            (r".\prog.exe", r"C:\work\.\prog.exe"),
            (r"D:prog.exe", r"D:\prog.exe"),
            (r"C:\bin\prog.exe", r"C:\bin\prog.exe"),
        ] {
            let command = create_command(program, &working_dir);
            assert_eq!(command.get_program(), expected);
            assert_eq!(command.get_current_dir(), Some(Path::new(r"C:\work")));
        }
    }

    #[test]
    fn pathext_extension_is_appended_to_a_program_given_by_path() -> std::io::Result<()> {
        let scratch = tempfile::tempdir()?;
        std::fs::create_dir(scratch.path().join("bin"))?;
        std::fs::write(scratch.path().join("bin").join("tool.cmd"), "@echo off\r\n")?;
        let working_dir = crate::ResolvedPath::try_from(scratch.path().to_owned())?;

        for program in [r"bin\tool", "bin/tool", r".\bin\tool", "./bin/tool"] {
            let command = create_command(program, &working_dir);
            assert_eq!(
                Path::new(command.get_program()),
                scratch.path().join("bin").join("tool.cmd"),
                "program: {program}"
            );
        }

        Ok(())
    }

    #[test]
    fn pathext_match_wins_over_an_extensionless_file() -> std::io::Result<()> {
        // npm installs a POSIX shell script `npx` beside `npx.cmd`; only the latter runs.
        let scratch = tempfile::tempdir()?;
        std::fs::write(scratch.path().join("npx"), "#!/bin/sh\n")?;
        std::fs::write(scratch.path().join("npx.cmd"), "@echo off\r\n")?;
        let working_dir = crate::ResolvedPath::try_from(scratch.path().to_owned())?;

        let command = create_command("./npx", &working_dir);
        assert_eq!(
            Path::new(command.get_program()),
            scratch.path().join("npx.cmd")
        );

        Ok(())
    }
}
