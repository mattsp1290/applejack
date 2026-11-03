/*
 * steamwebhelper-shim.exe: written over the Steam client's
 * bin/cef/cef.win<arch>/steamwebhelper.exe, with the real helper stashed beside
 * it as steamwebhelper_real.exe. Runs the real helper with the original
 * command line tail and the flags SHIM_FLAGS names, waits, and exits with
 * the child's code.
 *
 * Why: CEF draws the login page in a GPU process into a window the browser
 * process owns, and DXMT refuses a swap chain on a window another process
 * owns (DXMT issue 141). `--in-process-gpu` keeps the GPU thread in the
 * browser process, which owns the window. Steam's launcher maps no `-cef-*`
 * flag to it, so the flag has to come from in front of the helper.
 *
 * A CEF child (`--type=renderer`, `--type=gpu-process`, ...) that comes
 * through here gets no extra flag: the browser process decides its
 * children's switches, and a GPU child must not be told to be in-process.
 */
#include <windows.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>

#ifndef SHIM_FLAGS
#define SHIM_FLAGS L"--in-process-gpu"
#endif

/* The exit code when the real helper could not be run at all; CEF's own
 * codes are small positive numbers, and Steam logs any non-zero exit. */
#define SHIM_FAILED 0x5348494D /* 'SHIM' */

static const wchar_t REAL_NAME[] = L"steamwebhelper_real.exe";

/* In the image (through fail's message) so the launcher can tell a shim of
 * any build from the client's helper (`SHIM_MARKER` in
 * crates/applejack-core) and never stashes an old shim as the real helper. */
#define MARKER "applejack steamwebhelper shim"

static int fail(void)
{
    OutputDebugStringA(MARKER ": could not run " "steamwebhelper_real.exe");
    return SHIM_FAILED;
}

/* The tail of a command line after its first token, which is the program
 * path, quoted or not; the tail keeps its leading blank. */
static const wchar_t *command_tail(const wchar_t *cmd)
{
    if (*cmd == L'"') {
        cmd++;
        while (*cmd && *cmd != L'"')
            cmd++;
        if (*cmd)
            cmd++;
    } else {
        while (*cmd && *cmd != L' ' && *cmd != L'\t')
            cmd++;
    }
    return cmd;
}

/* `--type=` as its own switch, after a blank, not inside a value such as
 * a cache directory. The tail keeps the blank before its first switch. */
static int is_cef_child(const wchar_t *tail)
{
    return wcsstr(tail, L" --type=") != NULL || wcsstr(tail, L"\t--type=") != NULL;
}

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE previous, PWSTR args, int show)
{
    (void)instance;
    (void)previous;
    (void)args;
    (void)show;

    wchar_t self[MAX_PATH];
    DWORD length = GetModuleFileNameW(NULL, self, MAX_PATH);
    if (length == 0 || length >= MAX_PATH)
        return fail();
    wchar_t *slash = wcsrchr(self, L'\\');
    if (slash == NULL)
        return fail();
    slash[1] = 0;

    size_t real_length = wcslen(self) + wcslen(REAL_NAME) + 1;
    wchar_t *real = calloc(real_length, sizeof *real);
    if (real == NULL)
        return fail();
    wcscpy(real, self);
    wcscat(real, REAL_NAME);

    const wchar_t *tail = command_tail(GetCommandLineW());
    const wchar_t *flags = is_cef_child(tail) ? L"" : L" " SHIM_FLAGS;
    size_t cmd_length = 1 + wcslen(real) + 1 + wcslen(tail) + wcslen(flags) + 1;
    wchar_t *cmd = calloc(cmd_length, sizeof *cmd);
    if (cmd == NULL)
        return fail();
    wcscpy(cmd, L"\"");
    wcscat(cmd, real);
    wcscat(cmd, L"\"");
    wcscat(cmd, tail);
    wcscat(cmd, flags);

    STARTUPINFOW startup;
    memset(&startup, 0, sizeof startup);
    startup.cb = sizeof startup;
    GetStartupInfoW(&startup);
    PROCESS_INFORMATION process;
    memset(&process, 0, sizeof process);
    /* Handles are inherited: the parent hands the helper its pipes. */
    if (!CreateProcessW(real, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &startup, &process))
        return fail();
    CloseHandle(process.hThread);
    WaitForSingleObject(process.hProcess, INFINITE);
    DWORD code = SHIM_FAILED;
    GetExitCodeProcess(process.hProcess, &code);
    CloseHandle(process.hProcess);
    return (int)code;
}
