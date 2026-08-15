use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{AppState, Message, OutgoingImage, UnknownMember, WhoIsReport};

use crate::{avatar_data_url, image, invoke};

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
pub(crate) fn ChatView(
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
    // hash → data URL; present-but-empty marks an in-flight fetch.
    let thumbs = RwSignal::new(HashMap::<String, String>::new());
    // Full-res overlay: `Some("")` = loading, `Some(url)` = showing.
    let viewer = RwSignal::new(None::<String>);
    // Optional "show concurrency" affordance (U8): the causal-DAG cues
    // (crossed / merged, tenet 7) are advanced honesty data — hidden by
    // default, revealed on demand.
    let show_concurrency = RwSignal::new(false);
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
            <div class="picks">
                <button
                    class="secondary"
                    on:click=move |_| show_concurrency.update(|on| *on = !*on)
                >
                    {move || {
                        if show_concurrency.get() {
                            "hide when messages crossed"
                        } else {
                            "show when messages crossed"
                        }
                    }}
                </button>
            </div>
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
                            let pending = if message.pending { " · sending…" } else { "" };
                            // Delivery confirmation (De7): their device said
                            // it stored this. **Positive-only** (tenet 7) —
                            // nothing is rendered when empty, because an
                            // absent confirmation is not a failed delivery
                            // and must never be dressed as one. No greyed
                            // tick; "sending…" stays the only negative cue.
                            let confirmed = if message.confirmed.is_empty() {
                                String::new()
                            } else {
                                format!(" · ✓ delivered to {}", message.confirmed.join(", "))
                            };
                            // Concurrency cues (D4d, tenet 7): real causal
                            // data, but advanced — shown only when the reader
                            // opts in via the header toggle.
                            let concurrency = if show_concurrency.get() {
                                let mut cues = String::new();
                                if message.crossed {
                                    cues.push_str(" · ⇄ crossed in flight");
                                }
                                if message.merged {
                                    cues.push_str(" · ⋈ merged branches");
                                }
                                cues
                            } else {
                                String::new()
                            };
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
                                        {pending} {confirmed} {concurrency}
                                    </span>
                                    {(!deltas.is_empty())
                                        .then(|| view! { <div class="dim">{deltas.join(" · ")}</div> })}
                                    {images}
                                    {body.map(|text| view! { <div>{text}</div> })}
                                    {unopenable
                                        .then(|| view! { <div class="dim">"🔒 can't read this yet"</div> })}
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

/// hh:mm from the sender's wall-clock hint — display only, like the hint.
fn time_of(timestamp_ms: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}
