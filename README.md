# Mud Hut

**The easy way to install Adobe apps into [Neutron](https://github.com/) prefixes.**

Mud Hut is a small, non-interactive CLI that automates deploying the Adobe
Creative Suite onto Linux. It sets up the sandbox, ingests the vendor binaries,
provisions the prefix to be Neutron-ready, and installs launchers — so the
experience is *"run it and it just works."*

It is an **orchestrator only**: it never translates applications (that is
Neutron's job) and it **never ships or patches Adobe binaries**. You bring your
own licensed install; Mud Hut wires it up.

```
Mud Hut  (install + provision)
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
mudhut install <app> --method windows --source <C:> --prefix <dir> [--dry-run]
mudhut install --suite --method windows --source <C:> --prefix <dir>
```

## Ingestion methods

| Method | Status | Description |
| --- | --- | --- |
| `windows` | **P1 — implemented** | Copy from an existing Windows install (drive_c / mounted C: / copied tree). |
| `iso` | P2 — planned | Extract from an Adobe offline package / ISO. |
| `download` | P3 — planned | Download from Adobe's own servers (product feed → `Driver.xml` → chunked). |

## Building

```sh
cargo build --release   # target/release/mudhut
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Mud Hut contains no
Adobe code or assets.
