// disk-image/src/main.rs
//
// Host-side disk image builder and QEMU launcher.
//
// This binary is invoked by Cargo's runner mechanism (see kernel/.cargo/config.toml).
// When you run `cargo run` in the kernel/ directory, Cargo:
//   1. Builds the kernel ELF binary for x86_64-kernel target
//   2. Passes its path to THIS binary as argv[1]
//
// We then:
//   1. Wrap the kernel ELF in a BIOS-bootable raw disk image
//   2. (Optionally) launch QEMU with that image

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // Cargo passes the path to the kernel binary as the first argument.
    let kernel_binary_path = {
        let path = std::env::args()
            .nth(1)
            .expect("usage: disk-image <path-to-kernel-binary>");
        PathBuf::from(path)
            .canonicalize()
            .expect("kernel binary path does not exist")
    };

    let disk_image = create_disk_image(&kernel_binary_path);

    // Check if --no-run was passed (useful for just building the image)
    let no_run = std::env::args().any(|a| a == "--no-run");
    if !no_run {
        run_qemu(&disk_image);
    } else {
        println!("Disk image created: {}", disk_image.display());
        println!("To boot manually: qemu-system-x86_64 -drive format=raw,file={} -serial stdio", disk_image.display());
    }
}

/// Use bootloader 0.11's BiosBoot to create a BIOS-bootable disk image.
///
/// WHY BIOS?
///   BIOS mode works out of the box in QEMU without any extra firmware files.
///   UEFI requires OVMF firmware which must be installed separately.
///   The bootloader 0.11 BIOS mode DOES provide a framebuffer (via VESA/VBE),
///   so we get pixel graphics without needing UEFI.
fn create_disk_image(kernel_binary_path: &Path) -> PathBuf {
    // Place the output image next to the kernel binary, with a .img extension.
    let disk_image_path = kernel_binary_path.with_extension("img");

    println!("Building BIOS disk image...");
    println!("  kernel: {}", kernel_binary_path.display());
    println!("  output: {}", disk_image_path.display());

    let mut builder = bootloader::BiosBoot::new(kernel_binary_path);

    builder
        .create_disk_image(&disk_image_path)
        .expect("failed to create BIOS disk image — make sure llvm-tools-preview is installed");

    println!("Disk image created successfully.");
    disk_image_path
}

/// Launch QEMU with the disk image.
fn run_qemu(disk_image: &Path) {
    let mut qemu = Command::new("qemu-system-x86_64");

    qemu
        // Primary drive: our raw disk image
        .arg("-drive")
        .arg(format!("format=raw,file={}", disk_image.display()))
        // Serial port → stdout so we can see debug output (if we add serial later)
        .arg("-serial")
        .arg("stdio")
        // 128 MiB of RAM — enough for our kernel with heap
        .arg("-m")
        .arg("128M")
        // Disable default graphical window title changes (cleaner output)
        .arg("-name")
        .arg("os-dev kernel");

    println!("Launching QEMU...");
    println!("  command: {:?}", qemu);

    let exit_status = qemu.status().expect(
        "failed to launch qemu-system-x86_64 — is QEMU installed and in PATH?"
    );

    if !exit_status.success() {
        eprintln!("QEMU exited with status: {}", exit_status);
        std::process::exit(1);
    }
}
