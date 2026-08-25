# Project Context: Custom x86_64 Operating System (Rust)

## What this project is
I am building an operating system from scratch, targeting the **x86_64** architecture,
written in **Rust**. This is a learning project and a real long-term build — not a toy
snippet. I'm starting from zero: no existing kernel code yet.

## Current approach
- Using the **`bootloader` crate** (rust-osdev) to handle BIOS/UEFI boot, protected mode,
  long mode switching, and paging, instead of writing a custom bootloader by hand for now.
- A custom bootloader may be written later once the kernel itself is functional — but that
  is NOT the current priority. Do not suggest rewriting the bootloader from scratch unless
  I explicitly ask for it.
- Toolchain: **Rust nightly**, with `rust-src` and `llvm-tools-preview` components, since
  the kernel is `#![no_std]` and needs `-Z build-std` to rebuild `core` for a custom target.
- Custom LLVM target spec: `x86_64-kernel.json` (freestanding, `os: none`, red zone
  disabled, SSE/MMX disabled with soft-float enabled, panic strategy = abort).
- Kernel crate is `#![no_std]` and `#![no_main]`, with a custom `_start` entry point and a
  manual panic handler.
- Build/run loop: `cargo bootimage` to produce a bootable disk image, tested in
  **QEMU** (`qemu-system-x86_64`).

## How I want help
- Assume freestanding/`no_std` constraints everywhere — no reaching for `std`, threads,
  heap allocation, or anything OS-provided unless we've explicitly built that
  infrastructure ourselves first.
- Explain *why*, not just *what*, when it comes to low-level x86 concepts (GDT, IDT,
  paging, interrupts, VGA buffer, etc.) — I'm learning the internals, not just copying code.
- Prefer incremental steps I can actually build and boot in QEMU to verify, rather than
  large blocks of untested code.
- Flag anything unsafe/undefined-behavior-prone clearly, since kernel code leans heavily
  on `unsafe` and raw pointers.
- When suggesting crates, prefer well-known `no_std`/OS-dev ecosystem crates
  (e.g. `x86_64`, `spin`, `lazy_static`, `pic8259`, `pc-keyboard`) over reinventing things,
  unless the point is specifically to learn by implementing it manually.

## Not yet done (don't assume these exist)
Interrupt handling, memory management/paging beyond what `bootloader` sets up, heap
allocation, multitasking/processes, filesystem, drivers beyond VGA text mode, userspace.
