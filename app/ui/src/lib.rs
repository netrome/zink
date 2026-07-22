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
use zink_app_dto::{
    AppState, Conversation, Message, OutgoingImage, QrPayload, RecordPreview, UnknownMember,
    WhoIsReport,
};

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
                    state=state
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
    let selected = RwSignal::new(std::collections::BTreeSet::<String>::new());
    let text = RwSignal::new(String::new());
    let contacts = move || state.get().map(|state| state.contacts).unwrap_or_default();

    // Multi-select compose (D2c): a group is just several recipients.
    let start_chat = move |_| {
        let names: Vec<String> = selected.get_untracked().into_iter().collect();
        let body = text.get_untracked();
        if names.is_empty() || body.trim().is_empty() {
            return err("pick at least one contact and write a message".into());
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: Option<&'a str>,
                to: Option<Vec<String>>,
                text: &'a str,
            }
            let label = names.join(", ");
            let args = Args {
                conversation: None,
                to: Some(names),
                text: &body,
            };
            match invoke::invoke::<String>("send_message", &args).await {
                Ok(conversation) => {
                    text.set(String::new());
                    selected.update(|selected| selected.clear());
                    ok("sent");
                    open_chat(conversation, label);
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
                <div class="picks">
                    <span class="dim">"new chat with:"</span>
                    {move || {
                        contacts()
                            .into_iter()
                            .map(|contact| {
                                let name = contact.petname.clone();
                                let toggled = contact.petname.clone();
                                view! {
                                    <label class="pick">
                                        <input
                                            type="checkbox"
                                            prop:checked=move || {
                                                selected.with(|selected| selected.contains(&name))
                                            }
                                            on:change=move |_| {
                                                selected
                                                    .update(|selected| {
                                                        if !selected.remove(&toggled) {
                                                            selected.insert(toggled.clone());
                                                        }
                                                    })
                                            }
                                        />
                                        {contact.petname}
                                    </label>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </div>
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

/// The best-believed avatar for a key as a data URL — `None` when nothing
/// is claimed or fetchable (render nothing; display data is best-effort).
async fn avatar_data_url(subject: &str) -> Result<Option<String>, String> {
    #[derive(Serialize)]
    struct Args<'a> {
        subject: &'a str,
    }
    let args = Args { subject };
    let b64 = invoke::invoke::<Option<String>>("avatar", &args).await?;
    Ok(b64.map(|b64| image::data_url(&b64)))
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
    state: RwSignal<Option<AppState>>,
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

    // Sender avatars (D1d), lazily fetched per key; present-but-empty
    // marks in-flight or none (both render nothing).
    let avatars = RwSignal::new(HashMap::<String, String>::new());
    Effect::new(move |_| {
        for message in messages.get() {
            if message.mine {
                continue;
            }
            let key = message.sender_key.clone();
            if avatars.with_untracked(|avatars| avatars.contains_key(&key)) {
                continue;
            }
            avatars.update(|avatars| {
                avatars.insert(key.clone(), String::new());
            });
            spawn_local(async move {
                if let Ok(Some(url)) = avatar_data_url(&key).await {
                    avatars.update(|avatars| {
                        avatars.insert(key, url);
                    });
                }
            });
        }
    });

    // A who-is can have learned a fresh avatar claim (De3): re-fetch past
    // the miss the lazy cache may have recorded for this key.
    let refetch_avatar = move |key: String| {
        spawn_local(async move {
            if let Ok(Some(url)) = avatar_data_url(&key).await {
                avatars.update(|avatars| {
                    avatars.insert(key, url);
                });
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
                Ok(report) => {
                    refetch_avatar(subject.clone());
                    whois.set(Some((subject, Some(report))));
                }
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
                    if let Some((subject, _)) = whois.get_untracked() {
                        refetch_avatar(subject); // their avatar may now resolve
                    }
                    whois.set(None);
                    reload_messages(id); // sender labels flip to the petname
                }
                Err(e) => err(e),
            }
        });
    };
    // Unknown members — the "wild key appeared" surface (D2c, groups.md
    // §5): loaded from membership (covers added-but-silent members, which
    // per-message sender fields would miss), refreshed whenever the
    // messages change (the scoped auto-query has run by then).
    let unknowns = RwSignal::new(Vec::<UnknownMember>::new());
    let load_unknowns = move || {
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: &'a str,
            }
            let args = Args { conversation: &id };
            if let Ok(list) = invoke::invoke::<Vec<UnknownMember>>("unknown_members", &args).await {
                unknowns.set(list);
            }
        });
    };
    Effect::new(move |_| {
        messages.track();
        load_unknowns();
    });
    let ignore = move |key: String| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                subject: &'a str,
            }
            let args = Args { subject: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("dismiss", &args).await {
                Ok(_) => load_unknowns(),
                Err(e) => err(e),
            }
        });
    };

    // Add a contact to this conversation (D2c): a message with the grown
    // recipient set is the whole mechanism — the signed recipients list
    // announces the membership change.
    let add_pick = RwSignal::new(String::new());
    let add_member = move |_| {
        let petname = add_pick.get_untracked();
        if petname.is_empty() {
            return;
        }
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: Option<&'a str>,
                add: Option<Vec<String>>,
                text: &'a str,
            }
            let args = Args {
                conversation: Some(&id),
                add: Some(vec![petname.clone()]),
                text: "",
            };
            match invoke::invoke::<String>("send_message", &args).await {
                Ok(_) => {
                    add_pick.set(String::new());
                    ok(&format!("added {petname} to the conversation"));
                    reload_messages(id);
                }
                Err(e) => err(e),
            }
        });
    };

    // Introduce-now (D3c sugar, D3e button): an empty-body message whose
    // signed recipients announce this device's siblings to everyone here.
    // Optional — the next organic message would do the same.
    let introduce = move |_| {
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: &'a str,
            }
            let args = Args { conversation: &id };
            match invoke::invoke::<serde::de::IgnoredAny>("introduce_devices", &args).await {
                Ok(_) => {
                    ok("your devices were introduced to this conversation");
                    reload_messages(id);
                }
                Err(e) => err(e),
            }
        });
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
                let list = unknowns.get();
                (!list.is_empty())
                    .then(|| {
                        view! {
                            <div class="panel">
                                {list
                                    .into_iter()
                                    .map(|member| {
                                        let short = member.key.chars().take(8).collect::<String>();
                                        if member.dismissed {
                                            let ask_key = member.key.clone();
                                            view! {
                                                <div class="row">
                                                    <span class="dim">{format!("{short}… (ignored)")}</span>
                                                    <button
                                                        class="secondary"
                                                        on:click=move |_| ask(ask_key.clone())
                                                    >
                                                        "who is this?"
                                                    </button>
                                                </div>
                                            }
                                                .into_any()
                                        } else {
                                            let ask_key = member.key.clone();
                                            let ignore_key = member.key.clone();
                                            let avatar_key = member.key.clone();
                                            // The popup upgrade (D3c): who
                                            // *claims* this key, tiered.
                                            let evidence = member
                                                .device_evidence
                                                .iter()
                                                .chain(member.disavowals.iter())
                                                .map(|line| {
                                                    view! {
                                                        <div class="row">
                                                            <span class="dim">{line.clone()}</span>
                                                        </div>
                                                    }
                                                })
                                                .collect::<Vec<_>>();
                                            let candidates = member
                                                .candidates
                                                .into_iter()
                                                .map(|candidate| {
                                                    let avatar_key = avatar_key.clone();
                                                    view! {
                                                        <div class="row">
                                                            <b>{candidate.name}</b>
                                                            <span class="dim">{candidate.provenance}</span>
                                                            {candidate
                                                                .payload
                                                                .map(|payload| {
                                                                    view! {
                                                                        <button on:click=move |_| {
                                                                            add_learned(payload.clone());
                                                                            refetch_avatar(avatar_key.clone());
                                                                        }>"add as contact"</button>
                                                                    }
                                                                })}
                                                        </div>
                                                    }
                                                })
                                                .collect::<Vec<_>>();
                                            view! {
                                                <div class="wild">
                                                    <div class="row">
                                                        <b>{format!("a wild key appeared: {short}…")}</b>
                                                        <button
                                                            class="secondary"
                                                            on:click=move |_| ask(ask_key.clone())
                                                        >
                                                            "who is this?"
                                                        </button>
                                                        <button
                                                            class="secondary"
                                                            on:click=move |_| ignore(ignore_key.clone())
                                                        >
                                                            "ignore"
                                                        </button>
                                                    </div>
                                                    {evidence}
                                                    {candidates}
                                                </div>
                                            }
                                                .into_any()
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
                                                    "already your contact {petname:?} — {} fresh answer(s), asked {}, {} unreachable",
                                                    report.answers, report.asked, report.unreachable,
                                                ),
                                            ),
                                            (None, true) => Some(if report.asked == 0 {
                                                "no dialable contacts to ask — add a mutual contact first"
                                                    .to_string()
                                            } else if report.unreachable == report.asked {
                                                format!(
                                                    "no answers — none of the {} contact(s) asked were reachable; try again later",
                                                    report.asked,
                                                )
                                            } else {
                                                format!(
                                                    "no answers — asked {}, {} unreachable; the reachable ones don't know this key",
                                                    report.asked, report.unreachable,
                                                )
                                            }),
                                            (None, false) => None,
                                        };
                                        // Disavowal warnings (D4c): evidence
                                        // at the moment of decision.
                                        let warnings = report
                                            .disavowals
                                            .iter()
                                            .map(|line| {
                                                view! {
                                                    <div class="row">
                                                        <span class="dim">{line.clone()}</span>
                                                    </div>
                                                }
                                            })
                                            .collect::<Vec<_>>();
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
                                            {warnings}
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
                            // Concurrency markers (D4d): real data, shown —
                            // the rendered order stays the linear default.
                            let crossed = message
                                .crossed
                                .then_some(" · ⇄ crossed in flight")
                                .unwrap_or_default();
                            let merged = message
                                .merged
                                .then_some(" · ⋈ merged branches")
                                .unwrap_or_default();
                            let avatar_key = (!message.mine).then(|| message.sender_key.clone());
                            let deltas: Vec<String> = message
                                .joined
                                .iter()
                                .map(|name| format!("+ {name}"))
                                .chain(message.left.iter().map(|name| format!("− {name}")))
                                .collect();
                            view! {
                                <div class=class>
                                    {avatar_key
                                        .map(|key| {
                                            view! {
                                                {move || {
                                                    avatars
                                                        .with(|avatars| {
                                                            avatars.get(&key).filter(|url| !url.is_empty()).cloned()
                                                        })
                                                        .map(|url| view! { <img class="avatar" src=url /> })
                                                }}
                                            }
                                        })}
                                    <span class="dim">
                                        {message.sender} " · " {time_of(message.timestamp_ms)}
                                        {delivery} {crossed} {merged}
                                    </span>
                                    {(!deltas.is_empty())
                                        .then(|| view! { <div class="dim">{deltas.join(" · ")}</div> })}
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
                <div class="picks">
                    <select on:change=move |ev| add_pick.set(event_target_value(&ev))>
                        <option value="" selected disabled>
                            "add to conversation…"
                        </option>
                        {move || {
                            state
                                .get()
                                .map(|state| state.contacts)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|contact| {
                                    let value = contact.petname.clone();
                                    view! { <option value=value>{contact.petname}</option> }
                                })
                                .collect::<Vec<_>>()
                        }}
                    </select>
                    <button class="secondary" on:click=add_member>
                        "add"
                    </button>
                    {move || {
                        state
                            .get()
                            .map(|state| !state.devices.is_empty())
                            .unwrap_or(false)
                            .then(|| {
                                view! {
                                    <button class="secondary" on:click=introduce>
                                        "introduce my devices"
                                    </button>
                                }
                            })
                    }}
                </div>
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

    // Pair mode (D3e, multi-device.md §3): a scanned/pasted record is
    // previewed — name + full-key fingerprint — and NOTHING is signed
    // until the explicit confirm. `pair_scan` routes the next scan result
    // into the preview instead of add_contact.
    let pair_scan = RwSignal::new(false);
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
                        "recognized {name} as your device — scan back from it \
                         to pair both ways"
                    ));
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
                // Pair mode previews (confirm before signing); the normal
                // path adds a contact.
                Ok(scanned) if pair_scan.get_untracked() => preview(scanned.content),
                Ok(scanned) => add(scanned.content),
                // A cancelled scan also lands here — worth no red banner.
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

    // Contact avatars, lazily fetched per row (same pattern as the chat).
    let contact_avatars = RwSignal::new(HashMap::<String, String>::new());
    Effect::new(move |_| {
        for contact in state.get().map(|state| state.contacts).unwrap_or_default() {
            let key = contact.key;
            if key.is_empty()
                || contact_avatars.with_untracked(|avatars| avatars.contains_key(&key))
            {
                continue;
            }
            contact_avatars.update(|avatars| {
                avatars.insert(key.clone(), String::new());
            });
            spawn_local(async move {
                if let Ok(Some(url)) = avatar_data_url(&key).await {
                    contact_avatars.update(|avatars| {
                        avatars.insert(key, url);
                    });
                }
            });
        }
    });

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
                Ok(report) => {
                    ok(&format!(
                        "{} answer(s) (asked {}, {} unreachable) — fresh records apply automatically",
                        report.answers, report.asked, report.unreachable
                    ));
                    // A fresh answer can carry a new avatar claim (De3):
                    // re-fetch past any recorded miss.
                    if let Ok(Some(url)) = avatar_data_url(&subject).await {
                        contact_avatars.update(|avatars| {
                            avatars.insert(subject, url);
                        });
                    }
                }
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

    // D4c: vouching and repudiation. Repudiation is armed-then-confirmed
    // (two taps) — it publishes.
    let toggle_vouch = move |petname: String, vouched: bool| {
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
            }
            let args = Args { petname: &petname };
            let command = if vouched { "unvouch" } else { "vouch" };
            match invoke::invoke::<serde::de::IgnoredAny>(command, &args).await {
                Ok(_) => {
                    reload();
                    ok(if vouched {
                        "no longer vouching for them"
                    } else {
                        "vouching — shares the name you call them with anyone \
                         who asks you about them"
                    });
                }
                Err(e) => err(e),
            }
        });
    };
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
                    ok("repudiated — published in your record; contacts learn \
                        it from their next pull");
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
                    ok("un-recognized — local only, nothing published");
                }
                Err(e) => err(e),
            }
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
            // The fingerprint another device confirms against when it
            // recognizes this one (D3e, multi-device.md §3).
            {move || {
                state
                    .get()
                    .map(|state| {
                        view! {
                            <div class="dim" id="record-text">
                                {format!("this device's key: {}", state.my_key)}
                            </div>
                        }
                    })
            }}
            <h3>"my devices"</h3>
            {move || {
                let devices = state.get().map(|state| state.devices).unwrap_or_default();
                if devices.is_empty() {
                    view! {
                        <div class="dim">
                            "none recognized — pair by scanning the other device's QR"
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
                                        "un-recognize"
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
                                            }
                                        }
                                    >
                                        {move || {
                                            if armed.get().as_deref() == Some(label_key.as_str()) {
                                                "⚠ confirm repudiation"
                                            } else {
                                                "repudiate"
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
            <button on:click=move |ev| {
                pair_scan.set(true);
                scan(ev);
            }>"pair: scan a device's QR"</button>
            {move || {
                pair_preview
                    .get()
                    .map(|(payload, decoded)| {
                        let confirm_payload = payload;
                        view! {
                            <div class="wild">
                                <div class="row">
                                    <b>"recognize this device as me?"</b>
                                </div>
                                <div class="row">
                                    <b>{decoded.name.clone().unwrap_or_else(|| "(unnamed)".to_string())}</b>
                                </div>
                                // The one real risk (multi-device.md §3):
                                // compare against the key shown on the
                                // other device before signing anything.
                                <div class="dim" id="record-text">
                                    {format!("key: {}", decoded.key)}
                                </div>
                                <div class="row">
                                    <button on:click=move |_| recognize(confirm_payload.clone())>
                                        "recognize as my device"
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
            <h3>"add contact"</h3>
            <button on:click=move |ev| {
                pair_scan.set(false);
                scan(ev);
            }>"scan QR"</button>
            <textarea
                rows="2"
                placeholder="…or paste a ZINK: payload"
                prop:value=move || paste.get()
                on:input=move |ev| paste.set(event_target_value(&ev))
            />
            <button class="secondary" on:click=move |_| add(paste.get_untracked())>
                "add from pasted text"
            </button>
            <button class="secondary" on:click=move |_| preview(paste.get_untracked())>
                "pair from pasted text"
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
                        let avatar_key = contact.key.clone();
                        let vouch_name = contact.petname.clone();
                        let vouched = contact.vouched;
                        let arm_key = contact.key.clone();
                        let label_key = contact.key.clone();
                        let warnings = contact
                            .disavowals
                            .iter()
                            .map(|line| {
                                view! {
                                    <div class="row">
                                        <span class="dim">{line.clone()}</span>
                                    </div>
                                }
                            })
                            .collect::<Vec<_>>();
                        view! {
                            <div class="row">
                                {move || {
                                    contact_avatars
                                        .with(|avatars| {
                                            avatars
                                                .get(&avatar_key)
                                                .filter(|url| !url.is_empty())
                                                .cloned()
                                        })
                                        .map(|url| view! { <img class="avatar" src=url /> })
                                }}
                                <b>{contact.petname}</b>
                                <button
                                    class="secondary"
                                    on:click=move |_| refresh_contact(subject.clone())
                                >
                                    "who is?"
                                </button>
                                <button
                                    class="secondary"
                                    on:click=move |_| toggle_vouch(vouch_name.clone(), vouched)
                                >
                                    {if vouched { "withdraw vouch" } else { "vouch" }}
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
                                        }
                                    }
                                >
                                    {move || {
                                        if armed.get().as_deref() == Some(label_key.as_str()) {
                                            "⚠ confirm repudiation"
                                        } else {
                                            "repudiate"
                                        }
                                    }}
                                </button>
                            </div>
                            {warnings}
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
