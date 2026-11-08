# Spikes for the Rust port

Three questions had to be answered before the lanes start, because each one decides a crate. All three
answers are yes. The spike code stays in the repo as examples, so a lane can rerun it after a dependency
bump.

| Spike | Crate it decides | Answer |
|---|---|---|
| a. rmcp serves the Python MCP client | `ownhealth-mcp` | yes, both protocol versions |
| b. pdfium reads the same words, boxes and fonts as pdfplumber | `ownhealth-ingest-pdf` | yes, with two fixes lane B owns |
| c. webauthn-rs verifies passkeys that py_webauthn registered | `ownhealth-auth` | yes, from the stored row alone |

## a. rmcp 3.9.0 against the Python MCP SDK 2.3.0

Code: `crates/ownhealth-mcp/examples/spike_rmcp.rs` (one `ping` tool inside axum at `/mcp`) and
`scripts/spike_mcp_client.py` (the Python SDK client, mcp 2.3.0).

```sh
cargo run -p ownhealth-mcp --example spike_rmcp -- 127.0.0.1:18765
uv run --with mcp==2.3.0 python server/scripts/spike_mcp_client.py http://127.0.0.1:18765/mcp
```

What we saw:

- rmcp 3.9.0 knows the protocol versions 2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25 and 2026-07-28
  (stateless, `server/discover`, `ttlMs` and `cacheScope` on lists).
- The Python client in each of its modes: `2026-07-28` mode negotiated `2026-07-28`; `legacy` mode
  negotiated `2025-11-25`; `auto` picked `2026-07-28`. Every mode listed `['ping']` and got the
  structured content back.
- The `Authorization` header reaches the tool handler: rmcp puts the request's `http::request::Parts`
  in `context.extensions`. That is where lane F reads the PAT.
- A raw 2025-11-25 `initialize` answers `content-type: application/json` (no SSE), as
  `json_response=True` did in Python.
- `server/discover` without an `MCP-Protocol-Version` header is refused with JSON-RPC error `-32020`.

The configuration that matches the Python server (stateless, JSON responses, no keep-alive) is the one
`ownhealth_mcp::router` uses:

```rust
StreamableHttpServerConfig::default()
    .with_legacy_session_mode(false)
    .with_json_response(true)
    .with_sse_keep_alive(None)
    .with_allowed_hosts(hosts)
```

Two things lane F must know: the server is mounted with `route_service("/mcp", ...)` so `/mcp/x` stays a
404 as in Python, and rmcp checks the `Host` header against `allowed_hosts` (Python turned DNS-rebinding
protection off and checked Origin itself; the placeholder allows loopback names and the base URL host).

## b. pdfium-render 0.9.5 against pdfplumber

Code: `crates/ownhealth-ingest-pdf/examples/spike_pdfium.rs`. PDFium is the bblanchon build
chromium/8125, fetched and checked by `scripts/fetch-pdfium.sh` (sha256 per platform in the script)
into `server/vendor/pdfium/<platform>/` (gitignored). The crate feature is `pdfium_8125`.

```sh
scripts/fetch-pdfium.sh
cargo run -p ownhealth-ingest-pdf --example spike_pdfium -- server/vendor/pdfium/mac-arm64/lib <file.pdf> <plumber.json>
```

The spike groups pdfium's characters into words the way pdfplumber does (split on whitespace and
generated characters, on an x gap over 1.5 pt, or a top shift over 3 pt) and converts the boxes to
pdfplumber's coordinates (origin top-left; bottom = page height minus (origin y plus the font descent),
top = bottom minus the font size).

Over the 47 synthetic lab slips that `fixtures/labslips/generate.py` writes (de, en, es, fr layouts,
prompt-injection slips, a broken ToUnicode map, range-first tables):

| Check | Result |
|---|---|
| word count | equal on all 47 |
| word text, in order | equal on all but two layouts (below) |
| font name and size per word | equal on 100% of words |
| largest bbox difference | 0.09 to 0.41 pt |

The two differences, and the fix lane B owns:

1. **Word order on range-first layouts.** pdfplumber sorts words by line, then x; pdfium returns content
   stream order. Sort by line (top, with the same 3 pt tolerance), then x0.
2. **Characters with no Unicode mapping** (`es-broken-tounicode`): pdfplumber prints `(cid:1)`, pdfium
   returns the raw code `\x01`. Emit `(cid:N)` for unmapped codes, or the grounding and the
   `name_not_on_line` checks read different text.

The box difference comes from metrics: pdfium's Helvetica descent is about -0.210 em where the Adobe AFM
says -0.207, and pdfium's x1 is the glyph box where pdfplumber adds AFM advance widths. Using the base-14
AFM descents and widths for the standard fonts should close it; lane B checks that on the same slips,
because stored bboxes must stay equal between the backends.

## c. webauthn-rs 0.6.2 against py_webauthn

Code: `crates/ownhealth-auth/examples/spike_webauthn.rs` and `scripts/spike_webauthn.py` (a software
authenticator: P-256 key from `cryptography`, CBOR from `cbor2`).

```sh
cargo build -p ownhealth-auth --example spike_webauthn
uv run --with webauthn==3.0.1 --with cbor2==6.2.1 --with cryptography==50.0.2 python server/scripts/spike_webauthn.py server/target/debug/examples/spike_webauthn
```

The Python side registers through `py_webauthn.verify_registration_response` (attestation `none`) and
stores the row exactly as `auth/passkeys.py` does: credential id, the raw COSE public key (hex), the sign
count. The Rust side gets only that row, builds a `Passkey` from it, issues a discoverable login challenge
and verifies the assertion the Python authenticator signs.

| Run | Result |
|---|---|
| genuine assertion | ok, counter 1, user handle = the registration's user uuid |
| tampered signature | refused (`AuthenticationFailure`) |

What lane G needs from this:

- Feature `danger-credential-internals` builds a `Credential` from the stored COSE key
  (`COSEKey::try_from(&serde_cbor_2::Value)`), with `UserVerificationPolicy::Required`, no attestation,
  `RegisteredExtensions::none()`. `danger-allow-state-serialisation` stores the challenge state in
  `auth_challenge.data`, as Python stored its options.
- Use `finish_discoverable_authentication_v2` (the older call is deprecated).
- The user handle is the user uuid's 16 bytes, the same bytes py_webauthn wrote.
- For new registrations, take the raw COSE key bytes out of the attestation's authenticator data so the
  `public_key` column keeps its format; passkeys made by either backend then work on both.
- webauthn-rs 0.6 is pure Rust (crypto-glue); no OpenSSL in the image.
