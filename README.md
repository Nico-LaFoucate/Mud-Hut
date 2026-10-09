# Mud Hut

**The easy way to install Adobe apps into [Neutron](https://github.com/Nico-LaFoucate/neutron) prefixes.**

Mud Hut is a small, non-interactive CLI that automates deploying the Adobe
Creative Suite onto Linux. It acquires the app (download it straight from Adobe,
install from an offline package or `.iso`, or copy an existing Windows install), sets
up the prefix, and provisions it to be Neutron-ready (provision also writes the menu
launchers, one per app per prefix) — so the experience is *"run it and it just works."*

It is an **orchestrator only**: it never translates applications (that is
Neutron's job), and it **never ships, modifies or patches Adobe binaries, and never
bypasses licensing**. It drives Adobe's own installer library, and you sign in to your
own Adobe account inside the app.

```
Collider (GUI)    Mud Hut (acquire + install)    terminal
       │                       │                     │
       └───────────────────────┼─────────────────────┘
                               ▼
                neutron CLI (provision, launch)
                               │
                neutron-wine (the Wine runtime)
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

mudhut download <app> [--dest DIR] [--core-only]   # fetch+verify the app's own packages (not an offline package)
mudhut accc                                         # fetch+verify Adobe's Creative Cloud package (ACCCx) now
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

The installed app is genuine and unmodified. **You sign in inside the Adobe app, the same as
on Windows** (the app validates against your account via Adobe NGL); Mud Hut never touches your
Adobe account or login. See
[`docs/HDPIM_OFFLINE_INSTALL_METHODOLOGY.md`](docs/HDPIM_OFFLINE_INSTALL_METHODOLOGY.md) for how it works.

### How `--method offline` works

For installing with **no network access**, from a package already on disk:

```sh
mudhut install photoshop --method offline --source ~/mudhut-pkgs/PHSP-27.8-win64 --prefix ~/ps
```

`--source` is a directory in Adobe's ESD products layout — `<SAP>/Application.json`
plus the payload zips, for the product and each dependency. That is what
`mudhut install <app> --method download --keep-download` leaves in
`~/.cache/mudhut/<SAP>-<version>-<platform>/products`, and what a Set-up.exe offline
bundle carries in its `products/` dir (point `--source` at either the products dir or
its parent). `mudhut download --dest` is not enough: it fetches only the app's own
packages, without the `Application.json` manifests or the shared components.
Everything is resolved from the package: no feed, no CDN, no sign-in.

> #### Installing from an ISO
>
> Point `--source` at the `.iso` file itself — that is all:
>
> ```sh
> mudhut install photoshop --method offline --source ~/Downloads/photoshop.iso --prefix ~/ps
> ```
>
> Mud Hut mounts the image read-only with `udisksctl` (udisks2; no root needed), installs
> straight from it and unmounts it afterwards. An ISO you mounted yourself works too: point
> `--source` at the mount.
>
> The source does **not** need to be writable and nothing is copied. Mud Hut detects
> read-only media and writes its driver XML to a scratch dir, naming the payload
> dirs by absolute path. ✅ Verified against real HDPIM installs (2026-09-03), both
> auto-detected and forced via `MUDHUT_ESD_ABSOLUTE=1`.

Requires [Neutron](https://github.com/Nico-LaFoucate/neutron) and its runtime
(`neutron setup` installs both). The install engine finds Wine via the Neutron runtime
or `$MUDHUT_WINE`.

## Installing

`neutron setup` installs Mud Hut for you. To build and install it from source instead:

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

## What Mud Hut sends to Adobe

Mud Hut talks to Adobe's public endpoints anonymously: no account, no cookies, no tokens.
Adobe's product feed requires two client identifiers, which Mud Hut sends:
`X-Api-Key: CC_HD_ESD_1_0` and `X-Adobe-App-Id: accc-hdcore-desktop`. They live in
[`ledger/ledger.json`](ledger/ledger.json).

Everywhere Adobe accepts it, Mud Hut identifies itself honestly as `MudHut/<version>`. The one
exception is Adobe's download CDN: it only serves app packages to Adobe's own installer, so
package downloads use the User-Agent `Adobe Application Manager 2.0`.

## Status

Beta, like the rest of Neutron. See [Neutron's README](https://github.com/Nico-LaFoucate/neutron)
for what has been tested.

## Reporting bugs

Report problems with installing an app on this repository's
[Issues](https://github.com/Nico-LaFoucate/Mud-Hut/issues/new/choose). If an Adobe app misbehaves
while running, report it on
[Neutron's Issues](https://github.com/Nico-LaFoucate/Neutron/issues/new/choose) instead: that is
where launching and running the apps are handled. Questions go to
[Discussions](https://github.com/Nico-LaFoucate/Neutron/discussions). Report security problems
privately: see [`SECURITY.md`](SECURITY.md). To contribute, see [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

Mud Hut is licensed under the **Apache License, Version 2.0** (`Apache-2.0`). See
[`LICENSE`](LICENSE) for the full text. Mud Hut contains no Adobe code or assets.

## Disclaimer

Neutron is an independent project by Nico LaFoucate and Ficus Media Group. Adobe and its product
names are trademarks of Adobe Inc. Neutron is not affiliated with or endorsed by Adobe.
