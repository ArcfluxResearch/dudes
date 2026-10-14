//! Windows `ConPTY` setup with creation-time Job membership and kill-on-close ownership.

#![allow(unsafe_code)]

use std::fs::File;
use std::io::{self, Write as _};
use std::mem::size_of;
use std::os::windows::io::{
    AsHandle, AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle,
};
use std::thread;
use std::time::{Duration, Instant};

use dudes_providers::launch::LaunchSpec;
use windows_spawn::{AsPseudoConsole, Command, Job, SpawnOptions};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::{
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    QueryInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;

use crate::PtySize;
use crate::tree::{CloseReport, Spawned};

const JOB_EMPTY_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct Session {
    child: windows_spawn::Child,
    job: Job,
    console: Option<PseudoConsole>,
    writer: Option<File>,
    reaped: bool,
}

pub(crate) fn spawn(spec: LaunchSpec, size: PtySize) -> io::Result<Spawned> {
    let job = Job::create()?;
    job.set_kill_on_close(true)?;
    let (console, writer, reader) = PseudoConsole::create(size)?;

    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);
    command.env_clear();
    for env in spec.env {
        command.env(env.name(), env.expose_value());
    }
    command.current_dir(spec.cwd);
    let child = command.spawn_with(SpawnOptions::new().job(&job).pseudoconsole(&console))?;

    Ok(Spawned {
        session: Session {
            child,
            job,
            console: Some(console),
            writer: Some(writer),
            reaped: false,
        },
        reader,
        issue: None,
    })
}

impl Session {
    pub(crate) fn close_query(&self) -> io::Result<CloseQuery> {
        if !self.reaped {
            return Err(io::Error::other("PTY direct child is not reaped"));
        }
        self.job().map(|job| CloseQuery { job })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn job(&self) -> io::Result<OwnedHandle> {
        self.job.as_handle().try_clone_to_owned()
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY input is closed"))?
            .write(bytes)
    }

    pub(crate) fn resize(&mut self, size: PtySize) -> io::Result<()> {
        self.console
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "PTY is closed"))?
            .resize(size)
    }

    pub(crate) fn close(&mut self) -> CloseReport {
        let mut first = None;
        let observed_exited = match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                first.get_or_insert(error);
                false
            }
        };
        let job_signalled = match self.job.terminate(1) {
            Ok(()) => true,
            Err(error) => {
                first.get_or_insert(error);
                false
            }
        };
        let mut direct_done = observed_exited;
        if !direct_done {
            let direct_signal = if job_signalled {
                Ok(())
            } else {
                self.child.kill()
            };
            match direct_signal {
                Ok(()) => match self.child.wait() {
                    Ok(_) => direct_done = true,
                    Err(error) => {
                        first.get_or_insert(error);
                    }
                },
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        let job_done = match wait_job_empty(&self.job) {
            Ok(()) => true,
            Err(error) => {
                first.get_or_insert(error);
                false
            }
        };
        self.reaped |= direct_done;
        CloseReport {
            confirmed: direct_done && job_done,
            error: first,
        }
    }
}

pub(crate) struct CloseQuery {
    job: OwnedHandle,
}

impl CloseQuery {
    pub(crate) fn is_absent(&self) -> io::Result<bool> {
        active_processes(&self.job).map(|active| active == 0)
    }
}

fn wait_job_empty(job: &Job) -> io::Result<()> {
    let deadline = Instant::now() + JOB_EMPTY_TIMEOUT;
    loop {
        if active_processes(job)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "PTY job still has active processes after termination",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn active_processes(job: &impl AsHandle) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: the owned Job handle remains live for the call. `info` is correctly sized,
    // aligned writable storage for JobObjectBasicAccountingInformation; the optional returned
    // length pointer is null, and the call retains neither pointer.
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_handle().as_raw_handle() as HANDLE,
            JobObjectBasicAccountingInformation,
            (&raw mut info).cast(),
            u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                .expect("job accounting size fits u32"),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info.ActiveProcesses)
    }
}

struct PseudoConsole(HPCON);

impl PseudoConsole {
    fn create(size: PtySize) -> io::Result<(Self, File, File)> {
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        let mut console = 0;
        // SAFETY: both pipe handles remain owned and live for the call; `console` is writable
        // HPCON storage. On success the returned value is immediately adopted by PseudoConsole.
        let result = unsafe {
            CreatePseudoConsole(
                coord(size),
                input_read.as_raw_handle() as HANDLE,
                output_write.as_raw_handle() as HANDLE,
                0,
                &raw mut console,
            )
        };
        if result < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole failed with HRESULT {result:#x}"
            )));
        }
        drop((input_read, output_write));
        Ok((
            Self(console),
            File::from(input_write),
            File::from(output_read),
        ))
    }

    fn resize(&self, size: PtySize) -> io::Result<()> {
        // SAFETY: self owns a live HPCON and no operation changes its numeric value.
        let result = unsafe { ResizePseudoConsole(self.0, coord(size)) };
        if result < 0 {
            Err(io::Error::other(format!(
                "ResizePseudoConsole failed with HRESULT {result:#x}"
            )))
        } else {
            Ok(())
        }
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: self uniquely owns the successful CreatePseudoConsole result and closes it once.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

// SAFETY: PseudoConsole uniquely owns a nonzero live HPCON for its full borrow by SpawnOptions;
// windows-spawn borrows but never closes or changes the value.
unsafe impl AsPseudoConsole for PseudoConsole {
    fn raw_pseudoconsole(&self) -> isize {
        self.0
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    // SAFETY: both output pointers are writable. Null security attributes request private,
    // non-inheritable handles; each successful result is immediately adopted exactly once.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreatePipe returns two valid distinct handles whose ownership transfers.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read as RawHandle),
            OwnedHandle::from_raw_handle(write as RawHandle),
        )
    })
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: i16::try_from(size.cols).expect("validated PTY columns"),
        Y: i16::try_from(size.rows).expect("validated PTY rows"),
    }
}
