//! The VM itself, through Virtualization.framework (`objc2-virtualization`).
//!
//! A `VZVirtualMachine` must be used on the serial dispatch queue it was made
//! with: every call here runs there (`exec_sync`), and the framework's
//! completion handlers, which run there too, send their result back over a
//! channel.

use std::os::fd::{FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::AllocAnyThread as _;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZDirectorySharingDeviceConfiguration, VZDiskImageCachingMode,
    VZDiskImageStorageDeviceAttachment, VZDiskImageSynchronizationMode,
    VZFileHandleSerialPortAttachment, VZGenericPlatformConfiguration, VZLinuxBootLoader,
    VZNATNetworkDeviceAttachment, VZSerialPortConfiguration, VZSharedDirectory,
    VZSingleDirectoryShare, VZStorageDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceSerialPortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioNetworkDeviceConfiguration,
    VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration,
    VZVirtualMachine, VZVirtualMachineConfiguration, VZVirtualMachineState,
};
use shipyard_runtime::attach::forward;

use crate::VmError;

/// What the VM is made with.
#[derive(Debug)]
pub(crate) struct Settings {
    /// The raw kernel.
    pub(crate) kernel: PathBuf,
    /// The initramfs: the guest's whole root.
    pub(crate) initramfs: PathBuf,
    /// The kernel's command line.
    pub(crate) cmdline: String,
    /// The data disk (a file).
    pub(crate) disk: PathBuf,
    /// The Mac's directory shared with the guest (the home: D10.3-3), by
    /// the tag `home`.
    pub(crate) home: PathBuf,
    /// The guest's console, written to.
    pub(crate) console: std::fs::File,
    /// CPUs and memory.
    pub(crate) cpus: usize,
    pub(crate) memory: u64,
}

/// A value used only on the VM's queue: `objc2`'s objects aren't `Send`,
/// and the framework wants them touched on that one queue, which is what
/// every use here does.
struct OnQueue<T>(T);

// SAFETY: an `OnQueue` is only ever read inside closures run on the VM's
// serial queue (`Machine::on_queue`), one at a time: never from two threads
// at once, never off the queue the framework requires.
unsafe impl<T> Send for OnQueue<T> {}

// SAFETY: shared between the host's threads only to be cloned (an atomic
// retain) and handed to the queue, as above: the object itself is used on
// the queue alone.
unsafe impl<T> Sync for OnQueue<T> {}

/// The VM, made and on its queue.
pub(crate) struct Machine {
    queue: DispatchRetained<DispatchQueue>,
    vm: OnQueue<Retained<VZVirtualMachine>>,
}

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine").finish_non_exhaustive()
    }
}

/// How the VM is, as the framework says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    /// Starting, or running.
    Up,
    /// Stopped, or stopping: its end.
    Down,
    /// The framework's own error state.
    Failed,
}

fn url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn described(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

/// The Linux boot loader: the raw kernel, the initramfs, the command line.
fn boot_loader(settings: &Settings) -> Retained<VZLinuxBootLoader> {
    // SAFETY: a new boot loader given valid URLs and a string.
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(
            VZLinuxBootLoader::alloc(),
            &url(&settings.kernel),
        );
        boot.setInitialRamdiskURL(Some(&url(&settings.initramfs)));
        boot.setCommandLine(&NSString::from_str(&settings.cmdline));
        boot
    }
}

/// The console: what the guest writes, to `log` (it reads nothing).
fn console(log: std::fs::File) -> Retained<VZSerialPortConfiguration> {
    // SAFETY: the file handle takes the descriptor over (closed with it);
    // the port is given a valid attachment.
    unsafe {
        let written = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            std::os::fd::IntoRawFd::into_raw_fd(log),
            true,
        );
        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        console.setAttachment(Some(
            &VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&written),
            ),
        ));
        Retained::into_super(console)
    }
}

/// The data disk, every write the guest syncs synced on the Mac's disk too
/// (`Full`: the engine's atomic writes rely on `fsync`).
fn data_disk(path: &Path) -> Result<Retained<VZStorageDeviceConfiguration>, VmError> {
    // SAFETY: a new attachment for a file URL, then a block device of it.
    unsafe {
        let disk =
            VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url(path),
                false,
                VZDiskImageCachingMode::Automatic,
                VZDiskImageSynchronizationMode::Full,
            )
            .map_err(|error| VmError::Machine(format!("the data disk: {}", described(&error))))?;
        let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &disk,
        );
        Ok(Retained::into_super(block))
    }
}

/// The home, shared read-write by the tag the guest mounts.
fn home_share(home: &Path) -> Retained<VZDirectorySharingDeviceConfiguration> {
    // SAFETY: new objects, each given valid ones of the classes it takes.
    unsafe {
        let share = VZSingleDirectoryShare::initWithDirectory(
            VZSingleDirectoryShare::alloc(),
            &VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &url(home), false),
        );
        let files = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &NSString::from_str(forward::HOME_TAG),
        );
        files.setShare(Some(&share));
        Retained::into_super(files)
    }
}

/// The VM's configuration from `settings`, validated.
fn configuration(settings: Settings) -> Result<Retained<VZVirtualMachineConfiguration>, VmError> {
    let boot = boot_loader(&settings);
    let disk = data_disk(&settings.disk)?;
    let share = home_share(&settings.home);
    let console = console(settings.console);
    // SAFETY: property setters on a new configuration, each given valid
    // objects of the classes it takes, before any VM exists.
    unsafe {
        let config = VZVirtualMachineConfiguration::new();
        config.setBootLoader(Some(&boot));
        config.setPlatform(&VZGenericPlatformConfiguration::new());
        config.setCPUCount(settings.cpus);
        config.setMemorySize(settings.memory);
        config.setSerialPorts(&NSArray::from_retained_slice(&[console]));
        config.setStorageDevices(&NSArray::from_retained_slice(&[disk]));
        config.setDirectorySharingDevices(&NSArray::from_retained_slice(&[share]));
        let network = VZVirtioNetworkDeviceConfiguration::new();
        network.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            network,
        )]));
        config.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioSocketDeviceConfiguration::new(),
        )]));
        config.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        config.validateWithError().map_err(|error| {
            let why = described(&error);
            if why.contains("com.apple.security.virtualization") {
                VmError::Unsupported(format!(
                    "{why} (sign this binary with scripts/macos/sign.sh)"
                ))
            } else {
                VmError::Machine(format!("the VM's configuration: {why}"))
            }
        })?;
        Ok(config)
    }
}

impl Machine {
    /// The VM made from `settings` (not started).
    pub(crate) fn new(settings: Settings) -> Result<Self, VmError> {
        let config = OnQueue(configuration(settings)?);
        let queue = DispatchQueue::new("dev.shipyard.vm", DispatchQueueAttr::SERIAL);
        let (sent, made) = mpsc::channel();
        let own = queue.clone();
        queue.exec_sync(move || {
            let config = config;
            // SAFETY: made on the queue it's given, as the framework asks.
            let vm = unsafe {
                VZVirtualMachine::initWithConfiguration_queue(
                    VZVirtualMachine::alloc(),
                    &config.0,
                    &own,
                )
            };
            let _ = sent.send(OnQueue(vm));
        });
        let vm = made
            .recv()
            .map_err(|_| VmError::Machine("the VM wasn't made".to_owned()))?;
        Ok(Self { queue, vm })
    }

    /// Runs `work` with the VM, on its queue, and returns what it returns
    /// (`None` only if it didn't run to its end: it panicked).
    fn on_queue<R: Send + 'static>(
        &self,
        work: impl FnOnce(&VZVirtualMachine) -> R + Send + 'static,
    ) -> Option<R> {
        let vm = OnQueue(self.vm.0.clone());
        let (sent, result) = mpsc::channel();
        self.queue.exec_sync(move || {
            let vm = vm;
            let _ = sent.send(work(&vm.0));
        });
        result.recv().ok()
    }

    /// Starts the VM, waiting up to `limit` for the framework's answer.
    pub(crate) fn start(&self, limit: Duration) -> Result<(), VmError> {
        let (sent, started) = mpsc::channel::<Result<(), String>>();
        self.on_queue(move |vm| {
            let handler = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: the framework passes null or a valid error.
                let result = match unsafe { error.as_ref() } {
                    None => Ok(()),
                    Some(error) => Err(described(error)),
                };
                let _ = sent.send(result);
            });
            // SAFETY: on the VM's queue; the handler is copied by the call.
            unsafe { vm.startWithCompletionHandler(&handler) };
        });
        match started.recv_timeout(limit) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(why)) => Err(VmError::Machine(format!("the VM didn't start: {why}"))),
            Err(_) => Err(VmError::Machine(format!(
                "the VM didn't start within {limit:?}"
            ))),
        }
    }

    /// A vsock connection to the guest's `port`, as a descriptor of this
    /// process's own (a copy: the framework's is closed).
    pub(crate) fn connect(&self, port: u32, limit: Duration) -> Result<OwnedFd, VmError> {
        let (sent, connected) = mpsc::channel::<Result<OwnedFd, String>>();
        let asked = self.on_queue(move |vm| {
            // SAFETY: on the VM's queue; its one socket device is a virtio
            // one (the configuration made it).
            let device = unsafe { vm.socketDevices() }
                .firstObject()
                .and_then(|device| device.downcast::<VZVirtioSocketDevice>().ok());
            let Some(device) = device else {
                return false;
            };
            let handler = RcBlock::new(
                move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                    // SAFETY: the framework passes null or valid objects.
                    let result = match unsafe { (connection.as_ref(), error.as_ref()) } {
                        (Some(connection), _) => {
                            // SAFETY: the connection's own descriptor, valid
                            // until it's closed just after: duplicated first.
                            let copy = unsafe { libc::dup(connection.fileDescriptor()) };
                            // SAFETY: on the VM's queue (handlers run there).
                            unsafe { connection.close() };
                            if copy < 0 {
                                Err(std::io::Error::last_os_error().to_string())
                            } else {
                                // SAFETY: `dup` just returned it, ours alone.
                                Ok(unsafe { OwnedFd::from_raw_fd(copy) })
                            }
                        }
                        (None, Some(error)) => Err(described(error)),
                        (None, None) => Err("no connection, and no error".to_owned()),
                    };
                    let _ = sent.send(result);
                },
            );
            // SAFETY: on the VM's queue; the handler is copied by the call.
            unsafe { device.connectToPort_completionHandler(port, &handler) };
            true
        });
        if asked != Some(true) {
            return Err(VmError::Machine("the VM has no vsock device".to_owned()));
        }
        match connected.recv_timeout(limit) {
            Ok(Ok(fd)) => Ok(fd),
            Ok(Err(why)) => Err(VmError::Machine(format!(
                "no connection to the guest's port {port}: {why}"
            ))),
            Err(_) => Err(VmError::Machine(format!(
                "no connection to the guest's port {port} within {limit:?}"
            ))),
        }
    }

    /// How the VM is.
    pub(crate) fn state(&self) -> State {
        // SAFETY: on the VM's queue, a property read.
        let Some(state) = self.on_queue(|vm| unsafe { vm.state() }) else {
            return State::Failed;
        };
        match state {
            VZVirtualMachineState::Starting
            | VZVirtualMachineState::Running
            | VZVirtualMachineState::Pausing
            | VZVirtualMachineState::Paused
            | VZVirtualMachineState::Resuming
            | VZVirtualMachineState::Saving
            | VZVirtualMachineState::Restoring => State::Up,
            VZVirtualMachineState::Error => State::Failed,
            _ => State::Down,
        }
    }

    /// Stops the VM at once (the guest isn't asked: what it hasn't synced
    /// is lost), waiting up to `limit`.
    pub(crate) fn force_stop(&self, limit: Duration) {
        let (sent, stopped) = mpsc::channel::<()>();
        self.on_queue(move |vm| {
            let handler = RcBlock::new(move |_error: *mut NSError| {
                let _ = sent.send(());
            });
            // SAFETY: on the VM's queue; the handler is copied by the call.
            unsafe { vm.stopWithCompletionHandler(&handler) };
        });
        let _ = stopped.recv_timeout(limit);
    }
}
