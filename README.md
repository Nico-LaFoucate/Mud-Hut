# Mud Hut

**The easy way to install Adobe apps into [Neutron](https://github.com/Nico-LaFoucate/neutron) prefixes.**

Mud Hut is a small, non-interactive CLI that automates deploying the Adobe
Creative Suite onto Linux. It acquires the app (download it straight from Adobe,
or copy an existing Windows install), sets up the prefix, and provisions it to be
Neutron-ready (provision also writes the menu launchers, one per app per prefix) —
so the experience is *"run it and it just works."*

It is an **orchestrator only**: it never translates applications (that is
Neutron's job) and it **never ships, patches, or modifies Adobe binaries
or DRM**. It uses Adobe's own signed installer components and the user's own
license.

```
Mud Hut  (acquire + install + provision)
   └── Neutron  (Wine-based runtime)
          └── Collider  (GUI)
```

Collider drives Mud Hut exactly as it drives Neutron: `mudhut --json <cmd>`
streams newline-delimited JSON events, the last of which is the terminal
`result` (or `error`).

## Commands

```sh
mudhut doctor                     # check host readiness (drivers, 32-bit libs, Neutron, disk)
mudhut apps [--source DIR]        # list installable apps; with --source, what's present there

# Install genuine from Adobe (downloads product + dependencies, decrypts, installs):
mudhut install <app> --method download --prefix <dir> [--dry-run]

# Install by copying an existing Windows install:
mudhut install <app>  --method windows  --source <C:> --prefix <dir> [--dry-run]
mudhut install --suite --method windows --source <C:> --prefix <dir>

mudhut download <app> [--dest DIR] [--core-only]   # just fetch+verify packages
mudhut auth begin | poll                            # one-time Adobe sign-in (device/QR)
mudhut ledger                                       # show the resolved Adobe endpoints/apps
```

## Ingestion methods

| Method | Status | Description |
| --- | --- | --- |
| `download` | **implemented** | Install genuine from Adobe's own servers. Resolves the product + its shared dependencies from the product feed, downloads + verifies them, then drives Adobe's own `HDPIM.dll` to decrypt and install (no Set-up.exe, no Creative Cloud desktop). |
| `windows` | **implemented** | Copy from an existing Windows install (drive_c / mounted C: / copied tree). |
| `offline` | **implemented** | Install from a package you already have on disk, with no network at all. Same HDPIM decrypt engine as `download`; manifests are read from the package instead of Adobe's feed. |

### How `--method download` works

`mudhut install photoshop --method download --prefix ~/ps` will:

1. Resolve the latest build + its dependency components from Adobe's product feed.
2. Download every package (product + deps) and verify each 2 MiB segment
   (SHA-256 for the product payloads, MD5 for the small shared deps).
3. Prepare the prefix (Win11 spoof, VC++ runtimes) and seed Adobe's Desktop-Common
   runtime from the public ACCCx components.
4. Drive Adobe's shipped `HDPIM.dll` (`hdpimInstallProduct`) to decrypt the payloads
   and lay down a genuine, unmodified install.
5. `neutron prefix provision` the prefix — which also writes the menu launcher, icon and
   file associations (`neutron-<app>-<prefix>.desktop`; Mud Hut writes none of its own).

The installed app is genuine and unmodified; **licensing is a separate one-time
Adobe sign-in** (the app validates against your account via Adobe NGL). See
`docs/HDPIM_OFFLINE_INSTALL_METHODOLOGY.md` for the full method.

### How `--method offline` works

For installing with **no network access**, from a package already on disk:

```sh
mudhut install photoshop --method offline --source ~/mudhut-pkgs/PHSP-27.8-win64 --prefix ~/ps
```

`--source` is a directory in Adobe's ESD products layout — `<SAP>/Application.json`
plus the payload zips, for the product and each dependency. That is exactly what
`mudhut download <app> --dest <dir>` stages, and what a Set-up.exe offline bundle
carries in its `products/` dir (point `--source` at either the products dir or its
parent). Everything is resolved from the package: no feed, no CDN, no sign-in.

> #### Installing from an ISO
>
> Mount it and point `--source` at the mount — that is all:
>
> ```sh
> udisksctl loop-setup -r -f ~/Downloads/photoshop.iso     # or mount it in your file manager
> mudhut install photoshop --method offline --source /run/media/$USER/<label>/products --prefix ~/ps
> ```
>
> The source does **not** need to be writable and nothing is copied. Mud Hut detects
> read-only media and writes its driver XML to a scratch dir, naming the payload
> dirs by absolute path. ✅ Verified against real HDPIM installs (2026-09-03), both
> auto-detected and forced via `MUDHUT_ESD_ABSOLUTE=1`.
>
> Passing the `.iso` file itself is not supported — mount it first. (Auto-mounting
> would only save you the one command, at the cost of a udisks/fuse dependency.)

Requires the [Neutron](https://github.com/Nico-LaFoucate/neutron) runtime on `PATH`
(`neutron runtime install`). The install engine finds Wine via the Neutron runtime
or `$MUDHUT_WINE`.

## Installing

```sh
./install.sh            # build --release + install + put `mudhut` on PATH
mudhut doctor           # verify the install (wine, tools, host readiness)
```

`install.sh` installs the release binary to `~/.local/share/mudhut/` with its
runtime tools (`tools/hdpim_host.exe`, `tools/extract_accc_runtime.py`)
co-located next to it — the layout the binary resolves first — and symlinks
`~/.local/bin/mudhut` to it. Override the locations with `MUDHUT_INSTALL_DIR` /
`MUDHUT_BIN_DIR`; remove everything with `./install.sh --uninstall`.

Don't point PATH at `target/debug/mudhut` — that only reflects the source after
a manual `cargo build`, so it silently goes stale. `mudhut doctor` confirms the
installed binary can see its tools and a usable wine.

## Developing

```sh
cargo build             # target/debug/mudhut (dev tree; finds ../../tools itself)
cargo test              # unit tests
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Mud Hut contains no
Adobe code or assets.
