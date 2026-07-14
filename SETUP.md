## 1. install Rust

Open PowerShell and run:

    winget install Rustlang.Rustup

Run the installer it downloads and accept the defaults. If it offers to
install the Visual Studio C++ Build Tools, say yes (one-time download).

Then close PowerShell and open a new one, so the install takes effect.
Check it worked:

    cargo --version

## 2. extract and build

Put beltsim.zip in your Downloads folder, then right click, then click "Extract All" to decompress the .zip.

After that go to Windows Powershell and run:

    cd beltsim
    cargo build --release

The first build takes a minute or two.

Always build with --release. Without it you get a debug binary that is
10 to 50 times slower.

## 3. check it works

    cargo test --release

should end with: 30 passed. Then:

    .\target\release\beltsim.exe run examples\seal_flush.blt --seconds 40

You should see FLOW around 140 and PILLS exactly 1.

## 4. next

README.md explains the project, SYNTAX.md is the
language reference, examples\ and lib\ are circuits to copy from.

## if something breaks

- "cargo is not recognized": open a NEW PowerShell window after installing
  Rust. If it persists, Rust did not install.
- "Failed to open beltsim.zip": you are not in the folder that has the
  file. cd there first (step 2 assumes Downloads).
- error mentioning link.exe or MSVC: rerun the Rust installer and accept
  the C++ Build Tools this time.
- everything is slow: you built or ran without --release.
- 28 passed instead of 30, or files mentioned here are missing: you have
  an old version. Delete the beltsim folder, re-extract the newest zip,
  build again.
- a test fails or a run crashes: real bug, not your setup. You might be fried, idk.
