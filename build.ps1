# build.ps1 — Build and run the kernel in QEMU
#
# Usage:
#   .\build.ps1          # Build disk image + launch QEMU
#   .\build.ps1 build    # Build disk image only (no QEMU)
#   .\build.ps1 clean    # Clean all build artifacts
#
# Prerequisites:
#   rustup toolchain install nightly
#   rustup component add rust-src llvm-tools-preview
#   cargo install bootimage   # Not needed! We use bootloader 0.11 directly.
#   QEMU installed + in PATH  (https://www.qemu.org/download/#windows)

param(
    [string]$Command = "run"
)

$ErrorActionPreference = "Stop"

$KernelDir  = "$PSScriptRoot\kernel"
$Target     = "x86_64-kernel"
$KernelBin  = "$KernelDir\target\$Target\debug\kernel"
$DiskImage  = "$KernelBin.img"

function Build-Kernel {
    Write-Host "==> Building kernel..." -ForegroundColor Cyan
    Push-Location $KernelDir
    try {
        # The kernel's .cargo/config.toml handles --target and -Zbuild-std automatically.
        cargo build
        if ($LASTEXITCODE -ne 0) { throw "Kernel build failed" }
    } finally {
        Pop-Location
    }
    Write-Host "==> Kernel built: $KernelBin" -ForegroundColor Green
}

function Build-DiskImage {
    Write-Host "==> Creating disk image..." -ForegroundColor Cyan
    # Run the disk-image builder with --no-run so it only creates the image.
    cargo run --manifest-path "$PSScriptRoot\disk-image\Cargo.toml" -- $KernelBin --no-run
    if ($LASTEXITCODE -ne 0) { throw "Disk image creation failed" }
    Write-Host "==> Disk image: $DiskImage" -ForegroundColor Green
}

function Run-Qemu {
    Write-Host "==> Launching QEMU..." -ForegroundColor Cyan
    $qemuArgs = @(
        "-drive", "format=raw,file=$DiskImage",
        "-serial", "stdio",
        "-m", "128M",
        "-name", "os-dev kernel"
    )
    Write-Host "    qemu-system-x86_64 $qemuArgs"
    & qemu-system-x86_64 @qemuArgs
}

switch ($Command.ToLower()) {
    "build" {
        Build-Kernel
        Build-DiskImage
    }
    "run" {
        Build-Kernel
        Build-DiskImage
        Run-Qemu
    }
    "clean" {
        Write-Host "==> Cleaning..." -ForegroundColor Cyan
        cargo clean --manifest-path "$PSScriptRoot\kernel\Cargo.toml"
        cargo clean --manifest-path "$PSScriptRoot\disk-image\Cargo.toml"
        Write-Host "==> Clean done" -ForegroundColor Green
    }
    default {
        Write-Host "Unknown command: $Command. Use: build | run | clean" -ForegroundColor Red
        exit 1
    }
}
