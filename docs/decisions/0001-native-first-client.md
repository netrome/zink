# 0001 · Native-first client (Tauri v2) over PWA/WASM for the MVP

- **Status:** Accepted
- **Date:** 2026-07-11
- **Tenets:** 5 (relays are untrusted infrastructure), 9 (p2p when possible, relayed when necessary)
- **Where it landed:** project 1-mvp · Stage C pivot (SPEC §11 updated); C-spike verified

## Context

The plan of record had a browser/PWA client as the MVP's first client. Building it
surfaced that the browser platform carries the MVP's *hardest* costs and, worse,
denies true p2p:

- **Web Push** for offline wake-up (server keys, VAPID, a push service in the loop).
- An **evictable IndexedDB keystore** — the browser can drop your private keys.
- **TLS + domain + cert ops** just to stand the relay up for browsers.
- No hole-punching: browser traffic is *always* relay-routed.

A native client (Tauri v2, Leptos UI, Android + Linux desktop) replaces every one
of those with something simpler and more aligned: a **persistent connection** for
delivery ("forward-now" nudges) instead of Web Push, a **filesystem keystore**
reusing the B5 client work instead of evictable storage, and **direct `id@ip:port`
dialing** with real hole-punching instead of mandatory relaying.

## Decision

The MVP client is **native Android + Linux desktop (Tauri v2, Leptos UI)**. The
PWA/WASM client is **deferred to post-MVP**, where it becomes the second
implementation — the cross-implementation interop proof the protocol wants.

## Consequences

- The browser groundwork stays in-tree, unbuilt-upon but not deleted: the A6
  browser→relay spike, the `crates/zink-client` WASM build, and `web/spike`. The
  WASM target keeps compiling (sync/native paths are `cfg`-gated) so the second
  client isn't a from-scratch effort later.
- Delivery, keystore, and connectivity designs are native-shaped for the MVP; the
  PWA will re-solve wake-up (Web Push) and keystore (evictable) on its own terms.
- Escape hatch: none needed — this narrows scope rather than betting on an unknown.
  Revisit only when the second client is scheduled.

## Ties to the philosophy

Tenet 9 is explicit that browsers can't hole-punch and so ride relays, while
native clients get true direct paths — going native-first for the MVP means the
flagship client actually *is* p2p when the transport allows, instead of the whole
MVP inheriting the browser's relay-everything constraint. And because relays stay
untrusted ciphertext-movers either way (tenet 5), dropping TLS/VAPID/domain ops
removes operational surface without weakening the trust model: a native client
authenticates peers by endpoint key, not by a webPKI cert in the path.
