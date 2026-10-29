/*
 * DX11 probe: creates a window, a D3D11 device and swap chain, clears the back
 * buffer to a non-black colour each frame, prints the adapter description, and
 * exits 0 after N frames (default 120, or --frames N). Any failure exits 1 with
 * the failing call and HRESULT on stderr.
 */
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int g_closed;

static LRESULT CALLBACK wnd_proc(HWND hwnd, UINT msg, WPARAM wparam, LPARAM lparam)
{
    switch (msg) {
    case WM_CLOSE:
    case WM_DESTROY:
        g_closed = 1;
        PostQuitMessage(0);
        return 0;
    default:
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
}

static int fail(const char *what, HRESULT hr)
{
    fprintf(stderr, "dx11-sample: %s failed: 0x%08lx\n", what, (unsigned long)hr);
    return 1;
}

static void print_adapter(IDXGISwapChain *swap_chain)
{
    IDXGIDevice *dxgi_device = NULL;
    IDXGIAdapter *adapter = NULL;
    DXGI_ADAPTER_DESC desc;
    ID3D11Device *device = NULL;
    char name[256];

    if (FAILED(IDXGISwapChain_GetDevice(swap_chain, &IID_ID3D11Device, (void **)&device)))
        return;
    if (SUCCEEDED(ID3D11Device_QueryInterface(device, &IID_IDXGIDevice, (void **)&dxgi_device))) {
        if (SUCCEEDED(IDXGIDevice_GetAdapter(dxgi_device, &adapter))) {
            if (SUCCEEDED(IDXGIAdapter_GetDesc(adapter, &desc))) {
                WideCharToMultiByte(CP_UTF8, 0, desc.Description, -1, name, sizeof(name), NULL, NULL);
                printf("adapter: %s (vendor 0x%04x, device 0x%04x, vram %llu MiB)\n", name,
                       (unsigned)desc.VendorId, (unsigned)desc.DeviceId,
                       (unsigned long long)(desc.DedicatedVideoMemory >> 20));
            }
            IDXGIAdapter_Release(adapter);
        }
        IDXGIDevice_Release(dxgi_device);
    }
    ID3D11Device_Release(device);
}

int main(int argc, char **argv)
{
    static const WCHAR class_name[] = L"applejack-dx11-sample";
    static const FLOAT clear_colour[4] = { 0.1f, 0.6f, 0.9f, 1.0f };
    static const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0 };
    int frames = 120;
    WNDCLASSW wc;
    HWND hwnd;
    DXGI_SWAP_CHAIN_DESC scd;
    ID3D11Device *device = NULL;
    ID3D11DeviceContext *context = NULL;
    IDXGISwapChain *swap_chain = NULL;
    ID3D11Texture2D *back_buffer = NULL;
    ID3D11RenderTargetView *rtv = NULL;
    D3D_FEATURE_LEVEL level;
    HRESULT hr;
    MSG msg;
    int i;

    for (i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--frames") == 0 && i + 1 < argc)
            frames = atoi(argv[++i]);
    }
    if (frames <= 0)
        frames = 1;

    memset(&wc, 0, sizeof(wc));
    wc.lpfnWndProc = wnd_proc;
    wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = class_name;
    wc.hCursor = LoadCursorW(NULL, (LPCWSTR)IDC_ARROW);
    if (!RegisterClassW(&wc))
        return fail("RegisterClassW", HRESULT_FROM_WIN32(GetLastError()));
    hwnd = CreateWindowExW(0, class_name, L"applejack dx11-sample", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                           CW_USEDEFAULT, CW_USEDEFAULT, 640, 480, NULL, NULL, wc.hInstance, NULL);
    if (!hwnd)
        return fail("CreateWindowExW", HRESULT_FROM_WIN32(GetLastError()));

    memset(&scd, 0, sizeof(scd));
    scd.BufferCount = 2;
    scd.BufferDesc.Width = 640;
    scd.BufferDesc.Height = 480;
    scd.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    scd.BufferDesc.RefreshRate.Numerator = 60;
    scd.BufferDesc.RefreshRate.Denominator = 1;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.OutputWindow = hwnd;
    scd.SampleDesc.Count = 1;
    scd.Windowed = TRUE;
    scd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;

    hr = D3D11CreateDeviceAndSwapChain(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, levels,
                                       sizeof(levels) / sizeof(levels[0]), D3D11_SDK_VERSION,
                                       &scd, &swap_chain, &device, &level, &context);
    if (FAILED(hr))
        return fail("D3D11CreateDeviceAndSwapChain", hr);
    printf("feature level: 0x%04x\n", (unsigned)level);
    print_adapter(swap_chain);

    hr = IDXGISwapChain_GetBuffer(swap_chain, 0, &IID_ID3D11Texture2D, (void **)&back_buffer);
    if (FAILED(hr))
        return fail("IDXGISwapChain::GetBuffer", hr);
    hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)back_buffer, NULL, &rtv);
    ID3D11Texture2D_Release(back_buffer);
    if (FAILED(hr))
        return fail("ID3D11Device::CreateRenderTargetView", hr);

    for (i = 0; i < frames && !g_closed; i++) {
        while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        ID3D11DeviceContext_OMSetRenderTargets(context, 1, &rtv, NULL);
        ID3D11DeviceContext_ClearRenderTargetView(context, rtv, clear_colour);
        hr = IDXGISwapChain_Present(swap_chain, 1, 0);
        if (FAILED(hr))
            return fail("IDXGISwapChain::Present", hr);
    }
    printf("frames: %d\n", i);
    fflush(stdout);

    ID3D11RenderTargetView_Release(rtv);
    ID3D11DeviceContext_Release(context);
    IDXGISwapChain_Release(swap_chain);
    ID3D11Device_Release(device);
    DestroyWindow(hwnd);
    return 0;
}
