# openusd-rs — `.usd` layer extension rejected inside USDZ

## Symptom

`Stage::open("assets/external/Kitchen_set.usdz")` fails:

```
failed to read first layer from USDZ archive:
Unsupported file format for 'Kitchen_set.usd'. Expected .usda or .usdc extension
```

So the stage never loads and the viewer shows an empty scene.

## What works vs what doesn't (measured headlessly)

| file | result |
|---|---|
| `two_xforms.usda` (`.usda`) | ✅ opens (3 Xforms, 0 meshes — transform-only) |
| `agilex/scout_v2/scout_v2_base.usd` (plain `.usd`) | ✅ opens — 53 prims, **5 meshes with points** |
| `Kitchen_set.usdz` (`.usdz` with a `.usd` first layer) | ❌ rejected |

So **plain `.usd` and `.usda`/`.usdc` are fine** — the failure is specifically a
USDZ whose first/inner layer has the **`.usd`** extension.

## Cause

openusd-rs maps a layer's file format by **extension**, and the USDZ reader only
accepts `.usda` (text) or `.usdc` (crate binary). `.usd` is the
"format-agnostic" extension in USD — it should be resolved by **sniffing the
content** (a `.usdc` file starts with the magic bytes `PXR-USDC`; otherwise it is
`.usda` text). USD's own `Kitchen_set.usdz` ships its root layer as
`Kitchen_set.usd`, so this hits the most common reference asset.

## Suggested fix in openusd-rs

When opening a layer whose extension is `.usd` (or unknown) inside a USDZ archive
*and* for plain files, **detect the format from the first bytes** instead of
rejecting it:

```rust
fn layer_format(name: &str, head: &[u8]) -> Format {
    match ext(name) {
        "usda" => Format::Usda,
        "usdc" => Format::Usdc,
        // `.usd` (and anything else) → sniff content
        _ if head.starts_with(b"PXR-USDC") => Format::Usdc,
        _ => Format::Usda,
    }
}
```

This fixes Kitchen_set.usdz and any USDZ/`.usd` that uses the agnostic extension,
with no change to `.usda`/`.usdc` handling.

## Workaround in the meantime

Use plain `.usd` / `.usda` assets — e.g. the robots under
`assets/external/{agilex,clearpath,…}` load and have geometry. (We could also
extract a `.usd` USDZ and content-sniff its root layer app-side, but the clean
fix is in openusd-rs.)
