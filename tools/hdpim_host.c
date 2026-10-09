/*
 * hdpim_host.c  -- Minimal 32-bit orchestrator that drives Adobe's own
 * HDPIM.dll -> hdpimInstallProduct to install a staged ESD package offline.
 *
 * This is orchestration only: it LoadLibrary's Adobe's shipped HDPIM.dll and
 * calls its exported install API exactly the way Adobe's Set-up.exe does.
 * No Adobe binary is modified; no decryption is reimplemented (HDPIM does it).
 *
 * Build (32-bit PE, HDPIM.dll is PE32), from tools/ -- see BUILD_hdpim_host.md:
 *   i686-w64-mingw32-windres host.rc -O coff -o host_res.o
 *   i686-w64-mingw32-gcc -O2 -o hdpim_host.exe hdpim_host.c host_res.o -lole32
 *
 * Usage:
 *   hdpim_host.exe <HDPIM.dll path> <Driver.xml path> [wait_seconds]
 *   CWD is set to the Driver.xml directory so EsdDirectory "./PHSP" resolves.
 *
 * Signatures of the HDPIM.dll exports used here:
 *   __cdecl void  logfn(int level, const char* module, const char* category,
 *                       const char* c, const char* d, const char* fmt, ...);
 *   __cdecl int   hdpimSetLoggerFnPtr(logfn);
 *   __cdecl int   hdpimCreateSession(char** outSessionId, logfn logger, void* a3);
 *                 // on success *outSessionId = strdup("{GUID}")
 *   __cdecl int   hdpimInstallProduct(const char* sessionId,
 *                       const char* driverInfoXml, void* a3);
 *                 // all three args required non-NULL; returns 0 fast (async)
 *   __cdecl int   hdpimTerminateSession(const char* sessionId);
 */

#include <windows.h>
#include <objbase.h>
#include <stdio.h>
#include <stdarg.h>
#include <string.h>

typedef void (__cdecl *logfn_t)(int, const wchar_t*, const wchar_t*, const wchar_t*,
                                const wchar_t*, const wchar_t*, ...);
typedef int  (__cdecl *setlogger_t)(logfn_t);
typedef int  (__cdecl *createsession_t)(wchar_t**, logfn_t, void*);
typedef int  (__cdecl *install_t)(const wchar_t*, const wchar_t*, void*);
typedef int  (__cdecl *terminate_t)(const wchar_t*);

static FILE* g_log;

static void logline(const char* fmt, ...) {
    va_list ap; va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fprintf(stderr, "\n"); fflush(stderr);
    if (g_log) { va_start(ap, fmt); vfprintf(g_log, fmt, ap); va_end(ap);
                 fprintf(g_log, "\n"); fflush(g_log); }
}

/* HDPIM's log callback. __cdecl + variadic; args are WIDE (UTF-16LE):
 * (level, L"HDPIM", L"HDPIMSessionManager", wstr, wstr, wfmt, [vararg...]). */
static void __cdecl my_logger(int level, const wchar_t* a, const wchar_t* b,
                              const wchar_t* c, const wchar_t* d,
                              const wchar_t* fmt, ...) {
    wchar_t wbuf[8192];
    va_list ap;
    va_start(ap, fmt);
    if (fmt) _vsnwprintf(wbuf, 8191, fmt, ap); else wbuf[0] = 0;
    wbuf[8191] = 0;
    va_end(ap);
    fprintf(stderr, "  HDPIM[%d] %ls|%ls :: %ls\n",
            level, a ? a : L"", b ? b : L"", wbuf);
    fflush(stderr);
    if (g_log) {
        fprintf(g_log, "  HDPIM[%d] %ls|%ls :: %ls\n",
                level, a ? a : L"", b ? b : L"", wbuf);
        fflush(g_log);
    }
}

/* arg3 of hdpimInstallProduct is a CALLBACK FUNCTION POINTER (HDPIM does
 * `ecx=cb; call cb`). It is invoked for progress/RunProgram notifications and
 * its return value gates continuation (>=0 = success/continue). Provide a
 * real no-op that returns success. __cdecl => plain ret; callers restore esp
 * from their ebp frame. */
static int __cdecl progress_cb(int a,int b,int c,int d,int e,int f,int g,int h) {
    (void)a;(void)b;(void)c;(void)d;(void)e;(void)f;(void)g;(void)h;
    return 0;
}

static char* read_file(const char* path, DWORD* outlen) {
    HANDLE h = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ, NULL,
                           OPEN_EXISTING, 0, NULL);
    if (h == INVALID_HANDLE_VALUE) return NULL;
    DWORD sz = GetFileSize(h, NULL);
    char* buf = (char*)malloc(sz + 1);
    DWORD rd = 0;
    ReadFile(h, buf, sz, &rd, NULL);
    buf[rd] = 0;
    if (outlen) *outlen = rd;
    CloseHandle(h);
    return buf;
}

/* Pump messages so any window/COM work on this thread progresses while we wait
 * for the async install worker. */
static void pump_for(int seconds) {
    DWORD start = GetTickCount();
    int last = -1;
    while ((int)((GetTickCount() - start) / 1000) < seconds) {
        MSG msg;
        while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageA(&msg);
        }
        MsgWaitForMultipleObjects(0, NULL, FALSE, 1000, QS_ALLINPUT);
        int el = (int)((GetTickCount() - start) / 1000);
        if (el != last && el % 10 == 0) {
            logline("[host] alive, elapsed %ds / %ds", el, seconds);
            last = el;
        }
    }
}

int main(int argc, char** argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <HDPIM.dll> <Driver.xml> [wait_seconds]\n", argv[0]);
        return 2;
    }
    const char* hdpim_path = argv[1];
    const char* driver_path = argv[2];
    int wait_secs = (argc >= 4) ? atoi(argv[3]) : 2400;

    g_log = fopen("Z:\\tmp\\hdpim_host.log", "w");

    CoInitializeEx(NULL, COINIT_APARTMENTTHREADED);

    /* Set CWD to the Driver.xml dir so relative EsdDirectory resolves. */
    {
        char dir[MAX_PATH]; strncpy(dir, driver_path, MAX_PATH - 1);
        dir[MAX_PATH - 1] = 0;
        char* p = strrchr(dir, '\\'); if (!p) p = strrchr(dir, '/');
        if (p) { *p = 0; SetCurrentDirectoryA(dir); }
        char cwd[MAX_PATH]; GetCurrentDirectoryA(MAX_PATH, cwd);
        logline("[host] CWD = %s", cwd);
    }

    /* Put the AdobeGCClient dir on the DLL search path for HDPIM's siblings. */
    {
        char dir[MAX_PATH]; strncpy(dir, hdpim_path, MAX_PATH - 1);
        dir[MAX_PATH - 1] = 0;
        char* p = strrchr(dir, '\\'); if (!p) p = strrchr(dir, '/');
        if (p) { *p = 0; SetDllDirectoryA(dir);
                 logline("[host] SetDllDirectory = %s", dir); }
    }

    HMODULE h = LoadLibraryExA(hdpim_path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!h) { logline("[host] LoadLibrary FAILED err=%lu", GetLastError()); return 3; }
    logline("[host] HDPIM loaded at %p", (void*)h);

    setlogger_t   pSetLogger = (setlogger_t)   GetProcAddress(h, "hdpimSetLoggerFnPtr");
    createsession_t pCreate  = (createsession_t)GetProcAddress(h, "hdpimCreateSession");
    install_t     pInstall   = (install_t)     GetProcAddress(h, "hdpimInstallProduct");
    terminate_t   pTerm      = (terminate_t)   GetProcAddress(h, "hdpimTerminateSession");
    logline("[host] exports: setlogger=%p create=%p install=%p term=%p",
            pSetLogger, pCreate, pInstall, pTerm);
    if (!pSetLogger || !pCreate || !pInstall || !pTerm) {
        logline("[host] missing export"); return 4;
    }

    int r = pSetLogger(my_logger);
    logline("[host] hdpimSetLoggerFnPtr -> %d", r);

    wchar_t* sid = NULL;
    r = pCreate(&sid, my_logger, NULL);
    logline("[host] hdpimCreateSession -> ret=%d  sid='%ls'", r, sid ? sid : L"(null)");
    if (!sid) { logline("[host] no session id; abort"); return 5; }

    DWORD xlen = 0;
    char* xml = read_file(driver_path, &xlen);
    if (!xml) { logline("[host] cannot read Driver.xml"); return 6; }
    /* HDPIM expects the DriverInfo XML as a WIDE (UTF-16) string. Convert. */
    int wneed = MultiByteToWideChar(CP_UTF8, 0, xml, -1, NULL, 0);
    wchar_t* wxml = (wchar_t*)malloc(wneed * sizeof(wchar_t));
    MultiByteToWideChar(CP_UTF8, 0, xml, -1, wxml, wneed);
    logline("[host] DriverInfo XML (%lu bytes narrow -> %d wchars)", xlen, wneed);

    /* arg3 = progress/notification CALLBACK function pointer (see above). */
    void* a3 = (void*)progress_cb;
    logline("[host] ptrs: sid=%p wxml=%p a3(cb)=%p", (void*)sid, (void*)wxml, a3);

    logline("[host] calling hdpimInstallProduct(wide) ...");
    r = pInstall(sid, wxml, a3);
    logline("[host] hdpimInstallProduct -> %d (async; now waiting)", r);

    pump_for(wait_secs);

    logline("[host] calling hdpimTerminateSession");
    r = pTerm(sid);
    logline("[host] hdpimTerminateSession -> %d", r);

    logline("[host] done");
    return 0;
}
