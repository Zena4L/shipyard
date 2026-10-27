// Step 13.1's probe (macOS production): what Virtualization.framework gives the guest for
// Rosetta, a memory balloon, TRIM and a pause, measured from the Mac's side. Built and run by
// run.sh, never by Shipyard. It boots the guest's kernel with its initramfs and the probe's
// appended (`rdinit=/probe-init`), shares DIR as virtiofs `probe`, and answers the guest's
// stage markers (DIR/stage-NAME) by measuring, then acking (DIR/ack-NAME). Its notes go to
// DIR/host.txt.
//
// usage: vz-probe KERNEL INITRD DISK DIR

import Foundation
import Virtualization

let arguments = CommandLine.arguments
guard arguments.count == 5 else {
    FileHandle.standardError.write("usage: vz-probe KERNEL INITRD DISK DIR\n".data(using: .utf8)!)
    exit(2)
}
let kernel = URL(fileURLWithPath: arguments[1])
let initrd = URL(fileURLWithPath: arguments[2])
let disk = URL(fileURLWithPath: arguments[3])
let dir = URL(fileURLWithPath: arguments[4])
let memory: UInt64 = 4 << 30

let hostLog = dir.appendingPathComponent("host.txt")
FileManager.default.createFile(atPath: hostLog.path, contents: nil)
let hostHandle = try FileHandle(forWritingTo: hostLog)
func note(_ line: String) {
    hostHandle.write((line + "\n").data(using: .utf8)!)
    print("host: " + line)
}

/// Runs a command, its output's first line.
func run(_ command: String) -> String {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/bin/sh")
    process.arguments = ["-c", command]
    let pipe = Pipe()
    process.standardOutput = pipe
    try? process.run()
    process.waitUntilExit()
    let text = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
    return text.trimmingCharacters(in: .whitespacesAndNewlines)
}

/// The VM's process (the newest Virtualization service) and what it holds: its resident
/// size and its footprint as macOS counts it.
func vmMemory() -> String {
    let pid = run("pgrep -n -f com.apple.Virtualization.VirtualMachine")
    if pid.isEmpty { return "no VM process" }
    let rss = run("ps -o rss= -p \(pid)")
    let footprint = run("footprint -p \(pid) 2>/dev/null | grep -i 'phys_footprint:' | head -1")
    return "pid \(pid) rss \(rss) KiB; \(footprint)"
}

func diskUsage() -> String {
    run("du -k '\(disk.path)' | cut -f1") + " KiB allocated"
}

let configuration = VZVirtualMachineConfiguration()
configuration.platform = VZGenericPlatformConfiguration()
let boot = VZLinuxBootLoader(kernelURL: kernel)
boot.initialRamdiskURL = initrd
boot.commandLine = "console=hvc0 rdinit=/probe-init " + (ProcessInfo.processInfo.environment["PROBE_CMDLINE"] ?? "")
configuration.bootLoader = boot
configuration.cpuCount = 4
configuration.memorySize = memory

let consolePath = dir.appendingPathComponent("console.log").path
FileManager.default.createFile(atPath: consolePath, contents: nil)
let console = VZVirtioConsoleDeviceSerialPortConfiguration()
console.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: nil, fileHandleForWriting: FileHandle(forWritingAtPath: consolePath))
configuration.serialPorts = [console]

configuration.storageDevices = [
    VZVirtioBlockDeviceConfiguration(
        attachment: try VZDiskImageStorageDeviceAttachment(url: disk, readOnly: false))
]

let probeShare = VZVirtioFileSystemDeviceConfiguration(tag: "probe")
probeShare.share = VZSingleDirectoryShare(directory: VZSharedDirectory(url: dir, readOnly: false))
var shares: [VZDirectorySharingDeviceConfiguration] = [probeShare]
switch VZLinuxRosettaDirectoryShare.availability {
case .installed:
    let rosetta = VZVirtioFileSystemDeviceConfiguration(tag: "rosetta")
    rosetta.share = try VZLinuxRosettaDirectoryShare()
    shares.append(rosetta)
    note("rosetta: installed, shared as tag rosetta")
case .notInstalled:
    note("rosetta: supported, not installed (VZLinuxRosettaDirectoryShare.installRosetta asks)")
case .notSupported:
    note("rosetta: not supported on this Mac")
@unknown default:
    note("rosetta: unknown availability")
}
configuration.directorySharingDevices = shares
configuration.memoryBalloonDevices = [VZVirtioTraditionalMemoryBalloonDeviceConfiguration()]
configuration.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
try configuration.validate()

final class Watcher: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        note("the guest stopped")
        exit(0)
    }
    func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
        note("the VM stopped with an error: \(error)")
        exit(1)
    }
}

let vm = VZVirtualMachine(configuration: configuration)
let watcher = Watcher()
vm.delegate = watcher

/// Runs `body` on the VM's queue (the main one) and waits for it.
func onVM(_ body: @escaping () -> Void) {
    DispatchQueue.main.sync(execute: body)
}

/// What the Mac does at each of the guest's stages.
func answer(_ stage: String) {
    switch stage {
    case let name where name.hasPrefix("disk-"):
        note("\(name): \(diskUsage())")
    case let name where name.hasPrefix("mem-") && name != "mem-balloon" && name != "mem-measured":
        note("\(name): \(vmMemory())")
    case "mem-balloon":
        let device = vm.memoryBalloonDevices.first as? VZVirtioTraditionalMemoryBalloonDevice
        guard let device else {
            note("mem-balloon: no balloon device")
            return
        }
        onVM { device.targetVirtualMachineMemorySize = 1 << 30 }
        note("mem-balloon: target set to 1 GiB: \(vmMemory())")
    case "mem-measured":
        note("mem-measured (the balloon at 1 GiB 15 s): \(vmMemory())")
        let device = vm.memoryBalloonDevices.first as? VZVirtioTraditionalMemoryBalloonDevice
        onVM { device?.targetVirtualMachineMemorySize = memory }
        Thread.sleep(forTimeInterval: 5)
        note("mem-measured: reset to 4 GiB, 5 s later: \(vmMemory())")
    case "clock":
        let paused = Date()
        let semaphore = DispatchSemaphore(value: 0)
        DispatchQueue.main.async {
            vm.pause { result in
                note("clock: paused (\(result))")
                semaphore.signal()
            }
        }
        semaphore.wait()
        Thread.sleep(forTimeInterval: 60)
        DispatchQueue.main.async {
            vm.resume { result in
                note("clock: resumed (\(result))")
                semaphore.signal()
            }
        }
        semaphore.wait()
        note(String(format: "clock: paused for %.1f s; the Mac's time at resume %.3f",
                    Date().timeIntervalSince(paused), Date().timeIntervalSince1970))
    default:
        note("\(stage): (nothing to measure)")
    }
}

// The stages, watched on a thread of their own.
Thread.detachNewThread {
    var answered = Set<String>()
    while true {
        let names = (try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []
        for name in names.sorted() where name.hasPrefix("stage-") {
            let stage = String(name.dropFirst("stage-".count))
            if answered.contains(stage) { continue }
            answered.insert(stage)
            answer(stage)
            FileManager.default.createFile(
                atPath: dir.appendingPathComponent("ack-" + stage).path, contents: nil)
        }
        Thread.sleep(forTimeInterval: 0.2)
    }
}

note("before boot: \(diskUsage())")
vm.start { result in
    switch result {
    case .success:
        note("the VM started")
    case .failure(let error):
        note("the VM didn't start: \(error)")
        exit(1)
    }
}
// A bound on the whole probe.
DispatchQueue.main.asyncAfter(deadline: .now() + 900) {
    note("the probe took past 15 minutes: stopped")
    exit(3)
}
RunLoop.main.run()
