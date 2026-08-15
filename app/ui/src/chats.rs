use std::collections::BTreeSet;

use leptos::prelude::*;
use zink_app_dto::{AppState, Conversation, Inbox};

use crate::picker::PeoplePicker;

/// Conversation list. Starting a new chat is a deliberate "+" action: pick
/// people in the shared picker, then land in a draft chat whose first send
/// is the genesis (project 6 §7 — a new chat is always a new conversation).
/// No permanent form, no refresh button — live delivery + the backstop poll
/// keep the list current.
#[component]
pub(crate) fn ChatsView(
    conversations: RwSignal<Inbox>,
    state: RwSignal<Option<AppState>>,
    open_chat: impl Fn(String, String) + Copy + Send + 'static,
    start_draft: impl Fn(Vec<String>) + Copy + Send + 'static,
) -> impl IntoView {
    let selected = RwSignal::new(BTreeSet::<String>::new());
    // Whether the "new chat" picker is open (vs the plain list).
    let picking = RwSignal::new(false);
    let contacts =
        Signal::derive(move || state.get().map(|state| state.contacts).unwrap_or_default());

    let close_picker = move || {
        picking.set(false);
        selected.update(|selected| selected.clear());
    };
    let next = move |_| {
        let picks: Vec<String> = selected.get_untracked().into_iter().collect();
        if picks.is_empty() {
            return;
        }
        close_picker();
        start_draft(picks);
    };

    view! {
        <main>
            {move || {
                if picking.get() {
                    // The "new chat" picker: pick people, then the draft chat.
                    view! {
                        <h3>"new chat"</h3>
                        <PeoplePicker contacts=contacts selected=selected />
                        <button
                            disabled=move || selected.with(|selected| selected.is_empty())
                            on:click=next
                        >
                            "next"
                        </button>
                        <button class="secondary" on:click=move |_| close_picker()>
                            "cancel"
                        </button>
                    }
                        .into_any()
                } else {
                    // The plain list + the deliberate "+" to start a chat.
                    view! {
                        <button on:click=move |_| picking.set(true)>"+ new chat"</button>
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
