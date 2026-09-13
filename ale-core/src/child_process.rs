//! Ownership-based cleanup for application helper processes. Never use for user-opened apps.
use std::io;
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

pub fn configure(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

/// Allows an explicit user app launch to outlive the helper's job.
#[cfg(windows)]
pub fn configure_user_app(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x01000000 | 0x08000000);
}

pub struct ProcessTree {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}
unsafe impl Send for ProcessTree {}
unsafe impl Sync for ProcessTree {}
impl ProcessTree {
    pub fn attach(pid: u32) -> io::Result<Self> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
            use windows_sys::Win32::System::JobObjects::*;
            use windows_sys::Win32::System::Threading::*;
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(error);
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, pid);
            if process.is_null() {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(error);
            }
            let assigned = AssignProcessToJobObject(job, process);
            let error = io::Error::last_os_error();
            CloseHandle(process);
            if assigned == 0 {
                CloseHandle(job);
                return Err(error);
            }
            Ok(Self { job })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { pid })
        }
    }
    pub fn terminate(&self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
    }
}
impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

pub struct ManagedChild {
    pub child: std::process::Child,
    tree: ProcessTree,
}
impl ManagedChild {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        configure(command);
        let mut child = command.spawn()?;
        match ProcessTree::attach(child.id()) {
            Ok(tree) => Ok(Self { child, tree }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    pub fn terminate(&mut self) {
        self.tree.terminate();
        let _ = self.child.kill();
    }
    pub fn kill_tree_and_wait(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        self.terminate();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "helper exit deadline",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.try_wait();
    }
}

pub struct ManagedAsyncChild {
    pub child: tokio::process::Child,
    tree: ProcessTree,
}
impl ManagedAsyncChild {
    pub fn spawn(command: &mut tokio::process::Command) -> io::Result<Self> {
        configure(command.as_std_mut());
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("missing helper process id"))?;
        match ProcessTree::attach(pid) {
            Ok(tree) => Ok(Self { child, tree }),
            Err(error) => {
                let _ = child.start_kill();
                Err(error)
            }
        }
    }
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    pub fn start_kill(&mut self) -> io::Result<()> {
        self.tree.terminate();
        self.child.start_kill()
    }
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }
    pub async fn kill_tree_and_wait(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let _ = self.start_kill();
        tokio::time::timeout(timeout, self.wait())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "helper exit deadline"))?
    }
}
impl Drop for ManagedAsyncChild {
    fn drop(&mut self) {
        let _ = self.start_kill();
    }
}
