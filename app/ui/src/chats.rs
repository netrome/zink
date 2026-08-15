use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use zink_app_dto::{AppState, Conversation, Inbox};

use crate::invoke;

/// Conversation list. Starting a new chat is a deliberate "+" action (pick one
/// or more people, write the first message); later messages happen inside the
/// chat view. No permanent form, no refresh button — live delivery + the
/// backstop poll keep the list current.
#[component]
pub(crate) fn ChatsView(
    conversations: RwSignal<Inbox>,
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
                    // The message is stored, not yet delivered — the row's
                    // own "sending…" marker is the truth from here.
                    ok("sending…");
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
                            let inbox = conversations.get();
                            let row = |conversation: Conversation| {
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
                                            {format!("{} message(s)", conversation.message_count)}
                                        </span>
                                    </div>
                                }
                            };
                            if inbox.conversations.is_empty() && inbox.requests.is_empty() {
                                view! {
                                    <div class="dim">
                                        "no conversations yet — tap + new chat to start one"
                                    </div>
                                }
                                    .into_any()
                            } else {
                                let main = inbox
                                    .conversations
                                    .into_iter()
                                    .map(row)
                                    .collect::<Vec<_>>();
                                // Message requests (groups.md §6): nobody you
                                // know has written here yet. Kept out of the
                                // main list, never hidden — and phrased as
                                // "not yet", because one message from a
                                // contact promotes the whole conversation.
                                let (requests, dropped) = (inbox.requests, inbox.dropped);
                                let pending = (!requests.is_empty())
                                    .then(|| {
                                        let rows = requests
                                            .into_iter()
                                            .map(row)
                                            .collect::<Vec<_>>();
                                        view! {
                                            <div class="dim section">
                                                "message requests — nobody you know has written here yet"
                                            </div>
                                            {rows}
                                            {(dropped > 0)
                                                .then(|| {
                                                    view! {
                                                        <div class="dim">
                                                            {format!("+ {dropped} more not shown")}
                                                        </div>
                                                    }
                                                })}
                                        }
                                    });
                                view! {
                                    {main}
                                    {pending}
                                }
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
