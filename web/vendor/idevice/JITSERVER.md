This is the crates.io source for idevice 0.1.68 (upstream commit
d32c8189c51c2789496b0768039419c3705498c3), used only by the web package.
The local changes keep native TCP out of the WASM build and route Apple TSS
requests through browser fetch. Remove this patch when upstream exposes a
browser-compatible TSS transport.

The OPACK regression fixture uses synthetic device identifiers.
