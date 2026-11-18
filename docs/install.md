# One command installs this, on Windows too

PowerShell 5.1 runs on every Windows 10 and 11 machine, so the second installer is written for
it rather than for the newer shell, which many machines do not have.

    irm https://raw.githubusercontent.com/zencli/gesta/main/install.ps1 | iex

## What it does

Tries a prebuilt binary for this machine, and falls back to `cargo install` when there is none
or when Rust is already present. The binary goes to `%LOCALAPPDATA%\gesta\bin` and that
directory is added to the user `PATH`, which needs no administrator rights.

## The decisions worth stating

**Prebuilt first.** A machine with no Rust can still install. The installer asks for a
toolchain only when no binary matches the platform.

**`--locked` then unlocked.** The first attempt uses the committed `Cargo.lock`, so a build
gets the dependency versions the project was tested with. The second attempt without it covers
a lock file that has fallen behind a newer compiler.

**The user `PATH`, not the machine one.** Changing the machine `PATH` needs elevation, and an
install that asks for it is an install people abandon.

**Nothing is left behind on failure.** A failed download is removed before the build starts, so
a later run does not pick up half a file.

## What it does not do

No tag, no release asset, no signature. A prebuilt binary appears once a release publishes one;
until then every install builds from source, which needs Rust.

The script refuses an unknown platform with the command to build by hand rather than failing
with a 404 halfway through.
