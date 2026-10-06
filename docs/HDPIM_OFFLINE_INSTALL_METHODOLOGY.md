# How Mud Hut installs Adobe apps

Mud Hut installs a genuine, unmodified Adobe app into a Wine prefix without a Windows machine and without
the Creative Cloud desktop app. It never ships, modifies or patches Adobe binaries, and never bypasses
licensing: Adobe's own installer library does the installing, and you sign in to your own Adobe account
inside the app on first launch.

## The steps

1. **Resolve the app.** Mud Hut reads Adobe's public product feed to find the app's current build and the
   shared components it depends on.
2. **Download.** Every package comes from Adobe's CDN. Each 2 MiB segment is checked against the hashes
   Adobe publishes for it.
3. **Prepare the prefix.** Windows version information, then the Visual C++ runtimes, UCRT, GDI+ and
   core fonts (`neutron prefix provision --microsoft-only`, which downloads them from Microsoft and
   checks them against pinned checksums).
4. **Seed Adobe's installer runtime.** Mud Hut extracts Adobe's Desktop Common components, which include
   the installer library `HDPIM.dll`, from Adobe's public Creative Cloud package (ACCCx), using
   `tools/extract_accc_runtime.py`.
5. **Install.** A small 32-bit host program, `tools/hdpim_host.c` (source in this repo), loads Adobe's
   `HDPIM.dll` and calls its exported install function with a standard `Driver.xml` describing the
   product. Adobe's library verifies, decrypts and installs the payloads itself, exactly as it does when
   the Creative Cloud app drives it.
6. **Provision.** `neutron prefix provision` makes the prefix Neutron-ready and writes the menu entries.

The offline method (`--method offline`) skips steps 1 and 2 and reads the same packages from a folder or
disc image you already have.

## The host program

`tools/hdpim_host.c` loads `HDPIM.dll` from the HDBox folder and calls, in order:

- `hdpimSetLoggerFnPtr` — install a logger so Adobe's progress lands in Mud Hut's output
- `hdpimCreateSession`
- `hdpimInstallProduct(session, driverXml, progressCallback)` — runs asynchronously; the host waits
  for it to finish
- `hdpimTerminateSession`

The host carries a Windows 10/11 `supportedOS` manifest, because Adobe's installer checks the OS
version.

Build (mingw-w64), from `tools/` — see [`tools/BUILD_hdpim_host.md`](../tools/BUILD_hdpim_host.md):

```bash
cd tools
i686-w64-mingw32-windres host.rc -O coff -o host_res.o
i686-w64-mingw32-gcc -O2 -o hdpim_host.exe hdpim_host.c host_res.o -lole32
```
