# System Monitor

A MyWallpaper Canvas add-on backed by one supervised Windows `process-v2`
companion per release digest. It displays live CPU utilization, physical-memory usage,
and GPU engine utilization without injecting code into another process.

The companion uses only standard Windows APIs and receives only the validated
`device` settings shared by every instance. Layer presentation settings remain
inside Canvas. It changes its sampling interval without restarting, while
Canvas reports connection, reconnection, stale-data and
sampling failures directly in the widget. Missing or unknown refresh intervals
fail the companion session with a visible `settings-invalid` error instead of
silently changing the requested rate.

It implements MyWallpaper's
[surface-aware companion protocol v2](https://github.com/MyWallpapers/MyWallpaper/blob/main/docs/native-addons.md#companion-protocol-v2),
broadcasting each hardware sample to every visual instance of the release. Only
the canonical visual instance can send commands back to the companion.
That canonical instance also publishes the validated sample on
`mywallpaper.hardware/v1/metrics`, so other layers can reuse the telemetry
through the bounded scene bus without starting another sampler or duplicating
events on repeated displays.

## Development

Use Node.js 22 or newer and the pnpm version pinned by `packageManager`:

```powershell
pnpm install --frozen-lockfile
pnpm dev
```

For the complete native preview, run `mywallpaper dev` from this directory
with the released `@mywallpaper/cli`, enable Developer Mode in MyWallpaper
Desktop, then load the loopback URL shown by the CLI. Published GitHub releases
use MyWallpaper's reusable OIDC workflow to rebuild the x64 executable
from source; binaries and web `dist/` output are not committed. Publishing
creates a candidate identified by its SemVer and digest; promotion and
recommendation are separate, and installed releases never auto-update.

Run `mywallpaper generate` after a Canvas contract update and commit the
resulting `generated/mywallpaper-runtime.d.ts`. The official verifier
regenerates it and rejects missing or stale declarations.

## Native boundary

- CPU: `GetSystemTimes`
- physical memory: `GlobalMemoryStatusEx`
- GPU identity and dedicated memory: DXGI
- GPU engine utilization: Windows PDH `GPU Engine` counters

The companion keeps the LUID of the first non-software adapter selected by
DXGI, ignores PDH instances belonging to every other adapter, then aggregates
the selected adapter's per-process counters by physical engine and reports its
busiest engine. The displayed name, memory and utilization therefore always
describe the same GPU.
