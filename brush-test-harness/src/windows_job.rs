//! Running a test's shell in a Windows job object.

use std::{
    io::Write as _,
    os::windows::io::AsRawHandle as _,
    process::{Command, Output, Stdio},
    time::Duration,
};

use anyhow::{Result, anyhow};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
        Threading::WaitForSingleObject,
    },
};

struct Job(HANDLE);

impl Job {
    fn new() -> Result<Self> {
        // SAFETY: Null attributes and a null name are both documented as valid.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(handle))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: The handle came from `CreateJobObjectW` and is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Runs `cmd` to completion or until `timeout`, feeding it `stdin` and collecting its output.
///
/// Reading the output to its end can't be left to `assert_cmd`: a process the shell leaves
/// behind (say, one that stopped itself under the MSYS2 runtime, which nothing can continue
/// once the shell exits) holds the pipes open, so the read never ends and a timeout on the
/// shell doesn't help. Ending the job when the shell is done closes them.
pub(crate) fn output_in_job(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output> {
    let job = Job::new()?;

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // SAFETY: Both handles are valid: the job's is owned by `job`, and the process's by `child`.
    let assigned = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle().cast()) } != 0;
    // A shell that has already exited can't be assigned, and has nothing left to wait for.
    if !assigned && child.try_wait()?.is_none() {
        let error = std::io::Error::last_os_error();
        child.kill()?;
        child.wait()?;
        return Err(error.into());
    }

    let mut child_stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
    let input = stdin.unwrap_or_default().to_vec();
    let writer = std::thread::spawn(move || {
        // A shell that exits without reading all of its input isn't an error.
        let _ = child_stdin.write_all(&input);
    });
    let stdout = read_on_thread(
        child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout pipe"))?,
    );
    let stderr = read_on_thread(
        child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("no stderr pipe"))?,
    );

    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: The handle is owned by `child`, which outlives the wait.
    unsafe { WaitForSingleObject(child.as_raw_handle().cast(), timeout_ms) };
    // SAFETY: The handle is owned by `job`.
    unsafe { TerminateJobObject(job.0, 1) };

    let status = child.wait()?;
    let _ = writer.join();
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn read_on_thread(
    mut pipe: impl std::io::Read + Send + 'static,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = vec![];
        // Whatever was read before a failure is still the process's output.
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}
