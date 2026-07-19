//! The zink UI (Leptos, CSR): presentation only. Every decision that isn't
//! layout — naming, ordering, threading, crypto — happens on the other side
//! of `invoke`, in the command layer and `zink-client` beneath it.

mod image;
mod invoke;

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use wasm_bindgen::prelude::wasm_bindgen;
use zink_app_dto::{AppState, Conversation, Message, OutgoingImage, QrPayload, WhoIsReport};

#[wasm_bindgen(start)]
pub fn start() {
    leptos::mount::mount_to_body(App);
}

/// Which screen is showing. `Chat` carries its label so the header doesn't
/// need a lookup.
#[derive(Clone, PartialEq)]
enum View {
    Chats,
    Chat { id: String, label: String },
    Contacts,
}

#[derive(Serialize)]
struct NoArgs {}

#[component]
fn App() -> impl IntoView {
    let view = RwSignal::new(View::Chats);
    let state = RwSignal::new(None::<AppState>);
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let messages = RwSignal::new(Vec::<Message>::new());
    let status = RwSignal::new((String::new(), ""));

    let flash = move |text: String, class: &'static str| status.set((text, class));
    let ok = move |text: &str| flash(text.to_string(), "ok");
    let err = move |text: String| flash(format!("❌ {text}"), "err");

    let load_state = move || {
        spawn_local(async move {
            match invoke::invoke::<AppState>("app_state", &NoArgs {}).await {
                Ok(loaded) => {
                    // First run: no profile yet → straight to the setup view.
                    if loaded.name.is_none() {
                        view.set(View::Contacts);
                    }
                    state.set(Some(loaded));
                }
                Err(e) => err(e),
            }
        })
    };
    let load_conversations = move || {
        spawn_local(async move {
            match invoke::invoke::<Vec<Conversation>>("conversations", &NoArgs {}).await {
                Ok(list) => conversations.set(list),
                Err(e) => err(e),
            }
        })
    };
    let load_messages = move |conversation: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: &'a str,
            }
            let args = Args {
                conversation: &conversation,
            };
            match invoke::invoke::<Vec<Message>>("messages", &args).await {
                Ok(list) => messages.set(list),
                Err(e) => err(e),
            }
        })
    };

    load_state();
    load_conversations();

    // Live delivery (C4b): the Rust side's subscription loops emit
    // `new-messages` per nudged drain; re-render from the store.
    let on_arrival = move || {
        load_conversations();
        if let View::Chat { id, .. } = view.get_untracked() {
            load_messages(id);
        }
    };
    invoke::on_event("new-messages", move |_| on_arrival());

    // …and the old poll stays as a coarse backstop (rendezvous doc §8:
    // belt & suspenders — covers a wedged subscription).
    invoke::every(60_000, move || {
        spawn_local(async move {
            if let Ok(new_count) = invoke::invoke::<usize>("refresh", &NoArgs {}).await
                && new_count > 0
            {
                on_arrival();
            }
        });
    });

    let open_chat = move |id: String, label: String| {
        load_messages(id.clone());
        view.set(View::Chat { id, label });
    };

    view! {
        <header>
            <button
                class:active=move || matches!(view.get(), View::Chats | View::Chat { .. })
                on:click=move |_| {
                    load_conversations();
                    view.set(View::Chats);
                }
            >
                "chats"
            </button>
            <button
                class:active=move || view.get() == View::Contacts
                on:click=move |_| {
                    load_state();
                    view.set(View::Contacts);
                }
            >
                "contacts"
            </button>
        </header>
        <div
            id="status"
            class=move || status.get().1
        >
            {move || status.get().0}
        </div>
        {move || match view.get() {
            View::Chats => view! {
                <ChatsView
                    conversations=conversations
                    state=state
                    open_chat=open_chat
                    reload=load_conversations
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Chat { id, label } => view! {
                <ChatView
                    id=id
                    label=label
                    messages=messages
                    reload_messages=load_messages
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Contacts => view! {
                <ContactsView state=state reload=load_state ok=ok err=err />
            }
            .into_any(),
        }}
    }
}

/// Conversation list + the "new chat" composer (pick a contact, write the
/// first message — later messages happen inside the chat view).
#[component]
fn ChatsView(
    conversations: RwSignal<Vec<Conversation>>,
    state: RwSignal<Option<AppState>>,
    open_chat: impl Fn(String, String) + Copy + Send + 'static,
    reload: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let to = RwSignal::new(String::new());
    let text = RwSignal::new(String::new());
    let contacts = move || state.get().map(|state| state.contacts).unwrap_or_default();

    let start_chat = move |_| {
        let (petname, body) = (to.get_untracked(), text.get_untracked());
        if petname.is_empty() || body.trim().is_empty() {
            return err("pick a contact and write a message".into());
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: Option<&'a str>,
                to: Option<&'a str>,
                text: &'a str,
            }
            let args = Args {
                conversation: None,
                to: Some(&petname),
                text: &body,
            };
            match invoke::invoke::<String>("send_message", &args).await {
                Ok(conversation) => {
                    text.set(String::new());
                    ok("sent");
                    open_chat(conversation, petname);
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            {move || {
                conversations
                    .get()
                    .into_iter()
                    .map(|conversation| {
                        let (id, label) = (conversation.id.clone(), conversation.label.clone());
                        view! {
                            <div class="row" on:click=move |_| open_chat(id.clone(), label.clone())>
                                <b>{conversation.label}</b>
                                <span class="dim">{format!("{} message(s)", conversation.message_count)}</span>
                            </div>
                        }
                    })
                    .collect::<Vec<_>>()
            }}
            <div class="compose">
                <select on:change=move |ev| to.set(event_target_value(&ev))>
                    <option value="" selected disabled>
                        "new chat with…"
                    </option>
                    {move || {
                        contacts()
                            .into_iter()
                            .map(|contact| {
                                let value = contact.petname.clone();
                                view! { <option value=value>{contact.petname}</option> }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
                <textarea
                    rows="2"
                    placeholder="first message"
                    prop:value=move || text.get()
                    on:input=move |ev| text.set(event_target_value(&ev))
                />
                <button on:click=start_chat>"send"</button>
                <button class="secondary" on:click=move |_| reload()>
                    "refresh"
                </button>
            </div>
        </main>
    }
}

/// Fetch one blob of a stored message as a display-ready data URL.
async fn blob_data_url(conversation: &str, message: &str, hash: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        conversation: &'a str,
        message: &'a str,
        hash: &'a str,
    }
    let args = Args {
        conversation,
        message,
        hash,
    };
    let b64 = invoke::invoke::<String>("fetch_blob", &args).await?;
    Ok(image::data_url(&b64))
}

/// One conversation: linearized messages (text + image thumbnails, tap for
/// full-res) plus a reply box with an optional image attachment.
#[component]
fn ChatView(
    id: String,
    label: String,
    messages: RwSignal<Vec<Message>>,
    reload_messages: impl Fn(String) + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let draft = RwSignal::new(String::new());
    let attachment = RwSignal::new(None::<(OutgoingImage, String)>);
    /// hash → data URL; present-but-empty marks an in-flight fetch.
    let thumbs = RwSignal::new(HashMap::<String, String>::new());
    /// Full-res overlay: `Some("")` = loading, `Some(url)` = showing.
    let viewer = RwSignal::new(None::<String>);
    let conversation = StoredValue::new(id);

    // Fetch (cache-backed) every visible thumbnail not yet loaded.
    Effect::new(move |_| {
        for message in messages.get() {
            for blob in message.blobs.iter().filter(|blob| blob.kind == "thumbnail") {
                let known = thumbs.with_untracked(|thumbs| thumbs.contains_key(&blob.hash));
                if known {
                    continue;
                }
                thumbs.update(|thumbs| {
                    thumbs.insert(blob.hash.clone(), String::new());
                });
                let (message_id, hash) = (message.id.clone(), blob.hash.clone());
                spawn_local(async move {
                    let conversation = conversation.get_value();
                    match blob_data_url(&conversation, &message_id, &hash).await {
                        Ok(url) => thumbs.update(|thumbs| {
                            thumbs.insert(hash, url);
                        }),
                        Err(e) => err(e),
                    }
                });
            }
        }
    });

    let open_full = move |message_id: String, hash: String| {
        viewer.set(Some(String::new())); // loading
        spawn_local(async move {
            let conversation = conversation.get_value();
            match blob_data_url(&conversation, &message_id, &hash).await {
                Ok(url) => viewer.set(Some(url)),
                Err(e) => {
                    viewer.set(None);
                    err(e);
                }
            }
        });
    };

    let attach = move |ev: leptos::ev::Event| {
        let input = event_target::<web_sys::HtmlInputElement>(&ev);
        let Some(file) = input.files().and_then(|files| files.get(0)) else {
            return;
        };
        spawn_local(async move {
            match image::prepare(&file).await {
                Ok(prepared) => attachment.set(Some(prepared)),
                Err(e) => err(e),
            }
        });
    };

    // "who is this?" (D1c): `Some((subject, None))` = asking, `Some((_,
    // Some(report)))` = showing candidates. Manual trigger only — asking
    // reveals the interest to every contact asked (who-is-this.md §5).
    let whois = RwSignal::new(None::<(String, Option<WhoIsReport>)>);
    let ask = move |subject: String| {
        whois.set(Some((subject.clone(), None)));
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &subject };
            match invoke::invoke::<WhoIsReport>("who_is", &args).await {
                Ok(report) => whois.set(Some((subject, Some(report)))),
                Err(e) => {
                    whois.set(None);
                    err(e);
                }
            }
        });
    };
    let add_learned = move |payload: String| {
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
                petname: Option<&'a str>,
            }
            let args = Args {
                payload: &payload,
                petname: None, // prefilled from the self-claimed name
            };
            match invoke::invoke::<String>("add_contact", &args).await {
                Ok(petname) => {
                    ok(&format!("added {petname}"));
                    whois.set(None);
                    reload_messages(id); // sender labels flip to the petname
                }
                Err(e) => err(e),
            }
        });
    };
    // Unknown participants of this conversation, deduped — the banner rows.
    let unknown_keys = move || {
        let mut keys: Vec<String> = messages
            .get()
            .into_iter()
            .filter_map(|message| message.unknown_sender)
            .collect();
        keys.sort();
        keys.dedup();
        keys
    };

    let send = move |_| {
        let body = draft.get_untracked();
        let image = attachment.get_untracked().map(|(image, _)| image);
        if body.trim().is_empty() && image.is_none() {
            return;
        }
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: Option<&'a str>,
                to: Option<&'a str>,
                text: &'a str,
                image: Option<OutgoingImage>,
            }
            let args = Args {
                conversation: Some(&id),
                to: None,
                text: &body,
                image,
            };
            match invoke::invoke::<String>("send_message", &args).await {
                Ok(_) => {
                    draft.set(String::new());
                    attachment.set(None);
                    reload_messages(id);
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            <h3>{label}</h3>
            {move || {
                let keys = unknown_keys();
                (!keys.is_empty())
                    .then(|| {
                        view! {
                            <div class="panel">
                                {keys
                                    .into_iter()
                                    .map(|key| {
                                        let short = key.chars().take(8).collect::<String>();
                                        view! {
                                            <div class="row">
                                                <span class="dim">
                                                    {format!("unknown participant {short}…")}
                                                </span>
                                                <button
                                                    class="secondary"
                                                    on:click=move |_| ask(key.clone())
                                                >
                                                    "who is this?"
                                                </button>
                                            </div>
                                        }
                                    })
                                    .collect::<Vec<_>>()}
                            </div>
                        }
                    })
            }}
            {move || {
                whois
                    .get()
                    .map(|(_, report)| {
                        view! {
                            <div class="panel">
                                {match report {
                                    None => {
                                        view! { <span class="dim">"asking your contacts…"</span> }
                                            .into_any()
                                    }
                                    Some(report) => {
                                        let verdict = match (&report.contact, report.candidates.is_empty()) {
                                            (Some(petname), _) => Some(
                                                format!(
                                                    "already your contact {petname:?} — {} fresh answer(s)",
                                                    report.answers,
                                                ),
                                            ),
                                            (None, true) => Some(
                                                "no answers — none of your reachable contacts know this key"
                                                    .to_string(),
                                            ),
                                            (None, false) => None,
                                        };
                                        let candidates = report
                                            .candidates
                                            .into_iter()
                                            .map(|candidate| {
                                                view! {
                                                    <div class="row">
                                                        <b>{candidate.name}</b>
                                                        <span class="dim">{candidate.provenance}</span>
                                                        {candidate
                                                            .payload
                                                            .map(|payload| {
                                                                view! {
                                                                    <button on:click=move |_| add_learned(
                                                                        payload.clone(),
                                                                    )>"add as contact"</button>
                                                                }
                                                            })}
                                                    </div>
                                                }
                                            })
                                            .collect::<Vec<_>>();
                                        view! {
                                            {verdict.map(|text| view! { <span class="dim">{text}</span> })}
                                            {candidates}
                                        }
                                            .into_any()
                                    }
                                }}
                                <button class="secondary" on:click=move |_| whois.set(None)>
                                    "close"
                                </button>
                            </div>
                        }
                    })
            }}
            <div class="messages">
                {move || {
                    messages
                        .get()
                        .into_iter()
                        .map(|message| {
                            let class = if message.mine { "msg mine" } else { "msg" };
                            let body = message.text.clone().filter(|text| !text.is_empty());
                            let unopenable = message.text.is_none();
                            let full_hash = message
                                .blobs
                                .iter()
                                .find(|blob| blob.kind == "full")
                                .map(|blob| blob.hash.clone());
                            let images = message
                                .blobs
                                .iter()
                                .filter(|blob| blob.kind == "thumbnail")
                                .map(|blob| {
                                    let hash = blob.hash.clone();
                                    // Tap: full-res if the message has one, else the thumbnail itself.
                                    let target = full_hash.clone().unwrap_or_else(|| hash.clone());
                                    let message_id = message.id.clone();
                                    view! {
                                        {move || {
                                            let url = thumbs.with(|thumbs| thumbs.get(&hash).cloned());
                                            let (target, message_id) = (target.clone(), message_id.clone());
                                            match url.filter(|url| !url.is_empty()) {
                                                Some(url) => view! {
                                                    <img
                                                        class="thumb"
                                                        src=url
                                                        on:click=move |_| open_full(
                                                            message_id.clone(),
                                                            target.clone(),
                                                        )
                                                    />
                                                }
                                                    .into_any(),
                                                None => view! { <span class="dim">"📎 loading…"</span> }
                                                    .into_any(),
                                            }
                                        }}
                                    }
                                })
                                .collect::<Vec<_>>();
                            let delivery = message
                                .pending
                                .then_some(" · ⏳ not delivered yet")
                                .unwrap_or_default();
                            view! {
                                <div class=class>
                                    <span class="dim">
                                        {message.sender} " · " {time_of(message.timestamp_ms)} {delivery}
                                    </span>
                                    {images}
                                    {body.map(|text| view! { <div>{text}</div> })}
                                    {unopenable.then(|| view! { <div class="dim">"<unopenable>"</div> })}
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()
                }}
            </div>
            <div class="compose">
                {move || {
                    attachment
                        .get()
                        .map(|(_, preview)| {
                            view! {
                                <div class="pending">
                                    <img class="thumb" src=preview />
                                    <button class="secondary" on:click=move |_| attachment.set(None)>
                                        "remove image"
                                    </button>
                                </div>
                            }
                        })
                }}
                <input type="file" accept="image/*" on:change=attach />
                <textarea
                    rows="2"
                    placeholder="message"
                    prop:value=move || draft.get()
                    on:input=move |ev| draft.set(event_target_value(&ev))
                />
                <button on:click=send>"send"</button>
            </div>
            {move || {
                viewer
                    .get()
                    .map(|url| {
                        view! {
                            <div class="viewer" on:click=move |_| viewer.set(None)>
                                {if url.is_empty() {
                                    view! { <span>"loading…"</span> }.into_any()
                                } else {
                                    view! { <img src=url /> }.into_any()
                                }}
                            </div>
                        }
                    })
            }}
        </main>
    }
}

/// Profile (name + home relay + QR) and contact management — the C2 flows,
/// ported: display/copy your record, scan or paste a friend's.
#[component]
fn ContactsView(
    state: RwSignal<Option<AppState>>,
    reload: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let name = RwSignal::new(String::new());
    let relay = RwSignal::new(String::new());
    let paste = RwSignal::new(String::new());

    // Prefill the form from the loaded profile (once per state change).
    Effect::new(move |_| {
        if let Some(state) = state.get() {
            if let Some(profile_name) = state.name {
                name.set(profile_name);
            }
            if let Some(home_relay) = state.relay {
                relay.set(home_relay);
            }
        }
    });

    let save = move |_| {
        let (name, relay) = (name.get_untracked(), relay.get_untracked());
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                name: &'a str,
                relay: &'a str,
            }
            let args = Args {
                name: &name,
                relay: &relay,
            };
            match invoke::invoke::<QrPayload>("set_profile", &args).await {
                Ok(_) => {
                    reload();
                    ok("profile saved — let a friend scan your QR");
                }
                Err(e) => err(e),
            }
        });
    };

    let add = move |payload: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
                petname: Option<&'a str>,
            }
            let args = Args {
                payload: &payload,
                petname: None,
            };
            match invoke::invoke::<String>("add_contact", &args).await {
                Ok(petname) => {
                    paste.set(String::new());
                    reload();
                    ok(&format!("added {petname}"));
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
                Ok(scanned) => add(scanned.content),
                // A cancelled scan also lands here — worth no red banner.
                Err(e) => err(e),
            }
        });
    };

    // Freshness pull (D1c, who-is-this.md §7): re-ask the network about a
    // contact. Fresh answers land in the learned store and sharpen relay
    // resolution on their own — nothing to apply, nothing overwritten.
    let refresh_contact = move |subject: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &subject };
            match invoke::invoke::<WhoIsReport>("who_is", &args).await {
                Ok(report) => ok(&format!(
                    "{} answer(s) — fresh records apply automatically",
                    report.answers
                )),
                Err(e) => err(e),
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
            <input
                placeholder="how contacts see you"
                prop:value=move || name.get()
                on:input=move |ev| name.set(event_target_value(&ev))
            />
            <input
                placeholder="endpoint-id@ip:port"
                prop:value=move || relay.get()
                on:input=move |ev| relay.set(event_target_value(&ev))
            />
            <button on:click=save>"save profile & show QR"</button>
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
            <h3>"add contact"</h3>
            <button on:click=scan>"scan QR"</button>
            <textarea
                rows="2"
                placeholder="…or paste a ZINK: payload"
                prop:value=move || paste.get()
                on:input=move |ev| paste.set(event_target_value(&ev))
            />
            <button class="secondary" on:click=move |_| add(paste.get_untracked())>
                "add from pasted text"
            </button>
            <h3>"contacts"</h3>
            {move || {
                state
                    .get()
                    .map(|state| state.contacts)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|contact| {
                        let subject = contact.key.clone();
                        view! {
                            <div class="row">
                                <b>{contact.petname}</b>
                                <button
                                    class="secondary"
                                    on:click=move |_| refresh_contact(subject.clone())
                                >
                                    "who is?"
                                </button>
                            </div>
                        }
                    })
                    .collect::<Vec<_>>()
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

/// hh:mm from the sender's wall-clock hint — display only, like the hint.
fn time_of(timestamp_ms: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}
