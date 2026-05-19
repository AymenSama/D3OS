# Agents

## Cursor Cloud specific instructions

This is **D3OS**, a bare-metal operating system written in Rust targeting x86_64. It is not a web application — it builds a bootable disk image that runs in QEMU or on real hardware.

### Key commands

All build commands use `cargo-make`. See `README.md` for full details.

| Task | Command |
|---|---|
| Run tests | `cargo make --no-workspace test` |
| Build image only | `cargo make --no-workspace image` |
| Build + run in QEMU | `cargo make --no-workspace` |
| Clean | `cargo make --no-workspace clean` |
| Clippy (lint) | `cargo make --no-workspace clippy-members` |

### Running in headless / cloud environments

The default `cargo make --no-workspace` task launches QEMU with `-vga std`, which requires a display. In headless environments, build the image separately and run QEMU manually with `-display none`:

```bash
cargo make --no-workspace image
cargo make --no-workspace hdd
cargo make --no-workspace ovmf

qemu-system-x86_64 \
  -machine q35,nvdimm=on -m 512M,slots=2,maxmem=1G \
  -cpu Haswell,fsgsbase -bios RELEASEX64_OVMF.fd \
  -boot d -display none -serial stdio -rtc base=localtime \
  -device piix3-ide,id=ide -device ahci,id=ahci \
  -drive driver=raw,if=none,id=boot,file.filename=d3os.img \
  -drive driver=raw,if=none,id=hdd,file.filename=hdd.img \
  -device ide-hd,bus=ahci.0,drive=boot \
  -device ide-hd,bus=ide.0,drive=hdd \
  -device nvdimm,memdev=mem1,id=nv1,label-size=2M \
  -object memory-backend-file,id=mem1,share=on,mem-path=nvdimm0,size=16M \
  -nic model=rtl8139,id=rtl8139 \
  -audiodev id=audio0,driver=none
```

Use `timeout 30 qemu-system-x86_64 ...` to auto-terminate after verifying boot.

### Gotchas

- **`fdisk` must be installed** for the HDD image build task (`cargo make --no-workspace hdd`). It is not listed in the README but is required. Install with `sudo apt-get install -y fdisk`.
- The Rust toolchain (`nightly-2025-10-20` with `rust-src`) is auto-installed by `rustup` from `rust-toolchain.toml` on first compile.
- `towbootctl` and `RELEASEX64_OVMF.fd` are auto-downloaded during the build process (by `wget`).
- Unit tests only cover the host-testable library crates (`libc`, `syntax`, `text_buffer`). Kernel and application crates are `no_std` and cannot be tested on the host.
- QEMU audio driver: use `-audiodev id=audio0,driver=none` in headless environments (the Makefile defaults to PulseAudio).
