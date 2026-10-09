# Building `hdpim_host.exe`

A 32-bit PE (HDPIM.dll is PE32). Built with mingw-w64:

```
i686-w64-mingw32-windres host.rc -O coff -o host_res.o
i686-w64-mingw32-gcc -O2 -o hdpim_host.exe hdpim_host.c host_res.o -lole32
```

`host.rc` embeds `host.manifest` (RT_MANIFEST) — the Win10/11 `supportedOS` GUID is
load-bearing (HDPIM gates on the OS version). The prebuilt `hdpim_host.exe` is committed
so `mudhut install --method download` works without a mingw toolchain; rebuild if the
source changes. See `../docs/HDPIM_OFFLINE_INSTALL_METHODOLOGY.md` ("The host program").
