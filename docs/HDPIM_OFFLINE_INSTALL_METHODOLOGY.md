# Adobe Offline-Install Methodology — driving HDPIM.dll directly (2026-07-08 breakthrough)

**Summary:** Install a genuine Adobe app **offline, fully decrypted — no Windows box, no Creative Cloud
desktop UI, no `Set-up.exe`** — by writing a tiny 32-bit host that `LoadLibrary`s Adobe's own shipped
`HDPIM.dll` and calls its exported `hdpimInstallProduct` directly. HDPIM builds `Media_db.db` and decrypts
the encrypted payloads itself. Orchestration only: no Adobe binary modified, no DRM reimplemented, no Adobe
binary shipped. Verified: genuine PS 27.8 `Photoshop.exe` (269,727,216 B, PE32+ x86-64), 5.3 GB / 8777 files.

## 1. Host mechanism (`~/mudhut-hdpim-host/hdpim_host.c`, PE32 / 32-bit)
`LoadLibraryExA(HDPIM.dll, LOAD_WITH_ALTERED_SEARCH_PATH)` then, all `__cdecl`, all strings UTF-16:
- `hdpimSetLoggerFnPtr(logfn)` — variadic wide logger `(level, module, cat, _, _, fmt, ...)`.
- `hdpimCreateSession(wchar_t** outSid, logfn, NULL)` → `*outSid = L"{GUID}"`, ret 0.
- `hdpimInstallProduct(sid, driverInfoXmlWide, progressCb)` — **all 3 args non-NULL**; returns 0 immediately (async on worker thread).
- `hdpimTerminateSession(sid)` at the end.
- **arg3 is a CALLBACK FUNCTION POINTER** (HDPIM does `call cb`), return ≥0 = continue. Pass a real no-op `int __cdecl cb(...){return 0;}` (a data buffer here → execute-fault, EIP == buffer).
- Host: `CoInitializeEx(APARTMENTTHREADED)`, **CWD = Driver.xml dir** (so `EsdDirectory ./PHSP` resolves), `SetDllDirectory(HDBox dir)` (sibling ESD DLLs HDZIP/HDNative/HDIM/HUM), read XML → `MultiByteToWideChar(CP_UTF8)`, then pump messages until the async install finishes (judge by log + on-disk PE, not the return code).
- Build: `i686-w64-mingw32-windres host.rc -O coff -o host_res.o && i686-w64-mingw32-gcc -O2 -o hdpim_host.exe hdpim_host.c host_res.o -lole32` (`host.rc` = `1 24 "host.manifest"`).
- Other HDPIM exports: hdpimGetProductInstallStatus(10), …InstalledVersions(11), …LaunchPath(12), …InstallProduct(14), …InstallUpdate(15), …TerminateSession(20), …UnInstallProduct(21).

## 2. DriverInfo XML (`~/mudhut-pkgs/PHSP-27.8-win64/products/Driver_core.xml`)
`<DriverInfo><ProductInfo>` PHSP / CodexVersion 27.8 / BaseVersion 27.0 / Platform win64 /
`EsdDirectory ./PHSP` / `IsNonCCProduct false` / `IsNglEnabled true` / `SupportedLanguages en_US` +
`<Dependencies>` (8: COCM CORG CORE COPS UXPW UAM SEPS COMP) `</ProductInfo>`
`<RequestInfo><InstallDir>C:\Program Files\Adobe</InstallDir><InstallLanguage>en_US</InstallLanguage>`.
Trimmed from the full 11-dep `Driver.xml` by dropping **COSY, ACR, CCXP** (see §5). `EsdDirectory` is
relative to the Driver.xml dir. ⚠️ Rust `src/driver.rs` does NOT yet emit BaseVersion/IsNglEnabled/
IsNonCCProduct/SupportedLanguages/absolute InstallDir — productionize it to this shape.

## 3. WAM prevention = don't run Set-up.exe
WAM ("online, ignores local package, wants %TEMP%\{GUID}\Driver.xml") is a **mode of Set-up.exe**. Modern
HDBox `Set-up.exe` 5.9.0.372 is a signed WAM build; its mode can't be flag-flipped (`--edtWorkFlow`,
`--caller=CC_HD_ESD_5_2`, `--driverXML` all still WAM) without breaking Authenticode (= forbidden). The
root ACC `Set-up.exe` (ACCCx) has zero ESD strings (pure online). **Driving HDPIM directly has no mode
selection, no online fetch, no `Media_db.db` FATAL, no HttpCommunicator winhttp quirk** — WAM is
structurally eliminated. Older 4.7.0.400 Set-up.exe reached ESD mode but is too old for PS 27.8 + expired cert.

## 4. Prefix prerequisites (mandatory)
- **Win11 24H2 spoof** `HKLM\Software\Microsoft\Windows NT\CurrentVersion`: CurrentBuild/CurrentBuildNumber=26100, CurrentMajorVersionNumber=10, CurrentVersion=10.0, DisplayVersion=24H2, ProductName="Windows 11 Pro". **DELETE `HKCU\Software\Wine\Version`** (else Wine forces build 22000).
- **supportedOS manifest** on the host EXE (Win10/11 GUID `{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}` is load-bearing; unmanifested GetVersionEx caps at 6.2).
- **HDBox HDPIM.dll** `C:\Program Files (x86)\Common Files\Adobe\Adobe Desktop Common\HDBox\HDPIM.dll` — NOT `…\AdobeGCClient\HDPIM.dll` (2018, jsoncpp `LargestUInt out of UInt range` overflow on 5.6 GB ExtractSize).
- Runtime source (HDBox + ESD DLLs + ADS/IPCBox/CEF): from the **public ACCCx zip** or a CC-desktop install — extractable, no install/WAM needed for the DLLs themselves.

## 5. Download side (public, auth-free — sign-in only gates *running*, not downloading)
Rust `src/{catalog,feed,download,driver}.rs` (ureq/rustls, roxmltree, sha2). Catalog:
`prod-rel-ffc-ccm.oobesaas.adobe.com/adobe-ffc-external/core/v6/products/all?platform=win32,win64&productType=Desktop&_type=xml&channel=ccm,sti,ccd` (channel mandatory). Manifest v3 keyed by header
`x-adobe-build-guid`. Required headers (else CDN 403): `X-Adobe-App-Id: accc-hdcore-desktop`,
`User-Agent: Creative Cloud`, `X-Api-Key: CCHomeWeb1.0`. CDN `https://ccmdls.adobe.com` + package Path.
Deps may be win32; Conditions gate on `[OSProcessorFamily]==64-bit`/`[OSVersion]`/`[installLanguage]`.
`packageHashKey` is NOT plain sha256 — verify by size+valid-zip; HD re-validates at install via ValidationURL.
Staged: `~/mudhut-pkgs/PHSP-27.8-win64/products/` (9.7 GiB, 41 pkgs). **Trimmed:** CCXP (needs macOS
`CCXProcess-LaunchAgent.zip`, "not present in ESD Mode" 182), ACR (missing delta zips) — download those to
re-add; COSY only failed on the arg3 bug → re-addable now.

## 6. The six walls (chronological)
1. Wrong DLL → jsoncpp overflow → use HDBox HDPIM. 2. Narrow XML → parse err 103 → UTF-16. 3. OS gate #1 → Win11 registry spoof. 4. OS gate #2 → supportedOS manifest. 5. arg3 execute-fault → callback pointer, no-op ret 0. 6. Incomplete deps "not present in ESD Mode" 182 → trim CCXP/ACR/COSY.

## 7. Reproduce
Prefix prep (Win11 spoof + delete HKCU Wine Version) → build host → `~/mudhut-hdpim-host/run.sh [wait]`
(sets HDPIM + `Driver_core.xml` paths, teardown + `wine hdpim_host.exe <HDPIM> <Driver> <wait>`). Success:
`Exiting hdpimInstallProduct with status '0'` + `file …/Adobe Photoshop 2026/Photoshop.exe` == PE32+ x86-64.

## 8. Productionization gap (this doc → the CLI)
Fold into `mudhut install <app> --method download`: ship/build `hdpim_host.exe`; upgrade `driver.rs` to the
Driver_core shape; set prefix prereqs programmatically (Win11 spoof, delete HKCU Wine Version, HDBox HDPIM);
seed the ACCC runtime (extract public zip); wire download→install one flow; re-add CCXP/ACR/COSY; one-time
NGL sign-in via the CC-desktop CEF login (renders). ⚠️ OPEN: proven only on an already-signed-in prefix —
fresh/unauthed-prefix E2E is the #1 de-risk.
