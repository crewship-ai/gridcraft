# Shared Mooring editor client

This directory vendors Crewship's canonical TypeScript editor SDK, rather than
maintaining a second GridCraft handshake implementation. `provenance.json`
identifies the source commit, source digests, compiler and compiled ES module.
The source is Apache-2.0; see `LICENSE`.

`authority.rs` imports `native-sdk.js` as a local wasm-bindgen module. Trunk
includes it in the immutable browser bundle; there is no extra runtime origin
or network dependency. The SDK owns exact parent source/origin verification,
protocol negotiation, private port transport and lifecycle callbacks. Native
`document.rs` and `protocol.rs` still own cell snapshots, drafts, pending edits,
receipts and undo. The applied native save state passes back through the SDK;
transport errors cannot become Saved.

Regenerate from the identified Crewship checkout with its locked dependencies:

```sh
node scripts/vendor-mooring-sdk.mjs <gridcraft-checkout>
```

Or reproduce the module from this directory using the exact esbuild version
recorded in the provenance file and its `build` command. The command has no
source-path or clock input. `packaging/web/verify-sdk.py` checks the committed
source and module hashes before a qualified browser build. Web qualification
still requires two builds of the complete package to match.
