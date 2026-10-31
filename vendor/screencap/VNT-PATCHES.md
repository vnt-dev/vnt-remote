# Local Windows capture patches

The bundled source is screencap 0.1.1, upstream commit `254fcffaf3229c80f1a50cfa82f4ec0ddf9d9fe4`; its original license is retained.

WGC retains staging textures and reuses the last captured frame after a bounded 250 ms wait, so an idle desktop can keep streaming and stop promptly. DXGI matches monitor handles to adapter outputs and also retains idle frames.

WGC pins the already loaded system `GraphicsCapture.dll` for the process lifetime before starting capture. On this Windows build a real capture teardown produced `GraphicsCapture.dll_unloaded` / `0xc0000005`, matching the [reported native teardown race](https://github.com/robmikh/Win32CaptureSample/issues/99). This is a targeted mitigation for callbacks still winding down when WinRT releases the DLL. It retains one system module per process, releases all sessions, pools, textures and per-thread WinRT references normally, and adds no arbitrary shutdown sleep. [GetModuleHandleExW PIN semantics](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-getmodulehandleexw) make the module lifetime explicit. The repeated real-desktop teardown regression exercises this path.
