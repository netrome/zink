use std::collections::{BTreeSet, HashMap};

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{
    AppState, ConversationMembers, Message, OutgoingImage, UnknownMember, WhoIsReport,
};

use crate::picker::PeoplePicker;
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
    drafts: RwSignal<HashMap<String, String>>,
    reload_messages: impl Fn(String) + Copy + Send + 'static,
    open_person: impl Fn(String) + Copy + Send + 'static,
    back: impl Fn() + Copy + Send + 'static,
    ok: impl Fn(&str) + Copy + Send + 'static,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
    let conversation = StoredValue::new(id);
    // The half-typed reply survives leaving the chat (S4, U8): the text
    // lives in the App-owned drafts map, keyed by conversation.
    let draft = RwSignal::new(drafts.with_untracked(|drafts| {
        drafts
            .get(&conversation.get_value())
            .cloned()
            .unwrap_or_default()
    }));
    Effect::new(move |_| {
        let text = draft.get();
        drafts.update(|drafts| {
            drafts.insert(conversation.get_value(), text);
        });
    });
    let attachment = RwSignal::new(None::<(OutgoingImage, String)>);
    // hash → data URL; present-but-empty marks an in-flight fetch.
    let thumbs = RwSignal::new(HashMap::<String, String>::new());
    // Full-res overlay: `Some("")` = loading, `Some(url)` = showing.
    let viewer = RwSignal::new(None::<String>);
    // Optional "show concurrency" affordance (U8): the causal-DAG cues
    // (crossed / merged, tenet 7) are advanced honesty data — hidden by
    // default, revealed on demand.
    let show_concurrency = RwSignal::new(false);
    // The stuck cue's tap target (R3): a 1:1 chat is labelled by the
    // contact's petname, so that's the page with the repair actions. A
    // group label matches no contact and the cue stays plain text.
    let chat_label = StoredValue::new(label.clone());

    // The live header label + members panel (S2): membership is heads-based
    // (groups.md §2) and moves with the DAG, so both re-derive on every
    // messages change — the open-time `label` is just the first paint.
    let title = RwSignal::new(label);
    let members = RwSignal::new(None::<ConversationMembers>);
    let members_open = RwSignal::new(false);
    let adding = RwSignal::new(false);
    let picks = RwSignal::new(BTreeSet::<String>::new());
    let contacts =
        Signal::derive(move || state.get().map(|state| state.contacts).unwrap_or_default());
    let member_names = Signal::derive(move || {
        members
            .get()
            .map(|members| members.petnames)
            .unwrap_or_default()
    });
    let load_members = move || {
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: &'a str,
            }
            let args = Args { conversation: &id };
            if let Ok(loaded) =
                invoke::invoke::<ConversationMembers>("conversation_members", &args).await
            {
                title.set(loaded.label.clone());
                members.set(Some(loaded));
            }
        });
    };

    // Scroll pinning (S3): the list opens at the bottom and follows
    // arrivals while the reader is there; a reader who scrolled up is
    // never yanked. `pinned` is where the reader was *before* an update,
    // kept current by the scroll handler.
    let list_ref = NodeRef::<leptos::html::Div>::new();
    let pinned = StoredValue::new(true);
    let on_scroll = move |_| {
        if let Some(list) = list_ref.get_untracked() {
            let slack = list.scroll_height() - list.scroll_top() - list.client_height();
            pinned.set_value(slack < 60);
        }
    };
    Effect::new(move |_| {
        messages.track();
        thumbs.track(); // a late thumbnail grows the list under the reader
        // After the frame paints the new rows — the effect can run first.
        request_animation_frame(move || {
            if !pinned.get_value() {
                return;
            }
            if let Some(list) = list_ref.get_untracked() {
                list.set_scroll_top(list.scroll_height());
            }
        });
    });

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
        load_members();
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

    // Add people to this conversation (D2c): one message with the grown
    // recipient set is the whole mechanism — the signed recipients list
    // announces the membership change, however many joined at once.
    let add_inflight = RwSignal::new(false);
    let add_members = move |_| {
        if add_inflight.get_untracked() {
            return;
        }
        let names: Vec<String> = picks.get_untracked().into_iter().collect();
        if names.is_empty() {
            return;
        }
        add_inflight.set(true);
        let id = conversation.get_value();
        spawn_local(async move {
            #[derive(Serialize)]
            struct Args<'a> {
                conversation: Option<&'a str>,
                add: Option<Vec<String>>,
                text: &'a str,
            }
            let listed = names.join(", ");
            let args = Args {
                conversation: Some(&id),
                add: Some(names),
                text: "",
            };
            let result = invoke::invoke::<String>("send_message", &args).await;
            add_inflight.set(false);
            match result {
                Ok(_) => {
                    picks.update(|picks| picks.clear());
                    adding.set(false);
                    ok(&format!("added {listed} to the conversation"));
                    reload_messages(id); // membership moved → members re-derive
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

    // In flight → the button disables and re-taps drop (S4): staging is
    // fast but not instant, and a double-tap must not send twice.
    let sending = RwSignal::new(false);
    let send = move || {
        if sending.get_untracked() {
            return;
        }
        let body = draft.get_untracked();
        let image = attachment.get_untracked().map(|(image, _)| image);
        if body.trim().is_empty() && image.is_none() {
            return;
        }
        sending.set(true);
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
            let result = invoke::invoke::<String>("send_message", &args).await;
            sending.set(false);
            match result {
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
            <div class="picks">
                <button class="secondary" on:click=move |_| back()>
                    "‹ chats"
                </button>
            </div>
            // The tappable header (S2): title re-derived from membership,
            // the panel one tap away.
            <h3
                class="tappable"
                on:click=move |_| members_open.update(|open| *open = !*open)
            >
                {move || title.get()}
                " "
                <span class="dim">
                    {move || if members_open.get() { "▴" } else { "▾" }}
                </span>
            </h3>
            {move || {
                members_open
                    .get()
                    .then(|| {
                        view! {
                            <div class="panel">
                                <div class="dim">"members"</div>
                                {move || {
                                    members
                                        .get()
                                        .map(|members| {
                                            members
                                                .members
                                                .into_iter()
                                                .map(|member| {
                                                    view! {
                                                        <div class="row">
                                                            <b>{member}</b>
                                                        </div>
                                                    }
                                                })
                                                .collect::<Vec<_>>()
                                        })
                                }}
                                {move || {
                                    if adding.get() {
                                        view! {
                                            <PeoplePicker
                                                contacts=contacts
                                                selected=picks
                                                exclude=member_names
                                            />
                                            <div class="picks">
                                                <button
                                                    disabled=move || {
                                                        add_inflight.get()
                                                            || picks.with(|picks| picks.is_empty())
                                                    }
                                                    on:click=add_members
                                                >
                                                    "add"
                                                </button>
                                                <button
                                                    class="secondary"
                                                    on:click=move |_| {
                                                        adding.set(false);
                                                        picks.update(|picks| picks.clear());
                                                    }
                                                >
                                                    "cancel"
                                                </button>
                                            </div>
                                        }
                                            .into_any()
                                    } else {
                                        view! {
                                            <button
                                                class="secondary"
                                                on:click=move |_| adding.set(true)
                                            >
                                                "+ add people"
                                            </button>
                                        }
                                            .into_any()
                                    }
                                }}
                                // Advanced, rare affordances — one tap away
                                // instead of always-on (S2).
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
                        }
                    })
            }}
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
            <div class="messages" node_ref=list_ref on:scroll=on_scroll>
                {move || {
                    // Day separators (S3): one dim line whenever the local
                    // calendar day changes between consecutive messages.
                    let mut last_day = None;
                    messages
                        .get()
                        .into_iter()
                        .map(move |message| {
                            let day = day_of(message.timestamp_ms);
                            let separator = (last_day != Some(day))
                                .then(|| day_label(message.timestamp_ms));
                            last_day = Some(day);
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
                            // Honest send states (R3): our-deposit facts,
                            // never claims about their receipt. "sending…"
                            // only while young; a long-owed debt says so.
                            let pending = if message.undelivered {
                                " · undelivered — no relay took it in 30 days"
                            } else if message.stuck {
                                "" // its own cue below, with the tap-through
                            } else if message.pending {
                                " · sending…"
                            } else {
                                ""
                            };
                            let stuck_person = (message.stuck && !message.undelivered).then(|| {
                                let target = chat_label.get_value();
                                state
                                    .with_untracked(|state| {
                                        state.as_ref().is_some_and(|state| {
                                            state
                                                .contacts
                                                .iter()
                                                .any(|contact| contact.petname == target)
                                        })
                                    })
                                    .then_some(target)
                            });
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
                                {separator
                                    .map(|label| view! { <div class="day">{label}</div> })}
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
                                        {stuck_person.map(|target| match target {
                                            Some(petname) => view! {
                                                <span on:click=move |_| open_person(
                                                    petname.clone(),
                                                )>
                                                    " · ⚠ can't reach their relay — tap to check their page"
                                                </span>
                                            }
                                                .into_any(),
                                            None => view! {
                                                <span>" · ⚠ can't reach their relay — still trying"</span>
                                            }
                                                .into_any(),
                                        })}
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
                <Composer
                    draft=draft
                    attachment=attachment
                    send=send
                    sending=sending
                    err=err
                />
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

/// The message composer — attachment preview + picker, text box, send.
/// Shared by the live chat and the draft chat (project 6 S1): one composer,
/// so a first message can do everything a reply can.
#[component]
pub(crate) fn Composer(
    draft: RwSignal<String>,
    attachment: RwSignal<Option<(OutgoingImage, String)>>,
    send: impl Fn() + Copy + Send + 'static,
    /// True while the caller's send is in flight — disables the button
    /// (S4); the caller's own guard covers the Enter path.
    sending: RwSignal<bool>,
    err: impl Fn(String) + Copy + Send + 'static,
) -> impl IntoView {
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
    // Enter sends where a hardware keyboard is likely (fine pointer); on
    // touch, Enter stays a newline and the button is the send (S3).
    let enter_sends = !touch_device();
    let keydown = move |ev: leptos::ev::KeyboardEvent| {
        if enter_sends && ev.key() == "Enter" && !ev.shift_key() {
            ev.prevent_default();
            send();
        }
    };
    view! {
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
        <div class="composer-row">
            // A label wrapping a hidden input: the file dialog behind a
            // tap-sized 📎 instead of the browser's raw file widget.
            <label class="attach">
                "📎"
                <input type="file" accept="image/*" hidden on:change=attach />
            </label>
            <textarea
                rows="2"
                placeholder="message"
                prop:value=move || draft.get()
                on:input=move |ev| draft.set(event_target_value(&ev))
                on:keydown=keydown
            />
            <button disabled=move || sending.get() on:click=move |_| send()>
                "send"
            </button>
        </div>
    }
}

/// Coarse-pointer (touch) detection — the Enter-to-send policy's input.
fn touch_device() -> bool {
    window()
        .match_media("(pointer: coarse)")
        .ok()
        .flatten()
        .map(|query| query.matches())
        .unwrap_or(false)
}

/// hh:mm from the sender's wall-clock hint — display only, like the hint.
fn time_of(timestamp_ms: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    format!("{:02}:{:02}", date.get_hours(), date.get_minutes())
}

/// Local calendar day of a wall-clock hint — the separator grouping key.
fn day_of(timestamp_ms: u64) -> (u32, u32, u32) {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    (date.get_full_year(), date.get_month(), date.get_date())
}

/// "today" / "yesterday" / "aug 12" (+ year when it differs) — the day
/// separator text. Display only, from the sender's hint, like `time_of`.
fn day_label(timestamp_ms: u64) -> String {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let now = js_sys::Date::now();
    let day = day_of(timestamp_ms);
    if day == day_of(now as u64) {
        return "today".to_string();
    }
    if day == day_of((now - 86_400_000.0) as u64) {
        return "yesterday".to_string();
    }
    // `% 12` keeps a hostile timestamp (Invalid Date → 0-ish fields) from
    // panicking the render — garbage in, a harmless wrong label out.
    let month = MONTHS[day.1 as usize % 12];
    if day.0 == day_of(now as u64).0 {
        format!("{month} {}", day.2)
    } else {
        format!("{month} {} {}", day.2, day.0)
    }
}
