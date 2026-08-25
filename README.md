# Custom x86_64 Operating System in Rust

A 64-bit operating system kernel built from scratch in Rust, featuring a modular architecture, framebuffer text rendering, interrupt handling, memory management, heap allocation, and cooperative async multitasking.

## 🚀 Features

- **Freestanding Bare-Metal Kernel**: `#![no_std]`, `#![no_main]` targeting `x86_64-unknown-none`.
- **Custom LLVM Target**: `x86_64-kernel.json` with red-zone disabled for interrupt safety.
- **Pixel Framebuffer & Monospace Font**: Full text rendering engine with character rasterization using `noto-sans-mono-bitmap`, smooth hardware scrolling, and interrupt-safe `print!` / `println!` macros.
- **Global Descriptor Table (GDT)**: Segment configuration with Task State Segment (TSS) and dedicated Interrupt Stack Table (IST) for double-fault handling.
- **Interrupt Descriptor Table (IDT)**: Handlers for CPU exceptions (Breakpoint, Double Fault, Page Fault) and hardware IRQs.
- **Hardware Interrupt Controller (8259 PIC)**: Remapped PIC vectors with PIT timer tick tracking and PS/2 keyboard handling.
- **Physical Memory Management**: `BootInfoFrameAllocator` parsing BIOS/UEFI memory maps.
- **Virtual Memory Paging**: 4-level paging via `OffsetPageTable` and page mapping abstractions.
- **Kernel Heap Allocator**: Global bump allocator supporting dynamic structures (`Box`, `Vec`, `Arc`, `BTreeMap`).
- **Cooperative Multitasking & Async Executor**: Custom `async`/`await` task executor with `AtomicWaker` support and an interrupt-atomic CPU `hlt` idle loop.
- **Async Keyboard Subsystem**: Lock-free lockless interrupt-to-task scancode queue decoding US 104-key layouts.

---

## 📁 Project Structure

```
.
├── .cargo/
│   └── config.toml          # Workspace cargo configuration
├── disk-image/              # Host-side tool to create bootable BIOS disk image
│   ├── Cargo.toml
│   └── src/main.rs
├── kernel/                  # The core operating system kernel
│   ├── .cargo/config.toml
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # Kernel entry point & initialization flow
│       ├── allocator.rs     # Heap allocation
│       ├── framebuffer.rs   # Pixel framebuffer & font rendering
│       ├── gdt.rs           # GDT & TSS
│       ├── interrupts.rs    # IDT & 8259 PIC handlers
│       ├── memory.rs        # Paging & physical frame allocator
│       └── task/
│           ├── mod.rs       # Task abstractions
│           ├── executor.rs  # Async task executor
│           └── keyboard.rs  # Async keyboard stream
├── build.ps1                # PowerShell build and QEMU run script
├── rust-toolchain.toml      # Nightly channel with required components
├── x86_64-kernel.json       # Custom target specification
└── README.md
```

---

## 🛠️ Prerequisites

- **Rust Nightly**:
  ```bash
  rustup toolchain install nightly
  rustup component add rust-src llvm-tools-preview
  ```
- **QEMU**: `qemu-system-x86_64` installed and accessible in your `PATH`.

---

## ⚡ Building & Running

### Using PowerShell Script
```powershell
# Build kernel and launch in QEMU
.\build.ps1

# Build kernel and disk image only (no QEMU)
.\build.ps1 build

# Clean build artifacts
.\build.ps1 clean
```

### Manual Build
```powershell
cd kernel
cargo build
```
