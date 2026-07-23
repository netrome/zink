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
    AppState, Conversation, Message, OutgoingImage, PersonDetail, QrPayload, RecordPreview,
    UnknownMember, WhoIsReport,
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
    People,
    Person { petname: String },
    Me,
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
                    // First run: no profile yet → straight to the Me setup view.
                    if loaded.name.is_none() {
                        view.set(View::Me);
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
    let open_person = move |petname: String| view.set(View::Person { petname });

    view! {
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
            View::People => view! {
                <PeopleView
                    state=state
                    reload=load_state
                    open_person=open_person
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Person { petname } => view! {
                <PersonView
                    petname=petname
                    reload=load_state
                    back=move || view.set(View::People)
                    ok=ok
                    err=err
                />
            }
            .into_any(),
            View::Me => view! {
                <MeView state=state reload=load_state ok=ok err=err />
            }
            .into_any(),
        }}
        <nav class="tabbar">
            <button
                class:active=move || matches!(view.get(), View::Chats | View::Chat { .. })
                on:click=move |_| {
                    load_conversations();
                    view.set(View::Chats);
                }
            >
                "Chats"
            </button>
            <button
                class:active=move || matches!(view.get(), View::People | View::Person { .. })
                on:click=move |_| {
                    load_state();
                    view.set(View::People);
                }
            >
                "People"
            </button>
            <button
                class:active=move || view.get() == View::Me
                on:click=move |_| {
                    load_state();
                    view.set(View::Me);
                }
            >
                "Me"
            </button>
        </nav>
    }
}

/// Conversation list. Starting a new chat is a deliberate "+" action (pick one
/// or more people, write the first message); later messages happen inside the
/// chat view. No permanent form, no refresh button — live delivery + the
/// backstop poll keep the list current.
#[component]
fn ChatsView(
    conversations: RwSignal<Vec<Conversation>>,
    state: RwSignal<Option<AppState>>,
    open_chat: impl Fn(String, String) + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let selected = RwSignal::new(std::collections::BTreeSet::<String>::new());
    let text = RwSignal::new(String::new());
    // Whether the "new chat" composer is open (vs the plain list).
    let composing = RwSignal::new(false);
    let contacts = move || state.get().map(|state| state.contacts).unwrap_or_default();

    let close_compose = move || {
        composing.set(false);
        selected.update(|selected| selected.clear());
        text.set(String::new());
    };

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
                    composing.set(false);
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
                if composing.get() {
                    // The "new chat" composer: pick people, write the first message.
                    view! {
                        <h3>"new chat"</h3>
                        <div class="picks">
                            <span class="dim">"with:"</span>
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
                        <button class="secondary" on:click=move |_| close_compose()>
                            "cancel"
                        </button>
                    }
                        .into_any()
                } else {
                    // The plain list + the deliberate "+" to start a chat.
                    view! {
                        <button on:click=move |_| composing.set(true)>"+ new chat"</button>
                        {move || {
                            let list = conversations.get();
                            if list.is_empty() {
                                view! {
                                    <div class="dim">
                                        "no conversations yet — tap + new chat to start one"
                                    </div>
                                }
                                    .into_any()
                            } else {
                                list.into_iter()
                                    .map(|conversation| {
                                        let (id, label) = (
                                            conversation.id.clone(),
                                            conversation.label.clone(),
                                        );
                                        view! {
                                            <div
                                                class="row"
                                                on:click=move |_| open_chat(id.clone(), label.clone())
                                            >
                                                <b>{conversation.label}</b>
                                                <span class="dim">
                                                    {format!(
                                                        "{} message(s)",
                                                        conversation.message_count,
                                                    )}
                                                </span>
                                            </div>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .into_any()
                            }
                        }}
                    }
                        .into_any()
                }
            }}
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

/// "Me" — your own identity: profile (name, home relay, avatar, QR), your
/// recognized devices, and device pairing. The C2/D3e flows, unchanged — the
/// U2 screen split just homes them here. Its scan always pairs (previews a
/// record before signing); the contact-adding scan lives in `PeopleView`.
#[component]
fn MeView(
    state: RwSignal<Option<AppState>>,
    reload: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let name = RwSignal::new(String::new());
    // The home-relay set (U5 multi-relay), edited locally and persisted on
    // save; `new_relay` is the add field.
    let relays = RwSignal::new(Vec::<String>::new());
    let new_relay = RwSignal::new(String::new());
    // Pairing paste buffer (a device record to recognize).
    let paste = RwSignal::new(String::new());

    // Prefill the form from the loaded profile (once per state change).
    Effect::new(move |_| {
        if let Some(state) = state.get() {
            if let Some(profile_name) = state.name {
                name.set(profile_name);
            }
            relays.set(state.relays);
        }
    });

    let add_relay = move |_| {
        let value = new_relay.get_untracked().trim().to_string();
        if value.is_empty() {
            return;
        }
        relays.update(|list| {
            if !list.contains(&value) {
                list.push(value);
            }
        });
        new_relay.set(String::new());
    };
    let remove_relay = move |value: String| {
        relays.update(|list| list.retain(|relay| relay != &value));
    };

    let save = move |_| {
        let (name, relays) = (name.get_untracked(), relays.get_untracked());
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                name: &'a str,
                relays: &'a [String],
            }
            let args = Args {
                name: &name,
                relays: &relays,
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
                        "recognized {name} as your device — scan back from it \
                         to pair both ways"
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
                    ok("un-recognized — local only, nothing published");
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
                    ok("repudiated — published in your record; contacts learn \
                        it from their next pull");
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
                // Always pair mode here — preview before signing.
                Ok(scanned) => preview(scanned.content),
                // A cancelled scan also lands here — worth no red banner.
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
            <div class="dim">
                "other devices you recognize as also you. recognition is one-way — \
                 recognize this device from each of them too, so both sides agree"
            </div>
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
            <button on:click=scan>"pair: scan a device's QR"</button>
            <textarea
                rows="2"
                placeholder="…or paste a ZINK: payload to pair"
                prop:value=move || paste.get()
                on:input=move |ev| paste.set(event_target_value(&ev))
            />
            <button class="secondary" on:click=move |_| preview(paste.get_untracked())>
                "pair from pasted text"
            </button>
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

/// "People" — your contacts. The list is a plain, tappable list (a row opens
/// the person-detail lens, U4); adding a contact (scan / paste) hides behind a
/// "+" so the list stays clean, and the per-contact trust actions moved onto
/// the detail screen. Its scan always adds a contact; the device-pairing scan
/// lives in `MeView`.
#[component]
fn PeopleView(
    state: RwSignal<Option<AppState>>,
    reload: impl Fn() + Copy + Send + 'static,
    open_person: impl Fn(String) + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let paste = RwSignal::new(String::new());
    // Whether the add-contact form is open (vs the plain list).
    let adding = RwSignal::new(false);
    // Optional petname to set at add time (my lens); empty → their
    // self-claimed name. Applies to both the scan and paste paths.
    let new_name = RwSignal::new(String::new());

    let add = move |payload: String| {
        let petname = new_name.get_untracked();
        let petname = (!petname.trim().is_empty()).then_some(petname);
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                payload: &'a str,
                petname: Option<&'a str>,
            }
            let args = Args {
                payload: &payload,
                petname: petname.as_deref(),
            };
            match invoke::invoke::<String>("add_contact", &args).await {
                Ok(petname) => {
                    paste.set(String::new());
                    new_name.set(String::new());
                    adding.set(false);
                    reload();
                    ok(&format!("added {petname}"));
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

    // Scanning state drives the cancel overlay AND page transparency (see the
    // note in `MeView`). This scan always adds a contact.
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
    let cancel_scan = move |_| {
        spawn_local(async move {
            let _ = invoke::invoke::<serde::de::IgnoredAny>(
                "plugin:barcode-scanner|cancel",
                &NoArgs {},
            )
            .await;
        });
    };

    view! {
        <main>
            {move || {
                if adding.get() {
                    // Add-contact composer (scan / paste), off the "+".
                    view! {
                        <h3>"add contact"</h3>
                        <input
                            placeholder="your name for them (optional)"
                            prop:value=move || new_name.get()
                            on:input=move |ev| new_name.set(event_target_value(&ev))
                        />
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
                        <button
                            class="secondary"
                            on:click=move |_| {
                                adding.set(false);
                                paste.set(String::new());
                                new_name.set(String::new());
                            }
                        >
                            "cancel"
                        </button>
                    }
                        .into_any()
                } else {
                    // The plain list + the deliberate "+" to add a contact.
                    view! {
                        <button on:click=move |_| adding.set(true)>"+ add contact"</button>
                        {move || {
                            let contacts = state
                                .get()
                                .map(|state| state.contacts)
                                .unwrap_or_default();
                            if contacts.is_empty() {
                                view! {
                                    <div class="dim">
                                        "no contacts yet — tap + add contact to scan or paste a code"
                                    </div>
                                }
                                    .into_any()
                            } else {
                                contacts
                                    .into_iter()
                                    .map(|contact| {
                                        let petname = contact.petname.clone();
                                        let avatar_key = contact.key.clone();
                                        let has_warning = !contact.disavowals.is_empty();
                                        view! {
                                            <div
                                                class="row"
                                                on:click=move |_| open_person(petname.clone())
                                            >
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
                                                {has_warning
                                                    .then(|| view! { <span class="dim">"⚠"</span> })}
                                            </div>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .into_any()
                            }
                        }}
                    }
                        .into_any()
                }
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

/// The person-detail lens (U4, ui-facelift.md §4): one contact rendered as
/// three separated belief layers — my lens (petname + avatar + the keys I've
/// grouped), their self-claim, and my friends' lens (vouched names only,
/// never a friend's private petname). Trust actions (vouch / repudiate) and a
/// who-is freshness pull live here, in context. All read-time; nothing here
/// assumes one key per person.
#[component]
fn PersonView(
    petname: String,
    reload: impl Fn() + Copy + Send + 'static,
    back: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let petname = StoredValue::new(petname);
    let detail = RwSignal::new(None::<PersonDetail>);
    let avatar = RwSignal::new(None::<String>);
    // The editable petname (my lens) — prefilled from the loaded detail.
    let rename_to = RwSignal::new(String::new());
    // Repudiation is armed-then-confirmed (two taps) — it publishes.
    let armed = RwSignal::new(false);

    let load_detail = move || {
        let name = petname.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
            }
            let args = Args { petname: &name };
            match invoke::invoke::<PersonDetail>("person_detail", &args).await {
                Ok(loaded) => {
                    let key = loaded.avatar_key.clone();
                    rename_to.set(loaded.petname.clone());
                    detail.set(Some(loaded));
                    if !key.is_empty()
                        && let Ok(url) = avatar_data_url(&key).await
                    {
                        avatar.set(url);
                    }
                }
                Err(e) => err(e),
            }
        });
    };

    // Rename — set my petname for them (my lens). Local only; sharing that
    // name with friends is the separate `vouch` below.
    let do_rename = move || {
        let current = petname.get_value();
        let new = rename_to.get_untracked();
        if new.trim().is_empty() || new == current {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                current: &'a str,
                new: &'a str,
            }
            let args = Args {
                current: &current,
                new: &new,
            };
            match invoke::invoke::<serde::de::IgnoredAny>("rename_contact", &args).await {
                Ok(_) => {
                    // The view now tracks the new name (person_detail is
                    // keyed by petname).
                    petname.set_value(new.clone());
                    reload();
                    load_detail();
                    ok(&format!("renamed to {new}"));
                }
                Err(e) => err(e),
            }
        });
    };
    load_detail();

    let toggle_vouch = move || {
        let Some(current) = detail.get_untracked() else {
            return;
        };
        let (name, vouched) = (current.petname, current.vouched);
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                petname: &'a str,
            }
            let args = Args { petname: &name };
            let command = if vouched { "unvouch" } else { "vouch" };
            match invoke::invoke::<serde::de::IgnoredAny>(command, &args).await {
                Ok(_) => {
                    reload();
                    load_detail();
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

    // Re-ask the network (D1c): fresh answers land in the learned store and
    // sharpen resolution on their own — reload the detail to show them.
    let refresh = move || {
        let Some(subject) = detail.get_untracked().map(|person| person.avatar_key) else {
            return;
        };
        if subject.is_empty() {
            return;
        }
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
                    load_detail();
                }
                Err(e) => err(e),
            }
        });
    };

    let repudiate = move || {
        let Some(key) = detail.get_untracked().map(|person| person.avatar_key) else {
            return;
        };
        if key.is_empty() {
            return;
        }
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                key: &'a str,
            }
            let args = Args { key: &key };
            match invoke::invoke::<serde::de::IgnoredAny>("repudiate_key", &args).await {
                Ok(_) => {
                    armed.set(false);
                    reload();
                    load_detail();
                    ok("repudiated — published in your record; contacts learn \
                        it from their next pull");
                }
                Err(e) => err(e),
            }
        });
    };

    view! {
        <main>
            <button class="secondary" on:click=move |_| back()>
                "‹ people"
            </button>
            {move || {
                detail
                    .get()
                    .map(|person| {
                        let friends = person.friends.clone();
                        let keys = person.keys.clone();
                        let disavowals = person.disavowals.clone();
                        let has_key = !person.avatar_key.is_empty();
                        view! {
                            // My lens: avatar + the petname I call them.
                            <div class="pending">
                                {move || {
                                    avatar
                                        .get()
                                        .map(|url| view! { <img class="avatar avatar-lg" src=url /> })
                                }}
                                <h3>{person.petname.clone()}</h3>
                            </div>
                            // My lens: the name I call them (editable, local).
                            <div class="dim">"your name for them"</div>
                            <input
                                prop:value=move || rename_to.get()
                                on:input=move |ev| rename_to.set(event_target_value(&ev))
                            />
                            <button class="secondary" on:click=move |_| do_rename()>
                                "rename"
                            </button>
                            // Disavowal warnings — context at the moment of decision.
                            {disavowals
                                .into_iter()
                                .map(|line| {
                                    view! {
                                        <div class="row">
                                            <span class="dim">{line}</span>
                                        </div>
                                    }
                                })
                                .collect::<Vec<_>>()}
                            // Their self-claim.
                            <div class="dim">"they call themselves"</div>
                            <div class="row">
                                <b>{person.self_name.clone().unwrap_or_else(|| "—".to_string())}</b>
                            </div>
                            // Friends' lens: vouched names only — never a
                            // friend's private petname (who-is-this.md §6).
                            <div class="dim">"how your friends see them"</div>
                            {if friends.is_empty() {
                                view! {
                                    <div class="row">
                                        <span class="dim">"no friend has vouched a name for them yet"</span>
                                    </div>
                                }
                                    .into_any()
                            } else {
                                friends
                                    .into_iter()
                                    .map(|friend| {
                                        view! {
                                            <div class="row">
                                                <b>{friend.name}</b>
                                                <span class="dim">
                                                    {format!("vouched by {}", friend.vouched_by.join(", "))}
                                                </span>
                                            </div>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .into_any()
                            }}
                            // My grouping: the keys clustered as this person.
                            <div class="dim">
                                {format!("{} key(s) you've grouped as this person", keys.len())}
                            </div>
                            {keys
                                .into_iter()
                                .map(|key| {
                                    let short = key.chars().take(16).collect::<String>();
                                    view! {
                                        <div class="dim" id="record-text">
                                            {format!("{short}…")}
                                        </div>
                                    }
                                })
                                .collect::<Vec<_>>()}
                            // Actions, in context. Vouching *is* sharing your
                            // name for them — say so plainly (the friends'
                            // lens above is the other side of this act).
                            <button on:click=move |_| toggle_vouch()>
                                {if person.vouched {
                                    "stop sharing your name for them"
                                } else {
                                    "share the name you call them"
                                }}
                            </button>
                            <div class="dim">
                                "lets friends who ask you about them see this name"
                            </div>
                            <button class="secondary" on:click=move |_| refresh()>
                                "refresh — who is this?"
                            </button>
                            {has_key
                                .then(|| {
                                    view! {
                                        <button
                                            class="danger"
                                            on:click=move |_| {
                                                if armed.get_untracked() {
                                                    repudiate();
                                                } else {
                                                    armed.set(true);
                                                }
                                            }
                                        >
                                            {move || {
                                                if armed.get() {
                                                    "⚠ confirm — this key isn't them"
                                                } else {
                                                    "this key isn't them anymore"
                                                }
                                            }}
                                        </button>
                                    }
                                })}
                        }
                            .into_any()
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
