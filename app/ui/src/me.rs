use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{AppState, QrPayload, RELAY_QR_PREFIX, RecordPreview};

use crate::{NoArgs, avatar_data_url, image, invoke};

/// The Me form's in-progress edits (S4, U8): App-lifetime signals, so a tab
/// bounce (which remounts the view) can't destroy typing.
#[derive(Clone, Copy)]
pub(crate) struct MeForm {
    pub name: RwSignal<String>,
    /// The device qualifier beside the name ("phone", "laptop") — the S1
    /// split: `name` is the person, this is the device.
    pub device_label: RwSignal<String>,
    pub relays: RwSignal<Vec<String>>,
    pub new_relay: RwSignal<String>,
    /// Set by any edit, cleared on save — blocks the prefill from
    /// clobbering typing when a (background) reload lands.
    pub dirty: RwSignal<bool>,
}

impl Default for MeForm {
    fn default() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            device_label: RwSignal::new(String::new()),
            relays: RwSignal::new(Vec::new()),
            new_relay: RwSignal::new(String::new()),
            dirty: RwSignal::new(false),
        }
    }
}

/// "Me" — your own identity: profile (name, home relay, avatar, QR), your
/// recognized devices, and device pairing. The C2/D3e flows, unchanged — the
/// U2 screen split just homes them here. Its scan always pairs (previews a
/// record before signing); the contact-adding scan lives in `PeopleView`.
#[component]
pub(crate) fn MeView(
    state: RwSignal<Option<AppState>>,
    reload: impl Fn() + Copy + Send + 'static,
    form: MeForm,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    // The home-relay set (U5 multi-relay), edited locally and persisted on
    // save; `new_relay` is the add field.
    let MeForm {
        name,
        device_label,
        relays,
        new_relay,
        dirty,
    } = form;
    // Pairing paste buffer (a device record to recognize).
    let paste = RwSignal::new(String::new());

    // Prefill the form from the loaded profile — but never over typing
    // (S4): `dirty` blocks the refill until a save re-syncs it.
    Effect::new(move |_| {
        if let Some(state) = state.get() {
            if dirty.get_untracked() {
                return;
            }
            if let Some(profile_name) = state.name {
                name.set(profile_name);
            }
            if let Some(label) = state.device_label {
                device_label.set(label);
            }
            relays.set(state.relays);
        }
    });

    // Append one spec to the edited list (R4): scanned payloads and pasted
    // `ZINK-RELAY:` text both land here, normalized to the bare spec. The
    // existing "save" stays the one explicit act that applies the profile.
    let stage_relay = move |spec: &str| {
        let value = spec
            .trim()
            .strip_prefix(RELAY_QR_PREFIX)
            .unwrap_or(spec.trim())
            .trim()
            .to_string();
        if value.is_empty() {
            return;
        }
        relays.update(|list| {
            if !list.contains(&value) {
                list.push(value);
            }
        });
        // Staging is an edit, whichever door it came through (typed or
        // scanned) — without this, a background reload's prefill would
        // silently drop a scanned-but-unsaved relay (the U8 clobber).
        dirty.set(true);
    };
    let add_relay = move |_| {
        stage_relay(&new_relay.get_untracked());
        new_relay.set(String::new());
    };
    let remove_relay = move |value: String| {
        relays.update(|list| list.retain(|relay| relay != &value));
        dirty.set(true);
    };

    let save = move |_| {
        let (name, relays) = (name.get_untracked(), relays.get_untracked());
        let device_label = device_label.get_untracked();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                name: &'a str,
                relays: &'a [String],
                device_label: &'a str,
            }
            let args = Args {
                name: &name,
                relays: &relays,
                device_label: &device_label,
            };
            match invoke::invoke::<QrPayload>("set_profile", &args).await {
                Ok(_) => {
                    // Saved — the reload's prefill may take over again.
                    dirty.set(false);
                    reload();
                    ok("profile saved — let a friend scan your code");
                }
                Err(e) => err(e),
            }
        });
    };

    // Own avatar (D1d): preview loaded from the store, replaced via the
    // picker (canvas-downscaled, then encrypted + pushed on the Rust side).
    let my_avatar = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if let Some(loaded) = state.get() {
            let key = loaded.my_key.clone();
            spawn_local(async move {
                if let Ok(url) = avatar_data_url(&key).await {
                    my_avatar.set(url);
                }
            });
        }
    });
    let pick_avatar = move |ev: leptos::ev::Event| {
        let input = event_target::<web_sys::HtmlInputElement>(&ev);
        let Some(file) = input.files().and_then(|files| files.get(0)) else {
            return;
        };
        spawn_local(async move {
            let (b64, preview) = match image::prepare_avatar(&file).await {
                Ok(prepared) => prepared,
                Err(e) => return err(e),
            };
            #[derive(Serialize)]
            struct Args<'a> {
                image: &'a str,
            }
            let args = Args { image: &b64 };
            match invoke::invoke::<usize>("set_avatar", &args).await {
                Ok(pushed) => {
                    my_avatar.set(Some(preview));
                    ok(&format!(
                        "avatar set — pushed to {pushed} relay(s); contacts pick it up \
                         from a re-scanned QR or a who-is"
                    ));
                }
                Err(e) => err(e),
            }
        });
    };

    // Pair mode (D3e, multi-device.md §3): a scanned/pasted record is
    // previewed — name + full-key fingerprint — and NOTHING is signed
    // until the explicit confirm.
    let pair_preview = RwSignal::new(None::<(String, RecordPreview)>);
    let preview = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
            }
            let args = Args { payload: &payload };
            match invoke::invoke::<RecordPreview>("inspect_record", &args).await {
                Ok(decoded) => pair_preview.set(Some((payload, decoded))),
                Err(e) => err(e),
            }
        });
    };
    let recognize = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
            }
            let args = Args { payload: &payload };
            match invoke::invoke::<String>("recognize_device", &args).await {
                Ok(name) => {
                    pair_preview.set(None);
                    paste.set(String::new());
                    reload();
                    ok(&format!(
                        "linked {name} as your device — link back from it too, \
                         so both sides agree"
                    ));
                }
                Err(e) => err(e),
            }
        });
    };
    let unrecognize = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("unrecognize_device", &args).await {
                Ok(_) => {
                    reload();
                    ok("unlinked — local only, nothing published");
                }
                Err(e) => err(e),
            }
        });
    };
    // Repudiation of a device key (D4c) — armed-then-confirmed (two taps); it
    // publishes.
    let armed = RwSignal::new(None::<String>);
    let repudiate = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("repudiate_key", &args).await {
                Ok(_) => {
                    armed.set(None);
                    reload();
                    ok("marked compromised — published in your record; contacts \
                        learn it from their next pull");
                }
                Err(e) => err(e),
            }
        });
    };

    // Scanning state drives the cancel overlay AND page transparency: with
    // `windowed: true` the camera renders *behind* the webview, so html/body
    // must go transparent (the `scanning` class) for it to show through —
    // and our own overlay stays on top with a way out (the C2 footgun).
    let scanning = RwSignal::new(false);
    Effect::new(move |_| {
        if let Some(root) = document().document_element() {
            root.set_class_name(if scanning.get() { "scanning" } else { "" });
        }
    });
    let scan = move |_| {
        scanning.set(true);
        spawn_local(async move {
            #[derive(Serialize)]
            struct ScanArgs {
                windowed: bool,
                formats: Vec<&'static str>,
            }
            #[derive(serde::Deserialize)]
            struct Scanned {
                content: String,
            }
            if let Err(e) = invoke::invoke::<serde::de::IgnoredAny>(
                "plugin:barcode-scanner|request_permissions",
                &NoArgs {},
            )
            .await
            {
                scanning.set(false);
                return err(e);
            }
            let args = ScanArgs {
                windowed: true,
                formats: vec!["QR_CODE"],
            };
            let result = invoke::invoke::<Scanned>("plugin:barcode-scanner|scan", &args).await;
            scanning.set(false);
            match result {
                // One scanner, payload decides (R4): a relay code joins the
                // relay list (the save is the confirm); anything else is
                // pair mode — preview before signing, as always.
                Ok(scanned) if scanned.content.trim().starts_with(RELAY_QR_PREFIX) => {
                    stage_relay(&scanned.content);
                    ok("relay added to the list — save to apply");
                }
                Ok(scanned) => preview(scanned.content),
                Err(e) => crate::scan_failed(err, e),
            }
        });
    };
    let cancel_scan = move |_| {
        spawn_local(async move {
            // Rejects the pending scan invoke, which resets `scanning`.
            let _ = invoke::invoke::<serde::de::IgnoredAny>(
                "plugin:barcode-scanner|cancel",
                &NoArgs {},
            )
            .await;
        });
    };

    view! {
        <main>
            <h3>"me"</h3>
            <div class="pending">
                {move || {
                    my_avatar.get().map(|url| view! { <img class="avatar avatar-lg" src=url /> })
                }}
                <label>
                    "avatar: "
                    <input type="file" accept="image/*" on:change=pick_avatar />
                </label>
            </div>
            <input
                placeholder="how contacts see you"
                prop:value=move || name.get()
                on:input=move |ev| {
                    name.set(event_target_value(&ev));
                    dirty.set(true);
                }
            />
            <input
                placeholder="this device — phone, laptop… (optional)"
                prop:value=move || device_label.get()
                on:input=move |ev| {
                    device_label.set(event_target_value(&ev));
                    dirty.set(true);
                }
            />
            <h3>"your relays"</h3>
            <div class="dim">
                "where your messages wait when you're offline — add one you run, or one a friend shares"
            </div>
            {move || {
                let list = relays.get();
                if list.is_empty() {
                    view! {
                        <div class="dim">"no relay yet — add one below, or friends can't reach you"</div>
                    }
                        .into_any()
                } else {
                    list.into_iter()
                        .map(|relay| {
                            let value = relay.clone();
                            view! {
                                <div class="row">
                                    <span class="dim" id="record-text">{relay}</span>
                                    <button
                                        class="secondary"
                                        on:click=move |_| remove_relay(value.clone())
                                    >
                                        "remove"
                                    </button>
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()
                        .into_any()
                }
            }}
            <input
                placeholder="endpoint-id@ip:port#http://ip:port"
                prop:value=move || new_relay.get()
                on:input=move |ev| new_relay.set(event_target_value(&ev))
            />
            <button class="secondary" on:click=add_relay>
                "add relay"
            </button>
            <button class="secondary" on:click=scan>
                "scan a relay QR"
            </button>
            <button on:click=save>"save & show my code"</button>
            {move || {
                state
                    .get()
                    .and_then(|state| state.record)
                    .map(|record| {
                        view! {
                            <div id="qr" inner_html=record.svg></div>
                            <div id="record-text">{record.text}</div>
                        }
                    })
            }}
            // The fingerprint another device confirms against when it
            // recognizes this one (D3e, multi-device.md §3).
            {move || {
                state
                    .get()
                    .map(|state| {
                        view! {
                            <div class="dim" id="record-text">
                                {format!("this device's fingerprint: {}", state.my_key)}
                            </div>
                        }
                    })
            }}
            <h3>"my devices"</h3>
            <div class="dim">
                "your other devices. linking is one-way — link this device from each \
                 of them too, so both sides agree"
            </div>
            {move || {
                let devices = state.get().map(|state| state.devices).unwrap_or_default();
                if devices.is_empty() {
                    view! {
                        <div class="dim">
                            "none linked yet — link one by scanning its QR"
                        </div>
                    }
                        .into_any()
                } else {
                    devices
                        .into_iter()
                        .map(|device| {
                            let short = device.key.chars().take(8).collect::<String>();
                            let unrec_key = device.key.clone();
                            let arm_key = device.key.clone();
                            let label_key = device.key.clone();
                            view! {
                                <div class="row">
                                    <b>{device.name}</b>
                                    <span class="dim">{format!("{short}…")}</span>
                                    // Losing interest vs declaring it
                                    // compromised (web-of-trust.md §6).
                                    <button
                                        class="secondary"
                                        on:click=move |_| unrecognize(unrec_key.clone())
                                    >
                                        "unlink"
                                    </button>
                                    <button
                                        class="secondary"
                                        on:click=move |_| {
                                            if armed.get_untracked().as_deref()
                                                == Some(arm_key.as_str())
                                            {
                                                repudiate(arm_key.clone());
                                            } else {
                                                armed.set(Some(arm_key.clone()));
                                                // Armed-forever is a footgun
                                                // (S4): an untouched confirm
                                                // disarms itself.
                                                let timeout_key = arm_key.clone();
                                                set_timeout(
                                                    move || {
                                                        if armed.get_untracked().as_deref()
                                                            == Some(timeout_key.as_str())
                                                        {
                                                            armed.set(None);
                                                        }
                                                    },
                                                    std::time::Duration::from_secs(4),
                                                );
                                            }
                                        }
                                    >
                                        {move || {
                                            if armed.get().as_deref() == Some(label_key.as_str()) {
                                                "⚠ confirm — mark compromised"
                                            } else {
                                                "mark compromised"
                                            }
                                        }}
                                    </button>
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()
                        .into_any()
                }
            }}
            <button on:click=scan>"link a device: scan its QR"</button>
            <textarea
                rows="2"
                placeholder="…or paste their code to link"
                prop:value=move || paste.get()
                on:input=move |ev| paste.set(event_target_value(&ev))
            />
            <button class="secondary" on:click=move |_| preview(paste.get_untracked())>
                "link from pasted code"
            </button>
            {move || {
                pair_preview
                    .get()
                    .map(|(payload, decoded)| {
                        let confirm_payload = payload;
                        view! {
                            <div class="wild">
                                <div class="row">
                                    <b>"link this as your device?"</b>
                                </div>
                                <div class="row">
                                    <b>{decoded.name.clone().unwrap_or_else(|| "(unnamed)".to_string())}</b>
                                </div>
                                // The one real risk (multi-device.md §3):
                                // compare against the key shown on the
                                // other device before signing anything.
                                <div class="dim" id="record-text">
                                    {format!("fingerprint: {}", decoded.key)}
                                </div>
                                <div class="row">
                                    <button on:click=move |_| recognize(confirm_payload.clone())>
                                        "yes, this is my device"
                                    </button>
                                    <button
                                        class="secondary"
                                        on:click=move |_| pair_preview.set(None)
                                    >
                                        "cancel"
                                    </button>
                                </div>
                            </div>
                        }
                    })
            }}
            {move || {
                scanning
                    .get()
                    .then(|| {
                        view! {
                            <div class="scan-overlay">
                                <span>"point the camera at a zink QR"</span>
                                <button class="secondary" on:click=cancel_scan>
                                    "cancel"
                                </button>
                            </div>
                        }
                    })
            }}
        </main>
    }
}
