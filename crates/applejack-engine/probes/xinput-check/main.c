/*
 * XInput probe: polls user index 0 once and prints its state as one JSON line.
 * Exits 0 whether or not a pad is connected; the JSON says which.
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <xinput.h>
#include <stdio.h>

int main(void)
{
    XINPUT_STATE state;
    DWORD result;

    memset(&state, 0, sizeof(state));
    result = XInputGetState(0, &state);
    if (result == ERROR_SUCCESS) {
        printf("{\"connected\":true,\"packet\":%lu,\"buttons\":%u,\"left_trigger\":%u,\"right_trigger\":%u,"
               "\"thumb_lx\":%d,\"thumb_ly\":%d,\"thumb_rx\":%d,\"thumb_ry\":%d}\n",
               (unsigned long)state.dwPacketNumber, (unsigned)state.Gamepad.wButtons,
               (unsigned)state.Gamepad.bLeftTrigger, (unsigned)state.Gamepad.bRightTrigger,
               (int)state.Gamepad.sThumbLX, (int)state.Gamepad.sThumbLY,
               (int)state.Gamepad.sThumbRX, (int)state.Gamepad.sThumbRY);
    } else {
        printf("{\"connected\":false,\"error\":%lu}\n", (unsigned long)result);
    }
    fflush(stdout);
    return 0;
}
